use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::types::HotkeyMode;

use super::qa::handle_qa_option_edge;
use super::*;

/// 同一个 hotkey 边沿之间的最小间隔。低于此阈值的连按整体作为误触丢弃 ——
/// 避免微动开关回弹 / 用户手抖双击造成的空转写报错和 ASR session 抢资源。
const HOTKEY_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

pub(super) async fn handle_pressed_edge(inner: &Arc<Inner>) {
    let was_held = inner.hotkey_trigger_held.swap(true, Ordering::SeqCst);
    if !was_held {
        // 防抖：相邻 < HOTKEY_DEBOUNCE 的边沿直接丢弃，记到 log 方便排查。
        // 与 `hotkey_trigger_held` 互补：held 防 press-without-release，本检查防
        // press-release-press 三连过快。每个有效边沿都会更新时间戳。
        let now = std::time::Instant::now();
        let too_soon = {
            let mut last = inner.last_hotkey_dispatch_at.lock();
            let drop = matches!(*last, Some(t) if now.duration_since(t) < HOTKEY_DEBOUNCE);
            if !drop {
                *last = Some(now);
            }
            drop
        };
        if too_soon {
            log::info!(
                "[coord] hotkey pressed edge debounced (< {} ms since last dispatch)",
                HOTKEY_DEBOUNCE.as_millis()
            );
            return;
        }

        // 路由：QA 浮窗可见时，rightOption 边沿走 QA；否则走主听写。详见 issue #118 v2。
        // 例外：dictation session 已经在跑（Starting / Listening / Processing / Inserting），
        // 即使 QA 浮窗被打开了，这条边沿也必须先走 dictation。否则 begin_qa_session 会
        // 第二次抢同一个麦克风 device —— 在 Linux/PipeWire 上甚至会成功打开两路捕获，
        // dictation 的 recorder 没人停；在 macOS/Windows 上 cpal 会拒绝第二次 build_input_stream
        // 但 dictation session 仍在跑、用户找不到从 QA 面板停掉它的入口。审计 3.3.1。
        let dictation_active = !matches!(inner.state.lock().phase, SessionPhase::Idle);
        let panel_visible = inner.qa_state.lock().panel_visible;
        if panel_visible && !dictation_active {
            handle_qa_option_edge(inner).await;
        } else {
            handle_pressed(inner).await;
        }
    }
}

pub(super) async fn handle_pressed(inner: &Arc<Inner>) {
    let mode = inner.prefs.get().hotkey.mode;
    let phase = inner.state.lock().phase;
    log::info!("[coord] hotkey pressed (mode={mode:?}, phase={phase:?})");
    match (mode, phase) {
        (HotkeyMode::Toggle, SessionPhase::Idle) => {
            // 冷却检查：end_session 刚收尾时禁止短时间内再次激活，
            // 避免三连按第 3 次误触（此时胶囊仍在离场动画周期内，issue #545）。
            let now = std::time::Instant::now();
            let on_cooldown = inner
                .session_cooldown_until
                .lock()
                .map(|deadline| now < deadline)
                .unwrap_or(false);
            if on_cooldown {
                log::info!(
                    "[coord] toggle activation blocked by cooldown (session still winding down)"
                );
                return;
            }
            let _ = begin_session(inner).await;
        }
        (HotkeyMode::Toggle, SessionPhase::Listening) => {
            let _ = end_session(inner).await;
        }
        (HotkeyMode::Hold, SessionPhase::Idle) => {
            let _ = begin_session(inner).await;
        }
        // Toggle 模式 Starting 阶段第二次按 → 用户想停。
        // 不能直接 end_session（ASR session 还没建好），存边沿，握手完成后立即触发。
        (HotkeyMode::Toggle, SessionPhase::Starting) => {
            request_stop_during_starting(inner, "toggle stop edge");
        }
        _ => {}
    }
}

