//! 听写 / QA / combo / 翻译 / action / window 热键的 supervisor / bridge / 事件处理循环。
//!
//! 从 `coordinator.rs` 机械拆出（行为保持，仅可见性提升）。所有自由函数/类型
//! 仍通过 `use super::*` 看到父模块的 `Inner` / `ActiveAsr` / 兄弟 helper。

use super::*;

pub(crate) fn hotkey_supervisor_loop(inner: Arc<Inner>) {
    let mut attempts: u32 = 0;
    let capability = HotkeyMonitor::capability();
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let prefs = inner.prefs.get();

        if inner.hotkey.lock().is_some() {
            return;
        }
        // Linux: 启动前检查 fcitx5 插件是否可用
        #[cfg(target_os = "linux")]
        if !crate::linux_fcitx::available() {
            *inner.hotkey_status.lock() = HotkeyStatus {
                adapter: capability.adapter,
                state: HotkeyStatusState::Failed,
                message: Some("fcitx5 插件不可用 — 请确保 fcitx5 已安装且在运行".into()),
                last_error: Some(crate::types::HotkeyInstallError {
                    code: "fcitx5_unavailable".into(),
                    message: "fcitx5 插件 DBus 接口无响应".into(),
                }),
            };
            log::warn!("[hotkey-supervisor] fcitx5 plugin unavailable, retrying...");
            attempts += 1;
            std::thread::sleep(std::time::Duration::from_secs(3));
            continue;
        }
        *inner.hotkey_status.lock() = HotkeyStatus {
            adapter: capability.adapter,
            state: HotkeyStatusState::Starting,
            message: Some(format!("正在安装全局快捷键监听（第 {} 次）", attempts + 1)),
            last_error: None,
        };
        let trigger = crate::shortcut_binding::legacy_modifier_trigger(&prefs.dictation_hotkey)
            .unwrap_or(crate::types::HotkeyTrigger::Custom);
        let binding = crate::types::HotkeyBinding {
            trigger,
            mode: prefs.hotkey.mode,
            keys: None,
        };
        let (tx, rx) = mpsc::channel::<HotkeyEvent>();
        #[cfg(target_os = "linux")]
        let (fcitx_tx, fcitx_binding) = (tx.clone(), binding.clone());
        match HotkeyMonitor::start(binding, tx) {
            Ok(monitor) => {
                let adapter = monitor.kind();
                *inner.hotkey.lock() = Some(monitor);
                if let Some(monitor) = inner.hotkey.lock().as_ref() {
                    let (qa_trigger, translation_trigger) = modifier_shortcut_triggers(&inner);
                    monitor.update_modifier_shortcuts(qa_trigger, translation_trigger);
                }
                *inner.hotkey_status.lock() = HotkeyStatus {
                    adapter,
                    state: HotkeyStatusState::Installed,
                    message: Some(format!("{} 已安装", adapter.display_name())),
                    last_error: None,
                };
                log::info!(
                    "[coord] hotkey listener installed (after {} attempt(s))",
                    attempts + 1
                );
                let inner_clone = Arc::clone(&inner);
                std::thread::Builder::new()
                    .name("openless-hotkey-bridge".into())
                    .spawn(move || hotkey_bridge_loop(inner_clone, rx))
                    .ok();
                // Linux: 启动 fcitx5 插件信号监听作为热键源。
                #[cfg(target_os = "linux")]
                {
                    let (qa_trigger, translation_trigger) = modifier_shortcut_triggers(&inner);
                    let custom_key = custom_dictation_key_string(&inner);
                    crate::linux_fcitx::start_dictation_signal_listener(
                        fcitx_tx,
                        fcitx_binding.clone(),
                        qa_trigger,
                        translation_trigger,
                        custom_key,
                    );
                    if fcitx_binding.trigger == crate::types::HotkeyTrigger::Custom {
                        sync_custom_dictation_to_plugin(&inner);
                    } else {
                        crate::linux_fcitx::sync_binding_to_plugin(&fcitx_binding);
                    }
                }
                return;
            }
            Err(e) => {
                attempts += 1;
                let error_message = e.message.clone();
                *inner.hotkey_status.lock() = HotkeyStatus {
                    adapter: capability.adapter,
                    state: HotkeyStatusState::Failed,
                    message: Some(error_message.clone()),
                    last_error: Some(e),
                };
                if attempts <= 3 || attempts % 10 == 0 {
                    log::warn!(
                        "[coord] hotkey listener attempt #{attempts} failed: {}; retrying in 3s",
                        error_message
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        }
    }
}

// ─────────────────────────── QA hotkey supervisor ───────────────────────────

pub(crate) fn qa_hotkey_supervisor_loop(inner: Arc<Inner>) {
    let mut attempts: u32 = 0;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // 用户已经把 QA 关掉就睡着等 prefs 改动；改动通过 update_qa_hotkey_binding 唤醒。
        let binding = match inner.prefs.get().qa_hotkey.clone() {
            Some(b) => b,
            None => {
                inner.qa_hotkey.lock().take();
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };
        if crate::shortcut_binding::legacy_modifier_trigger(&binding).is_some() {
            inner.qa_hotkey.lock().take();
            if let Some(monitor) = inner.hotkey.lock().as_ref() {
                let (qa_trigger, translation_trigger) = modifier_shortcut_triggers(&inner);
                monitor.update_modifier_shortcuts(qa_trigger, translation_trigger);
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }

        if inner.qa_hotkey.lock().is_some() {
            // 已注册成功 → 不重复装；睡 5s 复查（ binding 变化由 update 路径手动触发 ）。
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }

        // global-hotkey crate 在 macOS 走 Carbon RegisterEventHotKey，要求 manager
        // 在主线程构造，否则 register() 看起来 Ok 但事件根本不会派发——这是 issue #118
        // PR #119 第一版漏掉的关键步骤，导致用户按了 hotkey 完全无反应。这里通过
        // run_on_main_thread 把 QaHotkeyMonitor::start 跳到主线程跑，结果再回 channel。
        let app = inner.app.lock().clone();
        let app = match app {
            Some(a) => a,
            None => {
                // 启动期 AppHandle 还没 bind，再等。
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };

        let (tx, rx) = mpsc::channel::<QaHotkeyEvent>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<QaHotkeyMonitor, QaHotkeyError>>(1);
        let binding_for_main = binding.clone();
        let _ = app.run_on_main_thread(move || {
            let result = QaHotkeyMonitor::start(binding_for_main, tx);
            let _ = init_tx.send(result);
        });

        // run_on_main_thread 是 fire-and-forget；等主线程跑完结果回来。给 5s 上限避免
        // 主线程繁忙时 supervisor 永久阻塞。
        let init_result = match init_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(r) => r,
            Err(_) => {
                attempts += 1;
                if attempts <= 3 || attempts % 10 == 0 {
                    log::warn!(
                        "[coord] QA hotkey 第 {attempts} 次注册超时（主线程未回执）；3s 后重试"
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
                continue;
            }
        };

        match init_result {
            Ok(monitor) => {
                *inner.qa_hotkey.lock() = Some(monitor);
                log::info!(
                    "[coord] QA hotkey listener installed on main thread (after {} attempt(s))",
                    attempts + 1
                );
                let inner_clone = Arc::clone(&inner);
                std::thread::Builder::new()
                    .name("openless-qa-hotkey-bridge".into())
                    .spawn(move || qa_hotkey_bridge_loop(inner_clone, rx))
                    .ok();
                attempts = 0;
            }
            Err(e) => {
                attempts += 1;
                if attempts <= 3 || attempts % 10 == 0 {
                    log::warn!("[coord] QA hotkey 第 {attempts} 次注册失败: {e}; 3s 后重试");
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        }
    }
}

pub(crate) fn qa_hotkey_bridge_loop(inner: Arc<Inner>, rx: mpsc::Receiver<QaHotkeyEvent>) {
    while let Ok(evt) = rx.recv() {
        if inner.shortcut_recording_active.load(Ordering::SeqCst) {
            continue;
        }
        let inner_cloned = Arc::clone(&inner);
        match evt {
            QaHotkeyEvent::Pressed => {
                async_runtime::spawn(async move { handle_qa_hotkey_pressed(&inner_cloned).await });
            }
        }
    }
}

// ─────────────────────────── combo hotkey supervisor ───────────────────────────
pub(crate) fn combo_hotkey_supervisor_loop(inner: Arc<Inner>) {
    let mut attempts: u32 = 0;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // 读当前 prefs
        let prefs = inner.prefs.get();
        if crate::shortcut_binding::legacy_modifier_trigger(&prefs.dictation_hotkey).is_some() {
            // 不是 Custom → 睡着等 prefs 改动
            take_combo_hotkey_on_main_thread(&inner);
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }

        let binding = prefs.dictation_hotkey.clone();
        if is_unconfigured_shortcut(&binding) {
            take_combo_hotkey_on_main_thread(&inner);
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }

        if inner.combo_hotkey.lock().is_some() {
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }

        let app = inner.app.lock().clone();
        let app = match app {
            Some(a) => a,
            None => {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };

        let (tx, rx) = mpsc::channel::<ComboHotkeyEvent>();
        let (init_tx, init_rx) =
            mpsc::sync_channel::<Result<ComboHotkeyMonitor, ComboHotkeyError>>(1);
        let binding_for_main = binding.clone();
        let _ = app.run_on_main_thread(move || {
            let result = ComboHotkeyMonitor::start(binding_for_main, tx);
            let _ = init_tx.send(result);
        });

        let init_result = match init_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(r) => r,
            Err(_) => {
                attempts += 1;
                if attempts <= 3 || attempts % 10 == 0 {
                    log::warn!(
                        "[coord] combo hotkey 第 {attempts} 次注册超时（主线程未回执）；3s 后重试"
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
                continue;
            }
        };

        match init_result {
            Ok(monitor) => {
                *inner.combo_hotkey.lock() = Some(monitor);
                log::info!(
                    "[coord] combo hotkey listener installed on main thread (after {} attempt(s))",
                    attempts + 1
                );
                let inner_clone = Arc::clone(&inner);
                std::thread::Builder::new()
                    .name("openless-combo-hotkey-bridge".into())
                    .spawn(move || combo_hotkey_bridge_loop(inner_clone, rx))
                    .ok();
                attempts = 0;
            }
            Err(e) => {
                attempts += 1;
                if attempts <= 3 || attempts % 10 == 0 {
                    log::warn!("[coord] combo hotkey 第 {attempts} 次注册失败: {e}; 3s 后重试");
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        }
    }
}

pub(crate) fn combo_hotkey_bridge_loop(inner: Arc<Inner>, rx: mpsc::Receiver<ComboHotkeyEvent>) {
    while let Ok(evt) = rx.recv() {
        if inner.shortcut_recording_active.load(Ordering::SeqCst) {
            continue;
        }
        let inner_cloned = Arc::clone(&inner);
        match evt {
            // P0 #468/#475: 同 hotkey_bridge_loop —— Pressed/Released 必须串行 await，
            // 否则 latch 竞态导致 combo 快捷键二次按键失效。
            ComboHotkeyEvent::Pressed => {
                async_runtime::block_on(async {
                    handle_pressed_edge(&inner_cloned).await;
                });
            }
            ComboHotkeyEvent::Released => {
                async_runtime::block_on(async {
                    handle_released_edge(&inner_cloned).await;
                });
            }
        }
    }
}

pub(crate) fn translation_hotkey_supervisor_loop(inner: Arc<Inner>) {
    let mut attempts: u32 = 0;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let binding = inner.prefs.get().translation_hotkey;
        if is_builtin_translation_shift(&binding)
            || crate::shortcut_binding::legacy_modifier_trigger(&binding).is_some()
        {
            take_translation_hotkey_on_main_thread(&inner);
            if let Some(monitor) = inner.hotkey.lock().as_ref() {
                let (qa_trigger, translation_trigger) = modifier_shortcut_triggers(&inner);
                monitor.update_modifier_shortcuts(qa_trigger, translation_trigger);
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }

        if inner.translation_hotkey.lock().is_some() {
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }

        let app = match inner.app.lock().clone() {
            Some(a) => a,
            None => {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };

        let (tx, rx) = mpsc::channel::<ComboHotkeyEvent>();
        let (init_tx, init_rx) =
            mpsc::sync_channel::<Result<ComboHotkeyMonitor, ComboHotkeyError>>(1);
        let binding_for_main = binding.clone();
        let _ = app.run_on_main_thread(move || {
            let result = ComboHotkeyMonitor::start(binding_for_main, tx);
            let _ = init_tx.send(result);
        });

        let init_result = match init_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(r) => r,
            Err(_) => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_secs(3));
                continue;
            }
        };

        match init_result {
            Ok(monitor) => {
                *inner.translation_hotkey.lock() = Some(monitor);
                let inner_clone = Arc::clone(&inner);
                std::thread::Builder::new()
                    .name("openless-translation-hotkey-bridge".into())
                    .spawn(move || translation_hotkey_bridge_loop(inner_clone, rx))
                    .ok();
                attempts = 0;
            }
            Err(e) => {
                attempts += 1;
                if attempts <= 3 || attempts % 10 == 0 {
                    log::warn!(
                        "[coord] translation hotkey 第 {attempts} 次注册失败: {e}; 3s 后重试"
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        }
    }
}

pub(crate) fn update_translation_hotkey_on_main_thread(
    inner: Arc<Inner>,
    binding: crate::types::ShortcutBinding,
) -> Result<(), ComboHotkeyError> {
    if let Some(monitor) = inner.translation_hotkey.lock().as_ref() {
        return monitor.update_binding(binding);
    }
    let (tx, rx) = mpsc::channel::<ComboHotkeyEvent>();
    let monitor = ComboHotkeyMonitor::start(binding, tx)?;
    *inner.translation_hotkey.lock() = Some(monitor);
    let bridge_inner = Arc::clone(&inner);
    std::thread::Builder::new()
        .name("openless-translation-hotkey-bridge".into())
        .spawn(move || translation_hotkey_bridge_loop(bridge_inner, rx))
        .map_err(|e| ComboHotkeyError::RegisterFailed(format!("spawn bridge thread: {e}")))?;
    Ok(())
}

pub(crate) fn translation_hotkey_bridge_loop(
    inner: Arc<Inner>,
    rx: mpsc::Receiver<ComboHotkeyEvent>,
) {
    while let Ok(evt) = rx.recv() {
        if inner.shortcut_recording_active.load(Ordering::SeqCst) {
            continue;
        }
        if matches!(evt, ComboHotkeyEvent::Pressed) {
            mark_translation_modifier_seen(&inner);
        }
    }
}

pub(crate) fn action_hotkey_supervisor_loop(inner: Arc<Inner>, kind: ActionHotkeyKind) {
    let mut attempts: u32 = 0;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        // None = 用户主动停用：反注册并睡着等 prefs 改动（由 update 路径唤醒）。
        let Some(binding) = action_hotkey_binding(&inner, kind) else {
            take_action_hotkey_on_main_thread(&inner, kind);
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        };
        if is_modifier_only_shortcut(&binding) {
            take_action_hotkey_on_main_thread(&inner, kind);
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }

        if action_hotkey_slot(&inner, kind).lock().is_some() {
            std::thread::sleep(std::time::Duration::from_secs(5));
            continue;
        }

        let app = match inner.app.lock().clone() {
            Some(a) => a,
            None => {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };

        let (tx, rx) = mpsc::channel::<ComboHotkeyEvent>();
        let (init_tx, init_rx) =
            mpsc::sync_channel::<Result<ComboHotkeyMonitor, ComboHotkeyError>>(1);
        let binding_for_main = binding.clone();
        let _ = app.run_on_main_thread(move || {
            let result = ComboHotkeyMonitor::start(binding_for_main, tx);
            let _ = init_tx.send(result);
        });

        let init_result = match init_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(r) => r,
            Err(_) => {
                attempts += 1;
                if attempts <= 3 || attempts % 10 == 0 {
                    log::warn!(
                        "[coord] action hotkey {kind:?} 第 {attempts} 次注册超时；3s 后重试"
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
                continue;
            }
        };

        match init_result {
            Ok(monitor) => {
                *action_hotkey_slot(&inner, kind).lock() = Some(monitor);
                log::info!(
                    "[coord] action hotkey {kind:?} listener installed after {} attempt(s)",
                    attempts + 1
                );
                let inner_clone = Arc::clone(&inner);
                std::thread::Builder::new()
                    .name(action_hotkey_bridge_thread_name(kind).into())
                    .spawn(move || action_hotkey_bridge_loop(inner_clone, rx, kind))
                    .ok();
                attempts = 0;
            }
            Err(e) => {
                attempts += 1;
                if attempts <= 3 || attempts % 10 == 0 {
                    log::warn!(
                        "[coord] action hotkey {kind:?} 第 {attempts} 次注册失败: {e}; 3s 后重试"
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        }
    }
}

pub(crate) fn action_hotkey_bridge_loop(
    inner: Arc<Inner>,
    rx: mpsc::Receiver<ComboHotkeyEvent>,
    kind: ActionHotkeyKind,
) {
    while let Ok(evt) = rx.recv() {
        if inner.shortcut_recording_active.load(Ordering::SeqCst) {
            continue;
        }
        if matches!(evt, ComboHotkeyEvent::Pressed) {
            handle_action_hotkey_pressed(&inner, kind);
        }
    }
}

pub(crate) fn handle_action_hotkey_pressed(inner: &Arc<Inner>, kind: ActionHotkeyKind) {
    match kind {
        ActionHotkeyKind::SwitchStyle => switch_to_previous_style(inner),
        ActionHotkeyKind::OpenApp => {
            if let Some(app) = inner.app.lock().clone() {
                let app_for_main = app.clone();
                let _ = app.run_on_main_thread(move || {
                    crate::show_main_window(&app_for_main);
                });
            }
        }
    }
}

pub(crate) fn switch_to_previous_style(inner: &Arc<Inner>) {
    let mut prefs = inner.prefs.get();
    let packs = match inner.style_packs.list() {
        Ok(packs) => packs,
        Err(error) => {
            log::warn!("[coord] switch style hotkey failed to load style packs: {error}");
            return;
        }
    };
    let enabled: Vec<crate::types::StylePack> =
        packs.into_iter().filter(|pack| pack.enabled).collect();
    if enabled.len() <= 1 {
        log::info!("[coord] switch style hotkey ignored: enabled style count <= 1");
        return;
    }
    let current_index = enabled
        .iter()
        .position(|pack| pack.id == prefs.active_style_pack_id)
        .unwrap_or(0);
    let next_index = if current_index == 0 {
        enabled.len() - 1
    } else {
        current_index - 1
    };
    prefs.active_style_pack_id = enabled[next_index].id.clone();
    sync_style_pack_preferences(&mut prefs, &enabled);
    if let Err(e) = inner.prefs.set(prefs.clone()) {
        log::warn!("[coord] switch style hotkey 保存失败: {e}");
    } else {
        log::info!(
            "[coord] switch style hotkey changed active style pack to {}",
            prefs.active_style_pack_id
        );
        if let Some(app) = inner.app.lock().clone() {
            let _ = app.emit("prefs:changed", &prefs);
            let _ = app.emit_to("main", "prefs:changed", &prefs);
            let app_for_main = app.clone();
            let _ = app.run_on_main_thread(move || {
                if let Err(err) = crate::refresh_tray_microphone_menu(&app_for_main) {
                    log::warn!("[tray] refresh style menu after switch style hotkey failed: {err}");
                }
            });
        }
    }
}

/// 在主线程上 `.take()` 一个 combo 监听槽。
///
/// 这三类 combo 热键（combo / translation / coding-agent）的「拿 app → 有则
/// `run_on_main_thread` 里 take，无则直接 take」marshaling 语义逐字相同，只是 take
/// 的字段不同（issue #118：监听器必须在主线程释放）。`slot` 闭包按 `&Arc<Inner>`
/// 取回对应字段，消除三处复制。`action_hotkey` 类已用 `action_hotkey_slot` 单独泛化。
pub(crate) fn take_combo_monitor_on_main_thread(
    inner: &Arc<Inner>,
    slot: impl Fn(&Arc<Inner>) -> &Mutex<Option<ComboHotkeyMonitor>> + Send + 'static,
) {
    let app = inner.app.lock().clone();
    if let Some(app) = app {
        let inner = Arc::clone(inner);
        let _ = app.run_on_main_thread(move || {
            slot(&inner).lock().take();
        });
    } else {
        slot(inner).lock().take();
    }
}

pub(crate) fn take_combo_hotkey_on_main_thread(inner: &Arc<Inner>) {
    take_combo_monitor_on_main_thread(inner, |inner| &inner.combo_hotkey);
}

pub(crate) fn take_translation_hotkey_on_main_thread(inner: &Arc<Inner>) {
    take_combo_monitor_on_main_thread(inner, |inner| &inner.translation_hotkey);
}

pub(crate) fn take_action_hotkey_on_main_thread(inner: &Arc<Inner>, kind: ActionHotkeyKind) {
    let app = inner.app.lock().clone();
    if let Some(app) = app {
        let inner = Arc::clone(inner);
        let _ = app.run_on_main_thread(move || {
            action_hotkey_slot(&inner, kind).lock().take();
        });
    } else {
        action_hotkey_slot(inner, kind).lock().take();
    }
}

pub(crate) fn action_hotkey_slot(
    inner: &Arc<Inner>,
    kind: ActionHotkeyKind,
) -> &Mutex<Option<ComboHotkeyMonitor>> {
    match kind {
        ActionHotkeyKind::SwitchStyle => &inner.switch_style_hotkey,
        ActionHotkeyKind::OpenApp => &inner.open_app_hotkey,
    }
}

pub(crate) fn action_hotkey_binding(
    inner: &Arc<Inner>,
    kind: ActionHotkeyKind,
) -> Option<crate::types::ShortcutBinding> {
    let prefs = inner.prefs.get();
    match kind {
        ActionHotkeyKind::SwitchStyle => prefs.switch_style_hotkey,
        ActionHotkeyKind::OpenApp => prefs.open_app_hotkey,
    }
}

pub(crate) fn is_modifier_only_shortcut(binding: &crate::types::ShortcutBinding) -> bool {
    binding.modifiers.is_empty()
        && (binding.primary.eq_ignore_ascii_case("shift")
            || crate::shortcut_binding::legacy_modifier_trigger(binding).is_some())
}

pub(crate) fn is_unconfigured_shortcut(binding: &crate::types::ShortcutBinding) -> bool {
    binding.primary.trim().is_empty()
}

pub(crate) fn action_hotkey_bridge_thread_name(kind: ActionHotkeyKind) -> &'static str {
    match kind {
        ActionHotkeyKind::SwitchStyle => "openless-switch-style-hotkey-bridge",
        ActionHotkeyKind::OpenApp => "openless-open-app-hotkey-bridge",
    }
}

pub(crate) fn is_builtin_translation_shift(binding: &crate::types::ShortcutBinding) -> bool {
    binding.modifiers.is_empty() && binding.primary.eq_ignore_ascii_case("shift")
}

/// Linux: 从 prefs 读取自定义组合键，同步到 fcitx5 插件。
#[cfg(target_os = "linux")]
pub(crate) fn custom_dictation_key_string(inner: &Arc<Inner>) -> Option<String> {
    let prefs = inner.prefs.get();
    let key_string = crate::linux_fcitx::binding_to_fcitx_key_string(&prefs.dictation_hotkey);
    if key_string.is_empty() {
        None
    } else {
        Some(key_string)
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn sync_custom_dictation_to_plugin(inner: &Arc<Inner>) {
    let prefs = inner.prefs.get();
    let dictation = &prefs.dictation_hotkey;
    let key_string = crate::linux_fcitx::binding_to_fcitx_key_string(dictation);
    if key_string.is_empty() {
        return;
    }
    match crate::linux_fcitx::set_custom_dictation_trigger(&key_string) {
        Ok(()) => log::info!(
            "[fcitx] Synced custom dictation trigger '{}' to plugin",
            key_string
        ),
        Err(e) => log::warn!("[fcitx] Failed to sync custom dictation trigger: {e}"),
    }
}

pub(crate) fn modifier_shortcut_triggers(
    inner: &Arc<Inner>,
) -> (
    Option<crate::types::HotkeyTrigger>,
    Option<crate::types::HotkeyTrigger>,
) {
    let prefs = inner.prefs.get();
    let qa_trigger = prefs
        .qa_hotkey
        .as_ref()
        .and_then(crate::shortcut_binding::legacy_modifier_trigger);
    let translation_trigger = if is_builtin_translation_shift(&prefs.translation_hotkey) {
        None
    } else {
        crate::shortcut_binding::legacy_modifier_trigger(&prefs.translation_hotkey)
    };
    (qa_trigger, translation_trigger)
}

pub(crate) fn mark_translation_modifier_seen(inner: &Arc<Inner>) {
    let phase = inner.state.lock().phase;
    if matches!(phase, SessionPhase::Starting | SessionPhase::Listening) {
        inner
            .translation_modifier_seen
            .store(true, Ordering::SeqCst);
        log::info!("[coord] translation modifier seen during {phase:?}");
    }
}

pub(crate) fn hotkey_bridge_loop(inner: Arc<Inner>, rx: mpsc::Receiver<HotkeyEvent>) {
    while let Ok(evt) = rx.recv() {
        if inner.shortcut_recording_active.load(Ordering::SeqCst) {
            continue;
        }
        let inner_cloned = Arc::clone(&inner);
        match evt {
            // P0 #468/#475: Pressed/Released 必须串行处理，否则在 Windows 上 WH_KEYBOARD_LL
            // 边沿间隔微秒级 → 两个独立 spawn 的 task 被 work-stealing 调度器并行执行 →
            // `hotkey_trigger_held` latch 翻转顺序错乱 → 下次按键被静默吞掉
            // (UI 关不掉 / 录音停不下来)。改为 bridge 线程内 block_on 顺序 await，
            // recv 的 FIFO 顺序就是 handler 执行顺序。
            // 注意：handle_pressed_edge / handle_released_edge 内部走 .await（含网络
            // 握手），会暂时阻塞本 bridge 线程；Hold 模式短按时 Released 会排队在 channel
            // 里直到 begin_session 完成，但 SessionPhase::Starting 已经有
            // request_stop_during_starting 兜底，begin_session 完成进 Listening 后
            // bridge 立刻 recv Released → end_session，行为正确，仅有短暂 stop 延迟。
            HotkeyEvent::Pressed => {
                async_runtime::block_on(async {
                    handle_pressed_edge(&inner_cloned).await;
                });
            }
            HotkeyEvent::Released => {
                async_runtime::block_on(async {
                    handle_released_edge(&inner_cloned).await;
                });
            }
            HotkeyEvent::Cancelled => {
                cancel_session(&inner_cloned);
            }
            HotkeyEvent::TranslationModifierPressed => {
                let translation_hotkey = inner_cloned.prefs.get().translation_hotkey;
                if is_builtin_translation_shift(&translation_hotkey)
                    || crate::shortcut_binding::legacy_modifier_trigger(&translation_hotkey)
                        .is_some()
                {
                    mark_translation_modifier_seen(&inner_cloned);
                }
            }
            HotkeyEvent::QaShortcutPressed => {
                async_runtime::spawn(async move { handle_qa_hotkey_pressed(&inner_cloned).await });
            }
        }
    }
}

pub(crate) fn reset_shortcut_held_state(inner: &Arc<Inner>) {
    inner.hotkey_trigger_held.store(false, Ordering::SeqCst);
    if let Some(monitor) = inner.hotkey.lock().as_ref() {
        monitor.reset_held_state();
    }
    let prefs = inner.prefs.get();
    if let Some(binding) = prefs.qa_hotkey.as_ref() {
        if crate::shortcut_binding::legacy_modifier_trigger(binding).is_none() {
            if let Some(monitor) = inner.qa_hotkey.lock().as_ref() {
                if let Err(e) = monitor.update_binding(binding.clone()) {
                    log::warn!("[coord] reset QA hotkey latch failed: {e}");
                }
            }
        }
    }
    if !is_builtin_translation_shift(&prefs.translation_hotkey)
        && crate::shortcut_binding::legacy_modifier_trigger(&prefs.translation_hotkey).is_none()
    {
        if let Some(monitor) = inner.translation_hotkey.lock().as_ref() {
            if let Err(e) = monitor.update_binding(prefs.translation_hotkey.clone()) {
                log::warn!("[coord] reset translation hotkey latch failed: {e}");
            }
        }
    }
    if let Some(switch_style) = prefs.switch_style_hotkey.as_ref() {
        if !is_modifier_only_shortcut(switch_style) {
            if let Some(monitor) = inner.switch_style_hotkey.lock().as_ref() {
                if let Err(e) = monitor.update_binding(switch_style.clone()) {
                    log::warn!("[coord] reset switch-style hotkey latch failed: {e}");
                }
            }
        }
    }
    if let Some(open_app) = prefs.open_app_hotkey.as_ref() {
        if !is_modifier_only_shortcut(open_app) {
            if let Some(monitor) = inner.open_app_hotkey.lock().as_ref() {
                if let Err(e) = monitor.update_binding(open_app.clone()) {
                    log::warn!("[coord] reset open-app hotkey latch failed: {e}");
                }
            }
        }
    }
}

pub(crate) async fn handle_window_hotkey_event(
    inner: &Arc<Inner>,
    event_type: String,
    key: String,
    code: String,
    repeat: bool,
) -> Result<(), String> {
    if inner.shortcut_recording_active.load(Ordering::SeqCst) {
        return Ok(());
    }
    if event_type == "keydown" && key == "Escape" {
        // Esc 路由（issue #161）：QA 浮窗可见时优先取消 QA（不动 dictation）；
        // 否则走 dictation 取消通路。之前无条件 cancel_session 导致 QA 浮窗
        // 按 Esc 杀的是 dictation 而 QA 流还在烧 token。
        let qa_active = {
            let st = inner.qa_state.lock();
            st.panel_visible || st.phase != QaPhase::Idle
        };
        if qa_active {
            close_qa_panel(inner);
        } else {
            cancel_session(inner);
        }
        return Ok(());
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (inner, event_type, key, code, repeat);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        if !window_hotkey_fallback_enabled() {
            if event_type == "keydown" && !repeat {
                log::info!(
                    "[window-hotkey] ignored because Windows lifecycle owner is the low-level hook"
                );
            }
            return Ok(());
        }

        let Some(trigger) =
            crate::shortcut_binding::legacy_modifier_trigger(&inner.prefs.get().dictation_hotkey)
        else {
            return Ok(());
        };
        if !window_key_matches_trigger(trigger, &key, &code) {
            return Ok(());
        }

        match event_type.as_str() {
            "keydown" => {
                if repeat {
                    return Ok(());
                }
                log::info!(
                    "[window-hotkey] pressed trigger={trigger:?} code={code} repeat={repeat}"
                );
                handle_pressed_edge(inner).await;
            }
            "keyup" => {
                log::info!("[window-hotkey] released trigger={trigger:?} code={code}");
                handle_released_edge(inner).await;
            }
            _ => {}
        }
        Ok(())
    }
}

pub(crate) fn window_hotkey_fallback_enabled() -> bool {
    crate::types::HotkeyCapability::current().explicit_fallback_available
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn window_key_matches_trigger(
    trigger: crate::types::HotkeyTrigger,
    key: &str,
    code: &str,
) -> bool {
    use crate::types::HotkeyTrigger;

    match trigger {
        HotkeyTrigger::RightControl => key == "Control" && code == "ControlRight",
        HotkeyTrigger::LeftControl => key == "Control" && code == "ControlLeft",
        HotkeyTrigger::RightOption | HotkeyTrigger::RightAlt => {
            (key == "Alt" || key == "AltGraph") && code == "AltRight"
        }
        HotkeyTrigger::LeftOption => (key == "Alt" || key == "AltGraph") && code == "AltLeft",
        HotkeyTrigger::RightCommand => key == "Meta" && code == "MetaRight",
        HotkeyTrigger::Fn => key == "Control" && code == "ControlRight",
        // MediaPlayPause 走 WH_KEYBOARD_LL，不走 window hotkey fallback
        HotkeyTrigger::MediaPlayPause => false,
        // Custom 走 global-hotkey crate，不走 window hotkey fallback
        HotkeyTrigger::Custom => false,
    }
}
