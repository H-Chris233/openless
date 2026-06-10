//! Capsule HUD 呈现层：显示策略（no-activate / fallback）、emit_capsule 状态广播、
//! 布局快照与底部居中定位、自动回 Idle 调度。
//!
//! 从 `coordinator.rs` 机械拆出（行为保持，仅可见性提升）。emit_capsule 由 cpal 音频
//! 回调线程 ~30Hz 调用，run_on_main_thread marshaling 原样保留（SIGTRAP 规避）。

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapsuleShowStrategy {
    NoActivate,
    FallbackShow,
}

pub(crate) fn capsule_show_strategy_for_platform() -> CapsuleShowStrategy {
    // ⚠️ 如果改下面的 cfg 列表，**必须**同步更新单元测试
    // `capsule_show_strategy_matches_platform_activation_contract` 的两组 cfg —
    // 否则 Linux CI 直接红（PR #451 即是这种漏改）。
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        CapsuleShowStrategy::NoActivate
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        CapsuleShowStrategy::FallbackShow
    }
}

static CAPSULE_NO_ACTIVATE_FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);
static CAPSULE_SUPPRESSED_BY_TOGGLE_LOGGED: AtomicBool = AtomicBool::new(false);
static CAPSULE_FIRST_SHOW_LOGGED: AtomicBool = AtomicBool::new(false);
// issue #631：上一次应用到胶囊窗口的点击穿透值。初始 false 与窗口创建时一致
// （tauri.conf.json 未设 ignore），按变化去重，避免录音中 ~30Hz 电平帧重复调系统 API。
static CAPSULE_IGNORE_CURSOR_APPLIED: AtomicBool = AtomicBool::new(false);
// #470 诊断 v2：capsule webview 句柄取不到时的一次性门，区分「窗口压根没创建」(A0)。
static CAPSULE_WINDOW_MISSING_LOGGED: AtomicBool = AtomicBool::new(false);

/// 给 #470 诊断日志用的 capsule 状态短名。显式枚举每个变体到 &'static str，
/// 不走 `Debug` —— 哪天 CapsuleState 加了 `String` 字段，`:?` 会把 ASR / polish
/// 内容意外灌进日志（pr_agent 提的 forward-looking 隐患）；这里只输出状态名。
pub(crate) fn capsule_state_log_name(state: CapsuleState) -> &'static str {
    match state {
        CapsuleState::Idle => "idle",
        CapsuleState::Recording => "recording",
        CapsuleState::Transcribing => "transcribing",
        CapsuleState::Polishing => "polishing",
        CapsuleState::Done => "done",
        CapsuleState::Cancelled => "cancelled",
        CapsuleState::Error => "error",
    }
}

pub(crate) fn show_capsule_window_for_recording<R: tauri::Runtime>(
    app: &AppHandle<R>,
    window: &tauri::WebviewWindow<R>,
) {
    let mut needs_fallback = true;
    if capsule_show_strategy_for_platform() == CapsuleShowStrategy::NoActivate {
        needs_fallback = !show_capsule_window_no_activate(app, window);
        if needs_fallback && !CAPSULE_NO_ACTIVATE_FALLBACK_WARNED.swap(true, Ordering::SeqCst) {
            // 产品取舍：no-activate 是 macOS/AeroSpace 的主路径；但如果 ns_window
            // 暂不可用，仍优先保住录音反馈，不让用户以为听写没启动。fallback 可能
            // 重新触发 workspace 跳转，只在 no-activate 失败时作为降级路径。
            log::warn!("[capsule] no-activate show failed; falling back to window.show()");
        }
    }

    if needs_fallback {
        if let Err(e) = window.show() {
            log::warn!("[capsule] show fallback failed: {e}");
        }
    }
}

/// issue #631：该状态下胶囊窗口是否应忽略鼠标事件（点击穿透到下层应用）。
/// 胶囊窗口 220×110 远大于可见 pill，贴近输入框时透明区域会吃掉用户点击并激活
/// OpenLess（误触弹出主界面）。终态（Done/Cancelled/Error）与 Idle 没有可交互
/// 按钮——包括 2s toast 停留和离场动画期间——让点击穿透；录音/转写/润色态有
/// ✕/✓ 按钮，保持可点。
pub(crate) fn capsule_ignore_cursor_for_state(state: CapsuleState) -> bool {
    !matches!(
        state,
        CapsuleState::Recording | CapsuleState::Transcribing | CapsuleState::Polishing
    )
}

