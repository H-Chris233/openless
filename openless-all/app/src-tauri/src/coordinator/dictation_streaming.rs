use std::sync::Arc;

use crate::correction::apply_correction_rules;

use super::*;

const STREAMING_INSERT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(12);

/// 跑流式润色路径（opt-in，跨平台）。
///
/// 平台差异：
/// - **macOS**：`switch_to_ascii` 切到 ABC 输入源（规避 CJK / 日文 IME 拦截 Unicode 事件），
///   session 结束 `restore_input_source` 切回。`type_unicode_chunk` 走 CGEvent FFI。
/// - **Windows**：`switch_to_ascii` 是 no-op（SendInput Unicode 绕过 TSF）；
///   `type_unicode_chunk` 走 `SendInput(KEYEVENTF_UNICODE)`。
/// - **Linux（实验）**：`switch_to_ascii` 是 no-op；`type_unicode_chunk` 走 enigo
///   `Keyboard::text`。X11 / XTest 稳定。
///
/// 通用流程：
/// 1. `switch_to_ascii`（macOS）/ no-op（其他）；失败则降级回一次性 `polish_or_passthrough`。
/// 2. 起一个 `spawn_blocking` 后台任务，从 mpsc 收 SSE delta，按 12ms flush window
///    合并后调 `type_unicode_chunk` 模拟键盘事件落到光标处。串行有序，无竞态。
/// 3. 调 `polish_or_passthrough_streaming`，`on_delta` 把 chunk 塞进 mpsc。
/// 4. 流结束 / 失败 / 取消 → drop mpsc 发送端 → typer 任务 drain 完剩余 delta 退出 →
///    `restore_input_source` 恢复用户原输入源（macOS 才有意义，其他平台 no-op）。
/// 5. 返回 `(polished, polish_error, already_streamed)`：
///    - 成功：`(text, None, true)` — 字符已经在屏幕上，调用方应当跳过 `inserter.insert`
///    - 失败：`(raw_text, Some(reason), false)` — 流式过程出错，调用方走 raw 一次性兜底
///    - 不支持：`run_streaming_polish` 内部直接调 `polish_or_passthrough` 透明降级
///
/// **不在流式路径里做**：`apply_chinese_script_preference` / `apply_correction_rules`
/// 这两步在 v1 跳过 —— 字符已经一边流一边落出去了，不好回退。需要的话只能关 toggle 走
/// 一次性路径。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_streaming_polish(
    inner: &Arc<Inner>,
    raw: &RawTranscript,
    mode: PolishMode,
    hotwords: &[String],
    style_system_prompt: &str,
    working_languages: &[String],
    chinese_script_preference: crate::types::ChineseScriptPreference,
    output_language_preference: crate::types::OutputLanguagePreference,
    llm_thinking_enabled: bool,
    front_app: Option<&str>,
    prior_turns: &[(String, String)],
) -> (String, Option<String>, bool) {
    log::info!(
        "[coord] streaming_insert path ENTER (raw_chars={})",
        raw.text.chars().count()
    );

    let app = inner.app.lock().clone();
    let Some(app) = app else {
        log::warn!("[coord] streaming_insert: no AppHandle in Inner; fall back to one-shot");
        let (p, e) = polish_or_passthrough(
            raw,
            mode,
            hotwords,
            style_system_prompt,
            working_languages,
            chinese_script_preference,
            output_language_preference,
            llm_thinking_enabled,
            front_app,
            prior_turns,
        )
        .await;
        return (p, e, false);
    };

    // 1. 切到 ABC 输入源。失败则降级 —— 流式路径上 CJK IME 拦截不是可恢复错误。
    log::info!("[coord] streaming_insert: switching input source to ABC");
    let prev_ime = match crate::unicode_keystroke::switch_to_ascii(&app).await {
        Ok(prev) => {
            log::info!(
                "[coord] streaming_insert: switched to ABC (had_previous={})",
                prev.is_some()
            );
            prev
        }
        Err(e) => {
            log::warn!(
                "[coord] streaming_insert: switch_to_ascii failed: {e}; fall back to one-shot"
            );
            let (p, err) = polish_or_passthrough(
                raw,
                mode,
                hotwords,
                style_system_prompt,
                working_languages,
                chinese_script_preference,
                output_language_preference,
                llm_thinking_enabled,
                front_app,
                prior_turns,
            )
            .await;
            return (p, err, false);
        }
    };

    // 2. 起 typer 后台任务：从 mpsc 收 delta，串行调 type_unicode_chunk。
    // 同时累积 typed_text：屏幕上真正落字的内容，用于（a）SSE 中途失败时让 history
    // 与用户实际看到的内容一致；（b）pr-agent #412 反馈 \"saved output diverges
    // from what the user actually sees\"。
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let typer_handle = tokio::task::spawn_blocking(move || {
        drain_streaming_insert_deltas(rx, STREAMING_INSERT_FLUSH_INTERVAL)
    });

    // 3. 调流式润色，on_delta 塞 mpsc；should_cancel 检查 dictation 取消旗。
    let inner_for_cancel = Arc::clone(inner);
    let should_cancel = move || inner_for_cancel.state.lock().cancelled;
    let outcome = super::polish_or_passthrough_streaming(
        raw,
        mode,
        hotwords,
        style_system_prompt,
        working_languages,
        chinese_script_preference,
        output_language_preference,
        llm_thinking_enabled,
        front_app,
        prior_turns,
        move |delta: &str| {
            let _ = tx.send(delta.to_string());
        },
        should_cancel,
    )
    .await;
    // tx 已经被 move 进 on_delta 闭包；闭包随 polish_or_passthrough_streaming 返回
    // 而 drop，typer 那侧 blocking_recv 拿到 None 自然退出。

    // 4. 等 typer 把缓冲 drain 完，拿到实际落字的全文 + 第一条失败原因。
    let (typed_text, typer_failure) = typer_handle.await.unwrap_or_else(|e| {
        log::error!("[coord] streaming_insert: typer task join failed: {e}");
        (String::new(), Some(format!("typer join: {e}")))
    });
    let typed_chars = typed_text.chars().count();
    log::info!("[coord] streaming_insert: typer drained, typed {typed_chars} chars");

    // 5. 无论流是否成功，都恢复用户原输入源。
    log::info!("[coord] streaming_insert: restoring input source");
    if let Err(e) = crate::unicode_keystroke::restore_input_source(&app, prev_ime).await {
        log::warn!("[coord] streaming_insert: restore_input_source failed: {e}");
    } else {
        log::info!("[coord] streaming_insert: input source restored");
    }

    // 6. 把 outcome 翻译成 (polished, polish_error, already_streamed)。
    match outcome {
        super::StreamingPolishOutcome::Streamed(text) => {
            log::info!(
                "[coord] streaming_insert SUCCESS: polished_chars={} typed_chars={} typer_err={:?}",
                text.chars().count(),
                typed_chars,
                typer_failure
            );
            // 边界 case：polish 成功但 typer 在第一字就失败（最常见：session 开始时
            // 已处于 Secure Input；或 SendInput / enigo 拒绝）。屏幕上一字未见，
            // already_streamed=true 会让上层跳过 inserter，最终用户看不到任何内容。
            // 这里显式回退到一次性兜底，让正常 inserter 路径写出 polish 结果。
            // pr-agent #412 反馈 \"Missing fallback\"。
            if typed_chars == 0 {
                if let Some(reason) = typer_failure {
                    log::warn!(
                        "[coord] streaming_insert: zero chars typed despite polish success ({reason}); falling back to one-shot inserter"
                    );
                    return (text, Some(reason), false);
                }
            }
            // 先确定 final_text —— typer 中途失败时屏幕只有 typed_text 这一段，
            // history 记完整 polish 反而会让用户复盘困惑。让 history / clipboard /
            // 后续逻辑统统用 final_text，三处保持一致。
            // pr-agent #412 反馈 \"Clipboard Mismatch\"：之前先写 text 到剪贴板再
            // 决定 typer 是否中途失败，导致 Cmd+V 粘出用户屏幕上没见过的内容。
            let (final_text, polish_err) = match typer_failure {
                Some(e) => (typed_text, Some(format!("typing partially failed: {e}"))),
                None => (text, None),
            };
            // 把 final_text 写回剪贴板（默认 on，macOS/Windows 适用）。
            // Linux：fcitx5 插件已直写文字到目标 app，跳过剪贴板避免破坏用户数据。
            #[cfg(not(target_os = "linux"))]
            if inner.prefs.get().streaming_insert_save_clipboard {
                match arboard::Clipboard::new() {
                    Ok(mut cb) => match cb.set_text(final_text.clone()) {
                        Ok(()) => log::info!(
                            "[coord] streaming_insert: final text written to clipboard ({} chars)",
                            final_text.chars().count()
                        ),
                        Err(e) => {
                            log::warn!("[coord] streaming_insert: clipboard set_text failed: {e}")
                        }
                    },
                    Err(e) => {
                        log::warn!("[coord] streaming_insert: clipboard handle init failed: {e}")
                    }
                }
            } else {
                log::info!("[coord] streaming_insert: clipboard save skipped (pref off)");
            }
            (final_text, polish_err, true)
        }
        super::StreamingPolishOutcome::UnsupportedFallback => {
            log::info!(
                "[coord] streaming_insert: dispatch reported unsupported, fall back to one-shot"
            );
            let (p, e) = polish_or_passthrough(
                raw,
                mode,
                hotwords,
                style_system_prompt,
                working_languages,
                chinese_script_preference,
                output_language_preference,
                llm_thinking_enabled,
                front_app,
                prior_turns,
            )
            .await;
            (p, e, false)
        }
        super::StreamingPolishOutcome::Failed(reason) => {
            log::warn!(
                "[coord] streaming_insert FAILED: {reason}; typed {typed_chars} chars before failure"
            );
            // 流式失败但已经流了一部分 chars：用户屏幕上有半截 polish。history 应当
            // 跟屏幕一致 —— 记 typed_text 而不是 raw.text，否则保存内容跟用户看见的
            // 内容会分叉（pr-agent #412 \"Wrong final text\" 反馈）。
            // 一字都没流时 typed_text 是空串，回到 raw 一次性兜底。
            if typed_chars > 0 {
                (
                    typed_text,
                    Some(format!(
                        "streaming polish failed mid-stream after {typed_chars} chars: {reason}"
                    )),
                    true,
                )
            } else {
                (raw.text.clone(), Some(reason), false)
            }
        }
    }
}

