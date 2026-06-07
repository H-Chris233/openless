//! Less Computer 语音 Agent 热键（coding_agent / less_computer）的 supervisor /
//! bridge / 事件处理。从 `coordinator.rs` 机械拆出（行为保持，仅可见性提升）。

use super::*;

// ─────────────────────── coding agent hotkey supervisor ───────────────────────

pub(crate) fn coding_agent_hotkey_supervisor_loop(inner: Arc<Inner>) {
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        update_coding_agent_hotkey_binding_now(&inner);
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

pub(crate) fn update_coding_agent_hotkey_binding_now(inner: &Arc<Inner>) {
    #[cfg(not(target_os = "macos"))]
    {
        // Less Computer is intentionally macOS-only for now; keep Windows/Linux hidden and inert.
        take_coding_agent_hotkeys_on_main_thread(inner);
        return;
    }

    #[cfg(target_os = "macos")]
    {
        let prefs = inner.prefs.get();
        let Some(binding) = prefs.coding_agent_voice_hotkey.clone() else {
            take_coding_agent_hotkeys_on_main_thread(inner);
            log::info!("[less-computer] hotkey disabled");
            return;
        };
        if !prefs.coding_agent_enabled || is_unconfigured_shortcut(&binding) {
            take_coding_agent_hotkeys_on_main_thread(inner);
            return;
        }

        if let Some(modifier_binding) = less_computer_modifier_binding(&binding) {
            take_coding_agent_combo_hotkey_on_main_thread(inner);
            if let Some(monitor) = inner.coding_agent_modifier_hotkey.lock().as_ref() {
                monitor.update_binding(modifier_binding);
                return;
            }
            let (tx, rx) = mpsc::channel::<HotkeyEvent>();
            match HotkeyMonitor::start(modifier_binding, tx) {
                Ok(monitor) => {
                    *inner.coding_agent_modifier_hotkey.lock() = Some(monitor);
                    log::info!(
                        "[less-computer] modifier hotkey installed ({})",
                        binding.display_label()
                    );
                    let bridge_inner = Arc::clone(inner);
                    std::thread::Builder::new()
                        .name("openless-less-computer-modifier-bridge".into())
                        .spawn(move || less_computer_modifier_bridge_loop(bridge_inner, rx))
                        .ok();
                }
                Err(e) => log::warn!("[less-computer] modifier hotkey install failed: {e}"),
            }
            return;
        }

        inner.coding_agent_modifier_hotkey.lock().take();
        let app = match inner.app.lock().clone() {
            Some(app) => app,
            None => {
                log::warn!("[less-computer] AppHandle 未 bind，跳过组合键注册");
                return;
            }
        };
        let inner_clone = Arc::clone(inner);
        let binding_for_main = binding.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(monitor) = inner_clone.coding_agent_combo_hotkey.lock().as_ref() {
                if let Err(e) = monitor.update_binding(binding_for_main.clone()) {
                    log::warn!("[less-computer] combo hotkey update failed: {e}");
                }
                return;
            }
            let (tx, rx) = mpsc::channel::<ComboHotkeyEvent>();
            match ComboHotkeyMonitor::start(binding_for_main.clone(), tx) {
                Ok(monitor) => {
                    *inner_clone.coding_agent_combo_hotkey.lock() = Some(monitor);
                    log::info!(
                        "[less-computer] combo hotkey installed ({})",
                        binding_for_main.display_label()
                    );
                    let bridge_inner = Arc::clone(&inner_clone);
                    std::thread::Builder::new()
                        .name("openless-less-computer-combo-bridge".into())
                        .spawn(move || less_computer_combo_bridge_loop(bridge_inner, rx))
                        .ok();
                }
                Err(e) => log::warn!("[less-computer] combo hotkey install failed: {e}"),
            }
        });
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn less_computer_modifier_binding(
    binding: &crate::types::ShortcutBinding,
) -> Option<crate::types::HotkeyBinding> {
    let trigger = crate::shortcut_binding::legacy_modifier_trigger(binding)?;
    Some(crate::types::HotkeyBinding {
        trigger,
        mode: crate::types::HotkeyMode::Hold,
        keys: None,
    })
}

/// Less Computer bridge 循环把不同来源的事件枚举（修饰键 `HotkeyEvent` /
/// combo `ComboHotkeyEvent`）归一成的语义边沿。
enum LessComputerEdge {
    Pressed,
    Released,
    Cancelled,
    /// 与 Less Computer 无关的事件（如 translation/qa），忽略。
    Ignore,
}

/// 修饰键与 combo 两个 bridge 循环体逐字相同，只是事件枚举类型不同。抽出泛型
/// 循环，调用方传入把各自事件映射到 `LessComputerEdge` 的闭包。`block_on(handle_*)`
/// 调用语义保持不变。
fn less_computer_bridge_loop<E>(
    inner: Arc<Inner>,
    rx: mpsc::Receiver<E>,
    to_edge: impl Fn(&E) -> LessComputerEdge,
) {
    while let Ok(evt) = rx.recv() {
        if inner.shortcut_recording_active.load(Ordering::SeqCst) {
            continue;
        }
        let inner_cloned = Arc::clone(&inner);
        match to_edge(&evt) {
            LessComputerEdge::Pressed => {
                async_runtime::block_on(async {
                    handle_less_computer_pressed(&inner_cloned).await
                });
            }
            LessComputerEdge::Released => {
                async_runtime::block_on(async {
                    handle_less_computer_released(&inner_cloned).await
                });
            }
            LessComputerEdge::Cancelled => cancel_session(&inner_cloned),
            LessComputerEdge::Ignore => {}
        }
    }
}

pub(crate) fn less_computer_modifier_bridge_loop(
    inner: Arc<Inner>,
    rx: mpsc::Receiver<HotkeyEvent>,
) {
    less_computer_bridge_loop(inner, rx, |evt| match evt {
        HotkeyEvent::Pressed => LessComputerEdge::Pressed,
        HotkeyEvent::Released => LessComputerEdge::Released,
        HotkeyEvent::Cancelled => LessComputerEdge::Cancelled,
        HotkeyEvent::TranslationModifierPressed | HotkeyEvent::QaShortcutPressed => {
            LessComputerEdge::Ignore
        }
    });
}

pub(crate) fn less_computer_combo_bridge_loop(
    inner: Arc<Inner>,
    rx: mpsc::Receiver<ComboHotkeyEvent>,
) {
    less_computer_bridge_loop(inner, rx, |evt| match evt {
        ComboHotkeyEvent::Pressed => LessComputerEdge::Pressed,
        ComboHotkeyEvent::Released => LessComputerEdge::Released,
    });
}

pub(crate) async fn handle_less_computer_pressed(inner: &Arc<Inner>) {
    let prefs = inner.prefs.get();
    if !prefs.coding_agent_enabled {
        return;
    }
    if !matches!(inner.state.lock().phase, SessionPhase::Idle) {
        log::info!("[less-computer] press ignored: dictation session already active");
        return;
    }
    if !matches!(inner.qa_state.lock().phase, QaPhase::Idle) {
        log::info!("[less-computer] press ignored: QA session active");
        return;
    }

    if begin_session(inner).await.is_err() {
        return;
    }
    let started = {
        let mut state = inner.state.lock();
        if matches!(
            state.phase,
            SessionPhase::Starting | SessionPhase::Listening
        ) {
            state.voice_agent = true;
            log::info!(
                "[less-computer] voice session started (session={:?})",
                state.session_id
            );
            true
        } else {
            false
        }
    };
    // 一按下键（开始录音）就点亮整屏彩虹描边，贯穿 录音 → 处理 → 出结果，完成/关闭才熄灭。
    if started {
        if let Some(app) = inner.app.lock().clone() {
            crate::show_less_computer_glow(&app);
        }
    }
}

pub(crate) async fn handle_less_computer_released(inner: &Arc<Inner>) {
    let (phase, voice_agent) = {
        let state = inner.state.lock();
        (state.phase, state.voice_agent)
    };
    if !voice_agent {
        return;
    }
    match phase {
        SessionPhase::Listening => {
            let _ = end_session(inner).await;
            // 收尾后熄灭整屏描边。正常路径 run_voice_agent_transcript 已熄过、这里兜底；
            // 空转写/出错路径不进 run_voice_agent_transcript，全靠这里熄，否则描边卡住不灭。
            if let Some(app) = inner.app.lock().clone() {
                crate::hide_less_computer_glow(&app);
            }
        }
        SessionPhase::Starting => {
            // 握手中松手：排队；正常路径真正收尾在 begin 续流的 end_session → run_voice_agent_transcript 熄灭。
            request_stop_during_starting(inner, "less-computer release edge");
            // 但若初始化失败永远到不了 Listening（不会进 run_voice_agent_transcript），
            // 描边会永久卡屏 → 这里兜底熄灭。Listening 分支已有熄灭逻辑，故只在 Starting 加。
            if let Some(app) = inner.app.lock().clone() {
                crate::hide_less_computer_glow(&app);
            }
        }
        _ => {}
    }
}

pub(crate) fn take_coding_agent_hotkeys_on_main_thread(inner: &Arc<Inner>) {
    inner.coding_agent_modifier_hotkey.lock().take();
    take_coding_agent_combo_hotkey_on_main_thread(inner);
}

pub(crate) fn take_coding_agent_combo_hotkey_on_main_thread(inner: &Arc<Inner>) {
    take_combo_monitor_on_main_thread(inner, |inner| &inner.coding_agent_combo_hotkey);
}

/// 快取用：抓当前选中文本 → Claude 润色 → 回插（替换选区）。全程胶囊反馈。
pub(crate) async fn handle_coding_agent_quick(inner: &Arc<Inner>) {
    let prefs = inner.prefs.get();
    if !prefs.coding_agent_enabled {
        return;
    }
    let selection = tauri::async_runtime::spawn_blocking(crate::selection::capture_selection)
        .await
        .ok()
        .flatten();
    let source_text = match selection {
        Some(ctx) => ctx.text,
        None => {
            log::info!("[coding-agent] 快取用：没有选中文本");
            emit_capsule(
                inner,
                CapsuleState::Error,
                0.0,
                0,
                Some("请先选中文本，再按快捷键".to_string()),
                None,
            );
            return;
        }
    };

    log::info!(
        "[coding-agent] 快取用：润色 {} 字",
        source_text.chars().count()
    );
    emit_capsule(
        inner,
        CapsuleState::Polishing,
        0.0,
        0,
        Some("Claude 润色中…".to_string()),
        None,
    );

    let prompt = format!(
        "请润色下面这段文字，使其更通顺自然、表达更清晰，保持原意、语言和事实不变。\
         直接输出润色后的文本，不要加任何解释、前缀或引号：\n\n{source_text}"
    );

    // 纯文本润色：不需要任何工具 → plan 只读、无 guard、便宜快、最可靠。
    let mut req = crate::coding_agent::CodingAgentRequest::new("quick-polish", prompt);
    req.model = prefs
        .coding_agent_model
        .clone()
        .filter(|m| !m.trim().is_empty())
        .or_else(|| Some("sonnet".to_string()));
    req.permission_mode = crate::coding_agent::CodingAgentPermissionMode::Plan;
    req.allowed_tools = Vec::new();
    req.max_budget_usd = Some(0.2);
    req.timeout_secs = 60;
    req.session_persistence = false;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let run = async_runtime::spawn(async move {
        crate::coding_agent::run_claude_agent("claude", req, tx, cancel).await
    });

    let mut final_text = String::new();
    let mut error_msg: Option<String> = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            crate::coding_agent::CodingAgentEvent::Completed { text, .. } => final_text = text,
            crate::coding_agent::CodingAgentEvent::Error { message, .. } => {
                error_msg = Some(message)
            }
            _ => {}
        }
    }
    let run_result = run.await;

    let final_text = final_text.trim().to_string();
    if final_text.is_empty() {
        let msg = error_msg
            .or_else(|| match run_result {
                Ok(Err(e)) => Some(e.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "Claude 无结果（确认已登录 claude 且额度充足）".to_string());
        log::warn!("[coding-agent] 快取用失败: {msg}");
        emit_capsule(inner, CapsuleState::Error, 0.0, 0, Some(msg), None);
        return;
    }

    let inserted = final_text.chars().count() as u32;
    let inner2 = Arc::clone(inner);
    let restore = prefs.restore_clipboard_after_paste;
    let paste_shortcut = prefs.paste_shortcut;
    let _ = tauri::async_runtime::spawn_blocking(move || {
        inner2.inserter.insert(&final_text, restore, paste_shortcut)
    })
    .await;
    log::info!("[coding-agent] 快取用：已回插 {inserted} 字");
    emit_capsule(inner, CapsuleState::Done, 0.0, 0, None, Some(inserted));
}