/// 终止态（Done / Cancelled / Error）后延迟 N ms 把胶囊改回 Idle，让浮窗自动消失。
/// 用户点 ✕ / ✓ / 中途出错 / 按 Esc 都走这里，统一 2 秒。
pub(crate) const CAPSULE_AUTO_HIDE_DELAY_MS: u64 = 2000;

/// Toggle 模式下，end_session 将 phase 设为 Idle 后在此时间内禁止新的 begin_session。
/// 避免用户三连按时第 3 次按下误激活新听写（此时胶囊仍在离场动画周期内）。
/// 值取 capsule EXIT_ANIM_MS (360ms) + 余量 ≈ 600ms。
pub(crate) const POST_SESSION_COOLDOWN_MS: u64 = 600;

pub(crate) fn schedule_capsule_idle(inner: &Arc<Inner>, delay_ms: u64) {
    let inner_clone = Arc::clone(inner);
    async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        // 必须 dictation **和** QA 同时空闲才能隐藏胶囊。否则旧 dictation Done timer
        // 的尾巴会在新 QA 录音/思考中把胶囊意外收掉（issue #118 v2 复现）。
        let dictation_idle = inner_clone.state.lock().phase == SessionPhase::Idle;
        let qa_idle = inner_clone.qa_state.lock().phase == QaPhase::Idle;
        if dictation_idle && qa_idle {
            emit_capsule(&inner_clone, CapsuleState::Idle, 0.0, 0, None, None);
        }
    });
}