fn drain_streaming_insert_deltas(
    rx: std::sync::mpsc::Receiver<String>,
    flush_interval: std::time::Duration,
) -> (String, Option<String>) {
    drain_streaming_insert_deltas_with(rx, flush_interval, flush_streaming_insert_buffer)
}

pub(crate) fn drain_streaming_insert_deltas_with<F>(
    rx: std::sync::mpsc::Receiver<String>,
    flush_interval: std::time::Duration,
    mut flush_pending: F,
) -> (String, Option<String>)
where
    F: FnMut(&mut String, &mut String) -> Option<String>,
{
    let mut typed_text = String::new();
    let mut first_failure: Option<String> = None;
    let mut pending = String::new();
    while let Ok(delta) = rx.recv() {
        pending.push_str(&delta);
        let flush_at = std::time::Instant::now() + flush_interval;
        loop {
            let now = std::time::Instant::now();
            if now >= flush_at {
                break;
            }
            match rx.recv_timeout(flush_at.duration_since(now)) {
                Ok(delta) => pending.push_str(&delta),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    first_failure = flush_pending(&mut pending, &mut typed_text);
                    return (typed_text, first_failure);
                }
            }
        }
        first_failure = flush_pending(&mut pending, &mut typed_text);
        if first_failure.is_some() {
            // 一旦类型链路出错（如 Secure Input 启用），后续 delta 全部丢弃，但仍
            // 把 mpsc drain 完，避免发送端阻塞。
            while rx.recv().is_ok() {}
            break;
        }
    }
    if first_failure.is_none() {
        first_failure = flush_pending(&mut pending, &mut typed_text);
    }
    (typed_text, first_failure)
}