pub(super) async fn handle_released_edge(inner: &Arc<Inner>) {
    let was_held = inner.hotkey_trigger_held.swap(false, Ordering::SeqCst);
    if was_held {
        // QA 浮窗可见时，Option 行为是 press-toggle（不分 hold/release），release 边沿忽略。
        // 与 handle_pressed_edge 的路由对称：dictation session 在跑时 Pressed 已经被路由到
        // dictation，那 Released 必须也路由到 dictation —— 否则 Hold 模式松开热键时
        // end_session 不会触发，dictation 永远停不下来。审计 3.3.1。
        let dictation_active = !matches!(inner.state.lock().phase, SessionPhase::Idle);
        let panel_visible = inner.qa_state.lock().panel_visible;
        if panel_visible && !dictation_active {
            return;
        }
        handle_released(inner).await;
    }
}

pub(super) async fn handle_released(inner: &Arc<Inner>) {
    let mode = inner.prefs.get().hotkey.mode;
    let phase = inner.state.lock().phase;
    log::info!("[coord] hotkey released (mode={mode:?}, phase={phase:?})");
    if mode == HotkeyMode::Toggle {
        // Toggle 听写松手不做事（点一下停）。Less Computer 走独立专用键监听器。
        return;
    }
    if mode == HotkeyMode::Hold {
        match phase {
            SessionPhase::Listening => {
                let _ = end_session(inner).await;
            }
            // Hold 模式 Starting 阶段松开 → 用户想停。同上：握手完成后再 end。
            SessionPhase::Starting => {
                request_stop_during_starting(inner, "hold release edge");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::batch_asr_chunk_limit_ms;
    use crate::coordinator::{
        append_typed_prefix, default_done_message, drain_streaming_insert_deltas_with,
        eligible_polish_context_turns, finalize_polished_text, flush_streaming_insert_buffer_with,
        streaming_insert_eligible,
    };
    use crate::types::{
        ChineseScriptPreference, CorrectionRule, DictationSession, InsertStatus, PolishMode,
    };

    fn correction_rule(pattern: &str, replacement: &str) -> CorrectionRule {
        CorrectionRule {
            id: "test".into(),
            pattern: pattern.into(),
            replacement: replacement.into(),
            enabled: true,
            created_at: String::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn history_session(
        id: &str,
        raw: &str,
        final_text: &str,
        style_pack_id: Option<&str>,
        translation_active: bool,
        polish_source: Option<&str>,
    ) -> DictationSession {
        DictationSession {
            id: id.into(),
            created_at: "2026-06-03T00:00:00Z".into(),
            raw_transcript: raw.into(),
            final_text: final_text.into(),
            mode: PolishMode::Structured,
            app_bundle_id: None,
            app_name: None,
            insert_status: InsertStatus::Inserted,
            error_code: None,
            duration_ms: Some(1000),
            dictionary_entry_count: None,
            has_audio_recording: None,
            style_pack_id: style_pack_id.map(str::to_string),
            translation_active,
            polish_source: polish_source.map(str::to_string),
        }
    }

    #[test]
    fn polish_context_resets_when_active_style_pack_changes() {
        let sessions = vec![
            history_session("new", "raw new", "final new", Some("pack.new"), false, None),
            history_session("old", "raw old", "final old", Some("pack.old"), false, None),
        ];

        let turns = eligible_polish_context_turns(sessions, "pack.new", false);

        assert_eq!(
            turns,
            vec![("raw new".to_string(), "final new".to_string())]
        );
    }

    #[test]
    fn normal_turn_uses_polished_source_of_translation_history_not_the_translation() {
        // 当前是普通润色轮：翻译历史喂"润色后的源文"，把译文剔除，避免外语污染。
        let sessions = vec![
            history_session(
                "translation",
                "你好",
                "Hello",
                Some("pack.new"),
                true,
                Some("你好。"),
            ),
            history_session("dictation", "继续", "继续。", Some("pack.new"), false, None),
        ];

        let turns = eligible_polish_context_turns(sessions, "pack.new", false);

        assert_eq!(
            turns,
            vec![
                ("你好".to_string(), "你好。".to_string()),
                ("继续".to_string(), "继续。".to_string()),
            ]
        );
    }

    #[test]
    fn normal_turn_skips_translation_history_without_polished_source() {
        // 译文历史没有 polish_source（解析失败 / 旧历史）→ 普通轮整条跳过，宁缺毋滥。
        let sessions = vec![
            history_session("translation", "你好", "Hello", Some("pack.new"), true, None),
            history_session("dictation", "继续", "继续。", Some("pack.new"), false, None),
        ];

        let turns = eligible_polish_context_turns(sessions, "pack.new", false);

        assert_eq!(turns, vec![("继续".to_string(), "继续。".to_string())]);
    }

    #[test]
    fn translation_turn_keeps_translation_text_of_translation_history() {
        // 当前还是翻译轮：翻译历史喂译文(final_text)，保持目标语言一致。
        let sessions = vec![history_session(
            "translation",
            "你好",
            "Hello",
            Some("pack.new"),
            true,
            Some("你好。"),
        )];

        let turns = eligible_polish_context_turns(sessions, "pack.new", true);

        assert_eq!(turns, vec![("你好".to_string(), "Hello".to_string())]);
    }

    #[test]
    fn translation_turn_uses_normal_history_final_text() {
        // 当前是翻译轮，普通历史照常喂 final_text（本就是源语言润色结果，不需要剔除）。
        let sessions = vec![history_session(
            "dictation",
            "继续",
            "继续。",
            Some("pack.new"),
            false,
            None,
        )];

        let turns = eligible_polish_context_turns(sessions, "pack.new", true);

        assert_eq!(turns, vec![("继续".to_string(), "继续。".to_string())]);
    }

    #[test]
    fn streamed_output_skips_postprocessing_mutations() {
        let rules = vec![correction_rule("Open AI", "OpenAI")];

        let result = finalize_polished_text(
            "Open AI".into(),
            false,
            false,
            PolishMode::Raw,
            &None,
            ChineseScriptPreference::Auto,
            &rules,
            true,
        );

        assert_eq!(result, "Open AI");
    }

    #[test]
    fn raw_llm_output_still_applies_script_preference() {
        let result = finalize_polished_text(
            "繁體".into(),
            false,
            true,
            PolishMode::Raw,
            &None,
            ChineseScriptPreference::Simplified,
            &[],
            false,
        );

        assert_eq!(result, "繁体");
    }

    #[test]
    fn non_streamed_output_still_applies_correction_rules() {
        let rules = vec![correction_rule("Open AI", "OpenAI")];

        let result = finalize_polished_text(
            "Open AI".into(),
            false,
            false,
            PolishMode::Raw,
            &None,
            ChineseScriptPreference::Auto,
            &rules,
            false,
        );

        assert_eq!(result, "OpenAI");
    }

    #[test]
    fn append_typed_prefix_keeps_unicode_char_boundaries() {
        let mut typed = String::from("前");

        let appended = append_typed_prefix(&mut typed, "a你🙂b", 3);

        assert_eq!(appended, 3);
        assert_eq!(typed, "前a你🙂");
    }

    #[test]
    fn append_typed_prefix_caps_at_delta_length() {
        let mut typed = String::new();

        let appended = append_typed_prefix(&mut typed, "好", 10);

        assert_eq!(appended, 1);
        assert_eq!(typed, "好");
    }

    #[test]
    fn streaming_insert_eligible_when_gates_allow() {
        assert!(streaming_insert_eligible(
            true,
            false,
            PolishMode::Light,
            false,
            crate::types::ChineseScriptPreference::Auto,
        ));
    }

    // issue #622：非 Auto 字形偏好必须关闭流式，改走会做字形转换的一次性路径。
    #[test]
    fn streaming_insert_ineligible_when_chinese_script_forced() {
        for pref in [
            crate::types::ChineseScriptPreference::Traditional,
            crate::types::ChineseScriptPreference::Simplified,
        ] {
            assert!(!streaming_insert_eligible(
                true,
                false,
                PolishMode::Light,
                false,
                pref,
            ));
        }
    }

    // issue #622：非 Auto + 成功 LLM 润色（非流式、无 error）时，最终插入文字
    // 必须套用字形转换，不能只靠 prompt 指示。覆盖 issue 给出的两个验收用例。
    #[test]
    fn finalize_forces_traditional_even_on_successful_polish() {
        let cases = [
            ("你知道你今天想要做什么吗？", "你知道你今天想要做什麼嗎？"),
            ("所以你已经考过了吗？", "所以你已經考過了嗎？"),
        ];
        for (input, expected) in cases {
            let out = finalize_polished_text(
                input.to_string(),
                false,             // translation_active
                true,              // raw_uses_llm
                PolishMode::Light, // 成功润色路径（非 Raw、非 error）
                &None,             // 无 polish_error
                crate::types::ChineseScriptPreference::Traditional,
                &[],   // 无 correction rules
                false, // already_streamed
            );
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn batch_asr_chunk_limit_applies_only_to_zhipu() {
        assert_eq!(batch_asr_chunk_limit_ms("zhipu"), Some(30_000));
        assert_eq!(batch_asr_chunk_limit_ms("openrouter"), Some(30_000));
        assert_eq!(batch_asr_chunk_limit_ms("whisper"), None);
        assert_eq!(batch_asr_chunk_limit_ms("siliconflow"), None);
        assert_eq!(batch_asr_chunk_limit_ms("groq"), None);
        assert_eq!(batch_asr_chunk_limit_ms("volcengine"), None);
    }

    #[test]
    fn default_done_message_works_correctly() {
        assert_eq!(
            default_done_message(InsertStatus::PasteSent, false),
            Some("已尝试粘贴".to_string())
        );
        assert_eq!(
            default_done_message(InsertStatus::Inserted, true),
            Some("润色失败，已插入原文".to_string())
        );
    }

    #[test]
    fn streaming_insert_batches_queued_deltas_before_flush() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send("你".to_string()).unwrap();
        tx.send("好".to_string()).unwrap();
        tx.send("🙂".to_string()).unwrap();
        drop(tx);

        let mut flushed = Vec::new();
        let (typed, failure) = drain_streaming_insert_deltas_with(
            rx,
            std::time::Duration::from_millis(50),
            |pending, typed_text| {
                flushed.push(pending.clone());
                typed_text.push_str(pending);
                pending.clear();
                None
            },
        );

        assert_eq!(flushed, vec!["你好🙂".to_string()]);
        assert_eq!(typed, "你好🙂");
        assert_eq!(failure, None);
    }

    #[test]
    fn flush_streaming_insert_buffer_keeps_partial_unicode_prefix() {
        let mut pending = "a你🙂b".to_string();
        let mut typed = String::new();

        let failure = flush_streaming_insert_buffer_with(&mut pending, &mut typed, |_| {
            Err(crate::unicode_keystroke::TypeError::Partial {
                typed_chars: 3,
                source: Box::new(platform_type_error()),
            })
        });

        assert_eq!(typed, "a你🙂");
        assert!(pending.is_empty());
        assert!(failure.is_some());
    }

    #[cfg(target_os = "macos")]
    fn platform_type_error() -> crate::unicode_keystroke::TypeError {
        crate::unicode_keystroke::TypeError::EventAllocFailed
    }

    #[cfg(target_os = "windows")]
    fn platform_type_error() -> crate::unicode_keystroke::TypeError {
        crate::unicode_keystroke::TypeError::SendInputFailed("fail".into())
    }

    #[cfg(target_os = "linux")]
    fn platform_type_error() -> crate::unicode_keystroke::TypeError {
        crate::unicode_keystroke::TypeError::EnigoText("fail".into())
    }
}