pub(crate) fn emit_capsule(
    inner: &Arc<Inner>,
    state: CapsuleState,
    level: f32,
    elapsed_ms: u64,
    message: Option<String>,
    inserted_chars: Option<u32>,
) {
    // 在 app 句柄校验之前记录，便于无 GUI 的测试断言「按下热键 → 弹了哪种胶囊」。
    *inner.last_capsule_state.lock() = Some(state);
    let app_opt = inner.app.lock().clone();
    let Some(app) = app_opt else { return };
    let translation = inner.translation_modifier_seen.load(Ordering::SeqCst);
    let operating = inner.state.lock().voice_agent;
    let payload = CapsulePayload {
        state,
        level,
        elapsed_ms,
        message,
        inserted_chars,
        translation,
        operating,
    };

    // visible / translation 是「这一帧 capsule:state event 的 payload」内容 ——
    // 必须在 call-site（即音频线程触发 emit_capsule 时）就算定，否则 main thread
    // 闭包里读到的将是「下一帧」的 state，跟实际下发给 JS 的 payload 不一致。
    let visible = !matches!(state, CapsuleState::Idle);

    // Linux: 通过 fcitx5 插件在候选词列表下方显示听写状态，不干扰输入法预编辑。
    // 只在文本变化时调用 DBus，避免录音中 ~30Hz 的音频电平回调重复调用。
    #[cfg(target_os = "linux")]
    {
        use std::sync::Mutex;
        static LAST_AUX: Mutex<Option<String>> = Mutex::new(None);

        let aux = match state {
            CapsuleState::Idle => None,
            CapsuleState::Recording => Some("🎤 收音中..."),
            CapsuleState::Transcribing => Some("🔄 识别中..."),
            CapsuleState::Polishing => Some("✨ 润色中..."),
            CapsuleState::Done => Some("✅ 已插入"),
            CapsuleState::Cancelled => Some("— 已取消"),
            CapsuleState::Error => Some("❌ 出错"),
        };

        let mut last = LAST_AUX.lock().unwrap();
        if aux != last.as_deref() {
            *last = aux.map(String::from);
            // 代数计数器：每次状态变化 +1，retry 线程只在自己代数仍为最新时生效。
            // 避免 Recording→Idle→Recording 快速切换时多个 retry 重复触发。
            static RETRY_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            // fetch_add 返回旧值，所以 latest_gen > gen+1 才表示"在我之后又发生了变更"。
            let gen = RETRY_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match aux {
                Some(t) => {
                    log::info!("[capsule] set_aux_down: {t} gen={gen}");
                    let text = t.to_string();
                    std::thread::spawn(move || {
                        let current = LAST_AUX.lock().unwrap().clone();
                        if current.as_deref() != Some(&text) {
                            log::info!(
                                "[capsule] set_aux_down skipped: state changed to {current:?}"
                            );
                            return;
                        }
                        if let Err(e) = crate::linux_fcitx::set_aux_down(&text) {
                            log::warn!("[capsule] set_aux_down failed: {e}");
                        }
                    });
                    // 终态（Done/Cancelled/Error）3 秒后自动清除，避免一直跟随焦点。
                    if matches!(
                        state,
                        CapsuleState::Done | CapsuleState::Cancelled | CapsuleState::Error
                    ) {
                        let text = t.to_string();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            let latest_gen = RETRY_GEN.load(std::sync::atomic::Ordering::SeqCst);
                            if latest_gen > gen + 1 {
                                return;
                            }
                            let current = LAST_AUX.lock().unwrap().clone();
                            if current.as_deref() != Some(&text) {
                                return;
                            }
                            log::info!("[capsule] auto-clear terminal state: {text}");
                            let _ = crate::linux_fcitx::set_aux_down("");
                            *LAST_AUX.lock().unwrap() = None;
                        });
                    }
                }
                None => {
                    log::info!("[capsule] clear_aux_down gen={gen}");
                    std::thread::spawn(move || {
                        let latest_gen = RETRY_GEN.load(std::sync::atomic::Ordering::SeqCst);
                        if latest_gen > gen + 1 {
                            log::info!(
                                "[capsule] clear_aux_down skipped: gen {gen}, latest {latest_gen}"
                            );
                            return;
                        }
                        let current = LAST_AUX.lock().unwrap().clone();
                        if current.is_some() {
                            log::info!(
                                "[capsule] clear_aux_down skipped: state changed to {current:?}"
                            );
                            return;
                        }
                        if let Err(e) = crate::linux_fcitx::clear_aux_down() {
                            log::warn!("[capsule] clear_aux_down failed: {e}");
                        }
                    });
                }
            }
        }
    }

    // emit_capsule 会被 cpal process_callback（音频回调线程）调用 ~30 Hz —— 在该
    // 线程上调用 NSWindow / HWND API 会撞 macOS dispatch_assert_queue_fail SIGTRAP
    // 或者 Win32 SendMessage 死锁。把 window.show/hide + 位置调整 marshal 到主线程；
    // app.emit_to 走 Tauri 内部事件总线，本身线程安全，保留同步调用。详见 audit 3.2.2。
    //
    // show_capsule（用户偏好）在主线程执行时再读 —— 用户可以在录音过程中改设置，
    // 闭包入队到真正跑之间窗口上限是一两帧（~16-33ms），用最新值消除 stale-pref
    // 闪烁。pr_agent 关注点 — 见 audit follow-up。
    let inner_for_main = Arc::clone(inner);
    let app_for_main = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(window) = app_for_main.get_webview_window("capsule") else {
            // #470 诊断 v2：比 A/B/C 更靠前的暗点 A0 —— capsule webview 句柄取不到
            // （窗口未创建/已销毁）。此前静默 return，无法观测。一次性 warn。
            if !CAPSULE_WINDOW_MISSING_LOGGED.swap(true, Ordering::SeqCst) {
                log::warn!(
                    "[capsule] capsule webview window not found — emit_capsule show path skipped (state={})",
                    capsule_state_log_name(state)
                );
            }
            return;
        };
        let show_capsule = inner_for_main.prefs.get().show_capsule;
        // Linux: 不操作胶囊窗口（不 show/hide，不 reposition）。
        // 文字通过 fcitx5 插件直接 commit，用户始终在目标 app 中。
        #[cfg(target_os = "linux")]
        {
            return;
        }
        #[cfg(not(target_os = "linux"))]
        {

        // issue #631：终态/空闲让胶囊点击穿透，录音完成后用户点击贴近的输入框
        // 不再误触胶囊激活 OpenLess。状态变化时才真正调系统 API。
        let ignore_cursor = capsule_ignore_cursor_for_state(state);
        if CAPSULE_IGNORE_CURSOR_APPLIED.swap(ignore_cursor, Ordering::SeqCst) != ignore_cursor {
            let _ = window.set_ignore_cursor_events(ignore_cursor);
        }

        // 三平台统一：Done / Cancelled / Error 状态保留 ~1.5s toast
        // （schedule_capsule_idle 之后会回 Idle 隐藏）。
        // Windows 上 linger 的真实问题（截图选中 / 死区 / 拖拽卡顿）由 #140 加的
        // `hide_capsule_window_if_present()` Win32 hard-hide 在 visible=false 分支
        // 处理，不依赖把 Done/Cancelled/Error 打成 invisible。详见 PR #140 评论。
        maybe_position_capsule_bottom_center(&inner_for_main, &window, translation);
        if show_capsule && visible {
            // 用户报"看不到胶囊"时第一时间能在 log 里确认：胶囊路径有跑、show_capsule
            // 开关是 true、当前进入 visible 帧 —— 排除 prefs 没存住 / emit_capsule 没触
            // 发 / state 一直 Idle 这几类常见 root cause。issue #470。
            if !CAPSULE_FIRST_SHOW_LOGGED.swap(true, Ordering::SeqCst) {
                log::info!(
                    "[capsule] first show this session: show_capsule=true visible=true state={}",
                    capsule_state_log_name(state)
                );
            }
            show_capsule_window_for_recording(&app_for_main, &window);
            // macOS/Windows 优先走 no-activate show，避免录音胶囊抢走当前工作 app 焦点。
            // 若 fallback 到 show()，OpenLess 已是前台 app 时再把 key window 还给 main。
            #[cfg(target_os = "macos")]
            crate::restore_main_window_key_if_active(&app_for_main);
        } else {
            // show_capsule 开关被用户关掉但本次确实想显示（visible=true）的情况：
            // 一次性 info log，让用户报"胶囊没显示"时能在日志里一眼看到根因 —— 维护者
            // 不必再让用户"去打开设置确认"。issue #470。
            if !show_capsule
                && visible
                && !CAPSULE_SUPPRESSED_BY_TOGGLE_LOGGED.swap(true, Ordering::SeqCst)
            {
                log::info!(
                    "[capsule] suppressed by user toggle: show_capsule=false visible=true state={}",
                    capsule_state_log_name(state)
                );
            }
            hide_capsule_window_if_present();
            let _ = window.hide();
        }
        }
    });

    let _ = app.emit_to("capsule", "capsule:state", &payload);
    // 主窗口也需要 capsule:state 事件：AudioCueListener 用它触发录音提示音。
    // Linux 上胶囊隐藏时提示音仍应工作，所以同时发给 main 窗口。
    let _ = app.emit_to("main", "capsule:state", &payload);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CapsuleLayoutState {
    translation_active: bool,
    monitor_x: i32,
    monitor_y: i32,
    monitor_width: u32,
    monitor_height: u32,
    scale_bits: u64,
}