fn flush_streaming_insert_buffer(pending: &mut String, typed_text: &mut String) -> Option<String> {
    flush_streaming_insert_buffer_with(
        pending,
        typed_text,
        crate::unicode_keystroke::type_unicode_chunk,
    )
}

pub(crate) fn flush_streaming_insert_buffer_with<F>(
    pending: &mut String,
    typed_text: &mut String,
    mut type_chunk: F,
) -> Option<String>
where
    F: FnMut(&str) -> Result<usize, crate::unicode_keystroke::TypeError>,
{
    if pending.is_empty() {
        return None;
    }
    let delta = std::mem::take(pending);
    let delta_chars = delta.chars().count();
    match type_chunk(&delta) {
        Ok(typed_chars) => {
            let appended = append_typed_prefix(typed_text, &delta, typed_chars);
            if appended < delta_chars {
                let reason = format!(
                    "type_unicode_chunk typed only {appended}/{delta_chars} chars without error"
                );
                log::error!(
                    "[coord] streaming_insert: {reason} at typed={} chars; \
                     dropping remaining deltas",
                    typed_text.chars().count()
                );
                Some(reason)
            } else {
                None
            }
        }
        Err(e) => {
            append_typed_prefix(typed_text, &delta, e.typed_chars());
            log::error!(
                "[coord] streaming_insert: type_unicode_chunk failed at typed={} chars: {e}; \
                 dropping remaining deltas",
                typed_text.chars().count()
            );
            Some(e.to_string())
        }
    }
}

