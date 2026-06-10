//! OS 级窗口 & 插入管线：Windows IME 会话（store/take/restore）、TSF 优先插入、
//! 非 TSF 回退，以及焦点目标抓取/恢复、前台 App 抓取、capsule 窗口 no-activate 显示/隐藏。
//!
//! 从 `coordinator.rs` 机械拆出（行为保持，仅可见性提升）。大量 unsafe + Win32/objc2 FFI，
//! 所有 cfg 分支与 SAFETY/issue 注释原样跟随。

use super::*;

#[cfg(target_os = "windows")]
pub(crate) fn store_prepared_windows_ime_session(
    slots: &mut Vec<PreparedWindowsImeSessionSlot>,
    session_id: SessionId,
    prepared: PreparedWindowsImeSession,
) {
    slots.retain(|slot| slot.session_id != session_id);
    slots.push(PreparedWindowsImeSessionSlot {
        session_id,
        prepared,
    });
}

#[cfg(target_os = "windows")]
pub(crate) fn take_matching_prepared_windows_ime_session(
    slots: &mut Vec<PreparedWindowsImeSessionSlot>,
    session_id: SessionId,
) -> Option<PreparedWindowsImeSession> {
    let index = slots
        .iter()
        .position(|slot| slot.session_id == session_id)?;
    Some(slots.remove(index).prepared)
}