/// 返回胶囊「应该摆放到的显示器」的标识信息。
///
/// 它看的显示器必须和 `position_capsule_bottom_center` 实际定位用的一致：
/// Windows 看「正在输入的 App 所在显示器」，其它平台看胶囊自己的显示器。
/// 这是「是否需要重新定位」去重缓存（`maybe_position_capsule_bottom_center`）
/// 的 key，如果这里看错了显示器，就会出现「输入焦点移到另一块屏、胶囊却没
/// 跟过去」的 bug。
pub(crate) fn capsule_layout_snapshot<R: tauri::Runtime>(
    window: &tauri::WebviewWindow<R>,
    translation_active: bool,
) -> Option<CapsuleLayoutState> {
    // Windows：以「正在输入的 App 所在显示器」为基准。若用胶囊自己的
    // current_monitor，输入焦点切到另一块屏时胶囊仍在原屏 → 误判「没变化」
    // → 跳过重新定位。
    #[cfg(target_os = "windows")]
    {
        if let Some(mon) = crate::foreground_window_monitor() {
            return Some(CapsuleLayoutState {
                translation_active,
                monitor_x: mon.left,
                monitor_y: mon.top,
                monitor_width: (mon.right - mon.left).max(0) as u32,
                monitor_height: (mon.bottom - mon.top).max(0) as u32,
                scale_bits: mon.scale.to_bits(),
            });
        }
        // 仅当 Win32 取不到前台显示器时，落回下面的 current_monitor。
    }
    let monitor = window.current_monitor().ok().flatten()?;
    Some(CapsuleLayoutState {
        translation_active,
        monitor_x: monitor.position().x,
        monitor_y: monitor.position().y,
        monitor_width: monitor.size().width,
        monitor_height: monitor.size().height,
        scale_bits: monitor.scale_factor().to_bits(),
    })
}

pub(crate) fn maybe_position_capsule_bottom_center<R: tauri::Runtime>(
    inner: &Arc<Inner>,
    window: &tauri::WebviewWindow<R>,
    translation_active: bool,
) {
    let Some(next) = capsule_layout_snapshot(window, translation_active) else {
        return;
    };
    {
        let last = inner.capsule_layout.lock();
        if last.as_ref() == Some(&next) {
            return;
        }
    }
    if crate::position_capsule_bottom_center(window, translation_active).is_ok() {
        let mut last = inner.capsule_layout.lock();
        *last = Some(next);
    }
}