pub(crate) fn finalize_polished_text(
    polished: String,
    _translation_active: bool,
    _raw_uses_llm: bool,
    _mode: PolishMode,
    _polish_error: &Option<String>,
    chinese_script_preference: crate::types::ChineseScriptPreference,
    correction_rules: &[crate::types::CorrectionRule],
    already_streamed: bool,
) -> String {
    if already_streamed {
        return polished;
    }
    // issue #622：无论是否经 LLM 润色成功，插入前都套用中文字形转换。
    // apply_chinese_script_preference 对 Auto 是 no-op；非 Auto 时确保最终插入文字
    // 不混简体——此前仅 Raw / 翻译 / 润色失败时强制转换，成功润色只靠 prompt 指示，
    // 而部分 provider 会跟随简体 ASR 输入继续输出简体（流式路径已在上游回退为一次性，
    // 见 streaming_insert_eligible）。
    let polished = apply_chinese_script_preference(&polished, chinese_script_preference);
    if correction_rules.is_empty() {
        polished
    } else {
        let corrected = apply_correction_rules(&polished, correction_rules);
        if corrected != polished {
            log::info!(
                "[coord] correction rules adjusted final text ({} → {} chars)",
                polished.chars().count(),
                corrected.chars().count()
            );
        }
        corrected
    }
}

pub(crate) fn streaming_insert_eligible(
    streaming_insert_enabled: bool,
    translation_active: bool,
    mode: PolishMode,
    raw_uses_llm: bool,
    chinese_script_preference: crate::types::ChineseScriptPreference,
) -> bool {
    streaming_insert_enabled
        && !translation_active
        && (mode != PolishMode::Raw || raw_uses_llm)
        // issue #622：非 Auto 字形偏好需在插入前整体转换；流式逐字落字无法回退，
        // 故关闭流式、走一次性路径（由 finalize_polished_text 套用字形转换）。
        && chinese_script_preference == crate::types::ChineseScriptPreference::Auto
}

pub(crate) fn default_done_message(status: InsertStatus, polish_failed: bool) -> Option<String> {
    if polish_failed {
        // polish 失败优先告知用户，即使 insert 成功也要让用户知道这版是原文
        Some("润色失败，已插入原文".to_string())
    } else {
        match status {
            InsertStatus::Inserted => None,
            InsertStatus::PasteSent => Some("已尝试粘贴".to_string()),
            InsertStatus::CopiedFallback => Some(if cfg!(target_os = "windows") {
                "已复制，请 Ctrl+V".to_string()
            } else {
                "已复制，请粘贴".to_string()
            }),
            InsertStatus::Failed => Some("插入失败".to_string()),
        }
    }
}

pub(crate) fn append_typed_prefix(target: &mut String, delta: &str, typed_chars: usize) -> usize {
    let mut end = 0;
    let mut appended = 0;
    for (idx, ch) in delta.char_indices().take(typed_chars) {
        end = idx + ch.len_utf8();
        appended += 1;
    }
    target.push_str(&delta[..end]);
    appended
}