#[cfg(target_os = "windows")]
pub(crate) fn take_current_prepared_windows_ime_session_for_restore(
    slots: &mut Vec<PreparedWindowsImeSessionSlot>,
    session_id: SessionId,
    current_session_id: SessionId,
) -> Option<PreparedWindowsImeSession> {
    let prepared = take_matching_prepared_windows_ime_session(slots, session_id)?;
    if current_session_id == session_id {
        Some(prepared)
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn restore_prepared_windows_ime_session(inner: &Arc<Inner>, session_id: SessionId) {
    let state = inner.state.lock();
    let prepared = {
        let mut slot = inner.prepared_windows_ime_session.lock();
        take_current_prepared_windows_ime_session_for_restore(
            &mut slot,
            session_id,
            state.session_id,
        )
    };
    if let Some(prepared) = prepared {
        inner.windows_ime.restore_session(prepared);
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn restore_prepared_windows_ime_session(_inner: &Arc<Inner>, _session_id: SessionId) {}

#[cfg(target_os = "windows")]
pub(crate) async fn insert_with_windows_ime_first(
    inner: &Arc<Inner>,
    session_id: SessionId,
    polished: &str,
    restore_clipboard: bool,
    allow_non_tsf_insertion_fallback: bool,
    paste_shortcut: PasteShortcut,
    ime_target: Option<ImeSubmitTarget>,
) -> InsertStatus {
    let prepared = {
        let mut slot = inner.prepared_windows_ime_session.lock();
        take_matching_prepared_windows_ime_session(&mut slot, session_id)
    };
    let Some(prepared) = prepared else {
        log::warn!("[windows-ime] no prepared TSF session for this dictation");
        if should_try_non_tsf_insertion_fallback(
            allow_non_tsf_insertion_fallback,
            InsertStatus::Failed,
        ) {
            return insert_via_non_tsf_fallback(inner, polished, restore_clipboard, paste_shortcut);
        }
        log::warn!("[windows-ime] non-TSF insertion fallback is disabled; failing insert");
        return InsertStatus::Failed;
    };

    let request = crate::windows_ime_ipc::ImeSubmitRequest {
        session_id: Uuid::new_v4().to_string(),
        text: polished.to_string(),
        created_at: Utc::now().to_rfc3339(),
        target: ime_target,
    };

    let ime_status = match inner.windows_ime.submit_prepared(&prepared, request).await {
        Ok(status) => status,
        Err(error) => {
            log::warn!("[windows-ime] TSF submit failed: {error}");
            InsertStatus::Failed
        }
    };
    inner.windows_ime.restore_session(prepared);

    if ime_status == InsertStatus::Inserted {
        ime_status
    } else if should_try_non_tsf_insertion_fallback(allow_non_tsf_insertion_fallback, ime_status) {
        insert_via_non_tsf_fallback(inner, polished, restore_clipboard, paste_shortcut)
    } else {
        log::warn!("[windows-ime] TSF did not insert; non-TSF insertion fallback is disabled");
        InsertStatus::Failed
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn should_try_non_tsf_insertion_fallback(
    allow_non_tsf_insertion_fallback: bool,
    ime_status: InsertStatus,
) -> bool {
    allow_non_tsf_insertion_fallback && ime_status != InsertStatus::Inserted
}

#[cfg(target_os = "windows")]
pub(crate) fn insert_via_non_tsf_fallback(
    inner: &Arc<Inner>,
    polished: &str,
    _restore_clipboard: bool,
    _paste_shortcut: PasteShortcut,
) -> InsertStatus {
    let status = finish_non_tsf_insertion_fallback(
        || inner.inserter.insert_via_unicode_keystrokes(polished),
        || inner.inserter.copy_fallback(polished),
    );

    match status {
        InsertStatus::Inserted => {
            log::warn!(
                "[windows-ime] TSF unavailable; inserted via paced Unicode SendInput fallback"
            );
        }
        InsertStatus::CopiedFallback => {
            log::warn!(
                "[windows-ime] TSF unavailable; Unicode SendInput failed, left text on clipboard"
            );
        }
        InsertStatus::PasteSent | InsertStatus::Failed => {
            log::warn!(
                "[windows-ime] TSF unavailable; Unicode SendInput fallback failed and copy fallback failed"
            );
        }
    }

    status
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn finish_non_tsf_insertion_fallback<U, C>(
    mut unicode_fallback: U,
    mut copy_only_fallback: C,
) -> InsertStatus
where
    U: FnMut() -> InsertStatus,
    C: FnMut() -> InsertStatus,
{
    match unicode_fallback() {
        InsertStatus::Inserted => InsertStatus::Inserted,
        InsertStatus::PasteSent | InsertStatus::CopiedFallback | InsertStatus::Failed => {
            match copy_only_fallback() {
                InsertStatus::CopiedFallback => InsertStatus::CopiedFallback,
                // TextInserter::copy_fallback is copy-only: success is CopiedFallback.
                // Treat any other status as failure so this helper never invents an insert.
                InsertStatus::Inserted | InsertStatus::PasteSent | InsertStatus::Failed => {
                    InsertStatus::Failed
                }
            }
        }
    }
}

#[cfg(test)]
mod non_tsf_fallback_tests {
    use super::finish_non_tsf_insertion_fallback;
    use crate::types::InsertStatus;

    #[test]
    fn unicode_fallback_runs_before_copy_fallback() {
        let mut copy_called = false;
        let status = finish_non_tsf_insertion_fallback(
            || InsertStatus::Inserted,
            || {
                copy_called = true;
                InsertStatus::CopiedFallback
            },
        );

        assert_eq!(status, InsertStatus::Inserted);
        assert!(!copy_called);
    }

    #[test]
    fn copy_fallback_runs_after_unicode_failure() {
        let mut copy_called = false;
        let status = finish_non_tsf_insertion_fallback(
            || InsertStatus::Failed,
            || {
                copy_called = true;
                InsertStatus::CopiedFallback
            },
        );

        assert_eq!(status, InsertStatus::CopiedFallback);
        assert!(copy_called);
    }

    #[test]
    fn double_failure_does_not_pretend_text_was_copied() {
        let mut copy_called = false;
        let status = finish_non_tsf_insertion_fallback(
            || InsertStatus::Failed,
            || {
                copy_called = true;
                InsertStatus::Failed
            },
        );

        assert_eq!(status, InsertStatus::Failed);
        assert!(copy_called);
    }
}

/// 与 capture_focus_target 类似，但前台窗口属于本进程（即用户停在 QA / capsule / main
/// 等自家窗口）时返回 None，让 caller 区分"用户没切到别处" vs "用户切到了另一个真正的
/// 外部 app"。issue #466 多轮场景下用来刷新 qa_focus_target。
#[cfg(target_os = "windows")]
pub(crate) fn capture_external_focus_target() -> Option<usize> {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == GetCurrentProcessId() {
            return None;
        }
        Some(hwnd.0 as usize)
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn capture_external_focus_target() -> Option<usize> {
    None
}

#[cfg(target_os = "windows")]
pub(crate) fn capture_focus_target() -> Option<usize> {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        None
    } else {
        Some(foreground.0 as usize)
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn capture_focus_target() -> Option<usize> {
    None
}

/// 捕获用户开始 dictation 时的前台 app 标签（"localizedName (bundle.id)"），用作 LLM
/// polish/translate 的上下文前提，让模型按 app 调风格。详见 issue #116。
///
/// macOS 走 NSWorkspace.frontmostApplication（公开 API，无需额外权限）；
/// Windows 复用前台 HWND 拿窗口标题；Linux/其他平台返回 None。
#[cfg(target_os = "macos")]
pub(crate) fn capture_frontmost_app() -> Option<String> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    unsafe {
        let cls = AnyClass::get("NSWorkspace")?;
        let workspace: *mut AnyObject = msg_send![cls, sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let app: *mut AnyObject = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let name_obj: *mut AnyObject = msg_send![app, localizedName];
        let bundle_obj: *mut AnyObject = msg_send![app, bundleIdentifier];
        let name = nsstring_to_string(name_obj);
        let bundle = nsstring_to_string(bundle_obj);
        match (name, bundle) {
            (Some(n), Some(b)) => Some(format!("{n} ({b})")),
            (Some(n), None) => Some(n),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) unsafe fn nsstring_to_string(
    ns_string: *mut objc2::runtime::AnyObject,
) -> Option<String> {
    use objc2::msg_send;
    if ns_string.is_null() {
        return None;
    }
    let utf8: *const std::os::raw::c_char = unsafe { msg_send![ns_string, UTF8String] };
    if utf8.is_null() {
        return None;
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(utf8) };
    let s = cstr.to_string_lossy().into_owned();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn capture_frontmost_app() -> Option<String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    };

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return None;
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let copied = GetWindowTextW(hwnd, &mut buf);
        if copied <= 0 {
            return None;
        }
        let title = String::from_utf16_lossy(&buf[..copied as usize]);
        if title.is_empty() {
            None
        } else {
            Some(title)
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn capture_frontmost_app() -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
pub(crate) fn restore_focus_target_if_possible(target: Option<usize>) -> bool {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IsIconic, IsWindow, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    let Some(raw_target) = target else {
        log::warn!("[coord] no original Windows insertion target captured");
        return false;
    };
    let hwnd = HWND(raw_target as *mut c_void);
    if hwnd.0.is_null() {
        return false;
    }
    if !unsafe { IsWindow(hwnd).as_bool() } {
        log::warn!("[coord] original Windows insertion target is no longer a valid window");
        return false;
    }

    let foreground = unsafe { GetForegroundWindow() };
    if foreground == hwnd {
        return true;
    }

    if unsafe { IsIconic(hwnd).as_bool() } {
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    }
    let _ = unsafe { SetForegroundWindow(hwnd) };
    std::thread::sleep(std::time::Duration::from_millis(60));

    let foreground = unsafe { GetForegroundWindow() };
    if foreground != hwnd {
        log::warn!("[coord] failed to restore original Windows insertion target before paste");
        return false;
    }
    true
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn restore_focus_target_if_possible(_target: Option<usize>) -> bool {
    true
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_hwnd_is_present(hwnd: windows::Win32::Foundation::HWND) -> bool {
    hwnd != windows::Win32::Foundation::HWND::default()
}

#[cfg(target_os = "windows")]
pub(crate) fn capture_ime_submit_target() -> Option<ImeSubmitTarget> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    };

    let foreground = unsafe { GetForegroundWindow() };
    if !windows_hwnd_is_present(foreground) {
        return None;
    }

    let mut foreground_process_id = 0;
    let foreground_thread_id =
        unsafe { GetWindowThreadProcessId(foreground, Some(&mut foreground_process_id)) };
    if foreground_thread_id == 0 {
        return None;
    }

    let mut gui_info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let target_window = if unsafe { GetGUIThreadInfo(foreground_thread_id, &mut gui_info).is_ok() }
        && windows_hwnd_is_present(gui_info.hwndFocus)
    {
        gui_info.hwndFocus
    } else {
        foreground
    };

    let mut process_id = 0;
    let thread_id = unsafe { GetWindowThreadProcessId(target_window, Some(&mut process_id)) };
    if process_id == 0 || thread_id == 0 {
        return None;
    }

    Some(ImeSubmitTarget {
        process_id,
        thread_id,
    })
}

// Windows topmost overlay 的已知 OS 级限制（issue #457）：
// `SetWindowPos(HWND_TOPMOST)` 让 capsule 在普通桌面合成、最大化窗口、borderless
// windowed fullscreen 上正常叠加；但**对独占全屏（exclusive fullscreen）DirectX /
// OpenGL 应用无效** —— 那条路径绕过桌面合成器，标准 topmost 窗口不参与合成 →
// 用户看不见 capsule。这是 OS 层面的限制，用户空间无法绕过（除非接入 DirectX
// overlay，工程量与风险都不在 surgical 修复范围内）。
//
// 用户侧 workaround：把游戏切到 borderless windowed fullscreen（Minecraft Java 默认
// 即是；F11 在不同版本表现不一致，按设置里的「全屏」选项决定）。
//
// 相关 UIPI 限制：若游戏以管理员身份运行而 OpenLess 不是，`WH_KEYBOARD_LL` 收不到
// 游戏的按键 → hotkey 完全不触发。这里跟 SetWindowPos 路径无关，但同源不可绕过。
#[cfg(target_os = "windows")]
pub(crate) fn show_capsule_window_no_activate<R: tauri::Runtime>(
    _app: &AppHandle<R>,
    window: &tauri::WebviewWindow<R>,
) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_SHOWWINDOW, SW_SHOWNOACTIVATE,
    };

    // #470：首帧 show 时 webview 句柄常常还没 realize（window_handle() 暂不可用），
    // 此前一拿不到就 return false → 回落到会抢焦点的 window.show()，正是「胶囊不显示 /
    // 焦点 churn」的主嫌。改为短暂有界重试：最多 5 次、每次 18ms（总 < 100ms），拿到
    // Win32 句柄就走 no-activate 正常路径；全部失败才让调用方回落 show()。
    // 本函数在 run_on_main_thread 闭包里同步执行（见 capsule.rs），短暂阻塞主线程是
    // 可接受代价 —— 否则那一帧本就要走 show() 抢焦点。
    const HANDLE_RETRY_ATTEMPTS: u32 = 5;
    const HANDLE_RETRY_INTERVAL_MS: u64 = 18;
    let mut hwnd: Option<HWND> = None;
    for attempt in 0..HANDLE_RETRY_ATTEMPTS {
        match window.window_handle() {
            Ok(handle) => match handle.as_raw() {
                RawWindowHandle::Win32(raw) => {
                    hwnd = Some(HWND(raw.hwnd.get() as *mut _));
                    break;
                }
                _ => {
                    // 非 Win32 句柄不会随重试变化，直接放弃。
                    log::warn!(
                        "[capsule] no_activate failed: non-Win32 RawWindowHandle — Win32 show skipped"
                    );
                    return false;
                }
            },
            Err(_) if attempt + 1 < HANDLE_RETRY_ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(HANDLE_RETRY_INTERVAL_MS));
            }
            Err(e) => {
                // #470：重试耗尽仍拿不到句柄，记录后让调用方回落 show()。
                log::warn!(
                    "[capsule] no_activate failed: window_handle() unavailable after {HANDLE_RETRY_ATTEMPTS} retries ({e}) — Win32 show skipped"
                );
                return false;
            }
        }
    }
    // 走到这里 hwnd 必为 Some：上面循环要么在 Win32 分支 break（hwnd 已赋值），
    // 要么提前 return false。此 else 仅是 HANDLE_RETRY_ATTEMPTS == 0（循环体一次都不跑）
    // 时的防御性兜底，当前常量为 5 时不可达，保留以防后续把次数改 0。
    let Some(hwnd) = hwnd else {
        return false;
    };

    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
    true
}

#[cfg(target_os = "macos")]
pub(crate) fn show_capsule_window_no_activate<R: tauri::Runtime>(
    _app: &AppHandle<R>,
    window: &tauri::WebviewWindow<R>,
) -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let Ok(handle) = window.ns_window() else {
        return false;
    };
    let ns_window = handle as *mut AnyObject;
    if ns_window.is_null() {
        return false;
    }

    // emit_capsule 已经把窗口操作 marshal 到 Tauri 主线程；这里不能再调用
    // window.show()/set_focus()/NSApp.activate，否则 AeroSpace 会把 workspace 切回
    // OpenLess 主窗口所在空间。先让胶囊加入所有 Spaces，再用
    // orderFrontRegardless 做无激活展示。
    if let Err(e) = window.set_visible_on_all_workspaces(true) {
        log::warn!("[capsule] set visible on all macOS Spaces failed: {e}");
    }

    unsafe {
        const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES: usize = 1 << 0;
        const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: usize = 1 << 8;
        let behavior: usize = msg_send![ns_window, collectionBehavior];
        let behavior = behavior
            | NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
            | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY;
        let _: () = msg_send![ns_window, setCollectionBehavior: behavior];
        let _: () = msg_send![ns_window, orderFrontRegardless];
    }
    true
}

#[cfg(target_os = "linux")]
pub(crate) fn show_capsule_window_no_activate<R: tauri::Runtime>(
    _app: &AppHandle<R>,
    _window: &tauri::WebviewWindow<R>,
) -> bool {
    true
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub(crate) fn show_capsule_window_no_activate<R: tauri::Runtime>(
    _app: &AppHandle<R>,
    _window: &tauri::WebviewWindow<R>,
) -> bool {
    false
}

#[cfg(target_os = "windows")]
pub(crate) fn hide_capsule_window_if_present() {
    use std::iter::once;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetWindowPos, ShowWindow, HWND_NOTOPMOST, SWP_HIDEWINDOW, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    };

    let title: Vec<u16> = "OpenLess Capsule".encode_utf16().chain(once(0)).collect();
    let hwnd = match unsafe { FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) } {
        Ok(hwnd) => hwnd,
        Err(_) => return,
    };
    if hwnd == HWND::default() || hwnd.0.is_null() {
        return;
    }

    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_HIDEWINDOW,
        )
    };
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn hide_capsule_window_if_present() {}
