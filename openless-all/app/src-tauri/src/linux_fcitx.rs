//! Linux fcitx5 插件 DBus 客户端。
//!
//! 封装对 `org.fcitx.Fcitx.OpenLess1` 接口的调用，
//! 提供文字提交（替代 enigo XTest）和热键设置功能。
//!
//! 所有函数会静默返回 `None` 如果 fcitx5 / 插件不可用，
//! 调用方应当降级到原有方案（clipboard / enigo）。

use std::time::Duration;

use dbus::blocking::BlockingSender;

const DEST: &str = "org.fcitx.Fcitx5";
const PATH: &str = "/openless";
const IFACE: &str = "org.fcitx.Fcitx.OpenLess1";
const TIMEOUT: Duration = Duration::from_secs(3);

/// 通过 fcitx5 插件向当前焦点输入上下文提交文字。
///
/// 返回 `Ok(())` 表示文字已提交，`Err` 表示调用失败（插件未加载 / DBus 不通等）。
pub fn commit_text(text: &str) -> Result<(), String> {
    let conn = dbus::blocking::Connection::new_session()
        .map_err(|e| format!("dbus session: {e}"))?;
    let msg = dbus::Message::new_method_call(DEST, PATH, IFACE, "CommitText")
        .map_err(|e| format!("build msg: {e}"))?
        .append1(text);
    conn.send_with_reply_and_block(msg, TIMEOUT)
        .map_err(|e| format!("CommitText: {e}"))?;
    Ok(())
}

/// 通过 fcitx5 插件设置听写触发快捷键。
///
/// `keys` 为 Key::parse 格式的字符串数组，例如 `["Control+space"]`。
pub fn set_hotkey(keys: &[&str]) -> Result<(), String> {
    let conn = dbus::blocking::Connection::new_session()
        .map_err(|e| format!("dbus session: {e}"))?;
    let list: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
    let msg = dbus::Message::new_method_call(DEST, PATH, IFACE, "SetHotkey")
        .map_err(|e| format!("build msg: {e}"))?
        .append1(list);
    conn.send_with_reply_and_block(msg, TIMEOUT)
        .map_err(|e| format!("SetHotkey: {e}"))?;
    Ok(())
}

/// 通过 fcitx5 插件直接设置 sym + states 作为触发键。
pub fn set_hotkey_raw(sym: u32, states: u32) -> Result<(), String> {
    let conn = dbus::blocking::Connection::new_session()
        .map_err(|e| format!("dbus session: {e}"))?;
    let msg = dbus::Message::new_method_call(DEST, PATH, IFACE, "SetHotkeyRaw")
        .map_err(|e| format!("build msg: {e}"))?
        .append2(sym, states);
    conn.send_with_reply_and_block(msg, TIMEOUT)
        .map_err(|e| format!("SetHotkeyRaw: {e}"))?;
    Ok(())
}

/// 通过 fcitx5 插件设置 QA 面板快捷键 sym + states。
pub fn set_qa_hotkey_raw(sym: u32, states: u32) -> Result<(), String> {
    let conn = dbus::blocking::Connection::new_session()
        .map_err(|e| format!("dbus session: {e}"))?;
    let msg = dbus::Message::new_method_call(DEST, PATH, IFACE, "SetQaHotkeyRaw")
        .map_err(|e| format!("build msg: {e}"))?
        .append2(sym, states);
    conn.send_with_reply_and_block(msg, TIMEOUT)
        .map_err(|e| format!("SetQaHotkeyRaw: {e}"))?;
    Ok(())
}

/// 通过 fcitx5 插件设置翻译模式修饰键 sym + states。
pub fn set_translation_hotkey_raw(sym: u32, states: u32) -> Result<(), String> {
    let conn = dbus::blocking::Connection::new_session()
        .map_err(|e| format!("dbus session: {e}"))?;
    let msg = dbus::Message::new_method_call(DEST, PATH, IFACE, "SetTranslationHotkeyRaw")
        .map_err(|e| format!("build msg: {e}"))?
        .append2(sym, states);
    conn.send_with_reply_and_block(msg, TIMEOUT)
        .map_err(|e| format!("SetTranslationHotkeyRaw: {e}"))?;
    Ok(())
}

/// X11 keysym 值（用于 SetHotkeyRaw / SetQaHotkeyRaw / SetTranslationHotkeyRaw，
/// 绕过 Key::parse 的修饰键限制）。
const KEYSYM_CONTROL_R: u32 = 0xffe4;
const KEYSYM_CONTROL_L: u32 = 0xffe3;
const KEYSYM_ALT_R: u32 = 0xffea;
const KEYSYM_ALT_L: u32 = 0xffe9;
const KEYSYM_SUPER_R: u32 = 0xffec;
const KEYSYM_SUPER_L: u32 = 0xffeb;
const KEYSYM_SHIFT_R: u32 = 0xffe2;
const KEYSYM_SHIFT_L: u32 = 0xffe1;

/// 将 HotkeyTrigger 转换为 X11 keysym。
fn trigger_to_keysym(trigger: crate::types::HotkeyTrigger) -> u32 {
    match trigger {
        crate::types::HotkeyTrigger::RightControl => KEYSYM_CONTROL_R,
        crate::types::HotkeyTrigger::LeftControl => KEYSYM_CONTROL_L,
        crate::types::HotkeyTrigger::RightOption | crate::types::HotkeyTrigger::RightAlt => KEYSYM_ALT_R,
        crate::types::HotkeyTrigger::LeftOption => KEYSYM_ALT_L,
        crate::types::HotkeyTrigger::RightCommand => KEYSYM_SUPER_R,
        crate::types::HotkeyTrigger::Fn => KEYSYM_SUPER_L,
        crate::types::HotkeyTrigger::Custom => unreachable!(),
    }
}

fn trigger_name(trigger: crate::types::HotkeyTrigger) -> &'static str {
    match trigger {
        crate::types::HotkeyTrigger::RightControl => "Control_R",
        crate::types::HotkeyTrigger::LeftControl => "Control_L",
        crate::types::HotkeyTrigger::RightOption | crate::types::HotkeyTrigger::RightAlt => "Alt_R",
        crate::types::HotkeyTrigger::LeftOption => "Alt_L",
        crate::types::HotkeyTrigger::RightCommand => "Super_R",
        crate::types::HotkeyTrigger::Fn => "Super_L",
        crate::types::HotkeyTrigger::Custom => unreachable!(),
    }
}

/// 将 OpenLess 的主听写热键绑定同步到 fcitx5 插件。
pub fn sync_binding_to_plugin(binding: &crate::types::HotkeyBinding) {
    if binding.trigger == crate::types::HotkeyTrigger::Custom {
        return;
    }
    let sym = trigger_to_keysym(binding.trigger);
    let name = trigger_name(binding.trigger);
    match set_hotkey_raw(sym, 0) {
        Ok(()) => log::info!("[fcitx] Synced hotkey {name} (sym={sym}) to plugin via SetHotkeyRaw"),
        Err(e) => log::warn!("[fcitx] Failed to sync hotkey to plugin: {e}"),
    }
}

/// 将 QA 面板快捷键同步到 fcitx5 插件。
pub fn sync_qa_binding(trigger: Option<crate::types::HotkeyTrigger>) {
    let Some(trigger) = trigger else {
        // 无 QA 快捷键时清空插件端配置
        let _ = set_qa_hotkey_raw(0, 0);
        return;
    };
    let sym = trigger_to_keysym(trigger);
    let name = trigger_name(trigger);
    match set_qa_hotkey_raw(sym, 0) {
        Ok(()) => log::info!("[fcitx] Synced QA hotkey {name} (sym={sym}) to plugin via SetQaHotkeyRaw"),
        Err(e) => log::warn!("[fcitx] Failed to sync QA hotkey to plugin: {e}"),
    }
}

/// 将翻译模式快捷键同步到 fcitx5 插件。
pub fn sync_translation_binding(trigger: Option<crate::types::HotkeyTrigger>) {
    let Some(trigger) = trigger else {
        let _ = set_translation_hotkey_raw(0, 0);
        return;
    };
    let sym = trigger_to_keysym(trigger);
    let name = trigger_name(trigger);
    match set_translation_hotkey_raw(sym, 0) {
        Ok(()) => log::info!("[fcitx] Synced translation hotkey {name} (sym={sym}) to plugin via SetTranslationHotkeyRaw"),
        Err(e) => log::warn!("[fcitx] Failed to sync translation hotkey to plugin: {e}"),
    }
}

/// 快速检查 fcitx5 OpenLess 插件是否可用（DBus 对象存在）。
pub fn available() -> bool {
    let conn = match dbus::blocking::Connection::new_session() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let msg = match dbus::Message::new_method_call(DEST, PATH, "org.freedesktop.DBus.Peer", "Ping")
    {
        Ok(m) => m,
        Err(_) => return false,
    };
    conn.send_with_reply_and_block(msg, TIMEOUT).is_ok()
}

/// 启动 fcitx5 DictationKeyEvent 信号监听线程。
///
/// 当 fcitx5 OpenLess 插件检测到配置的听写热键被按下或松开时，
/// 发出 `DictationKeyEvent(uub)` DBus 信号（sym, states, isPress）。
/// 本函数将此信号转发为 `HotkeyEvent::Pressed` / `Released` 到协调器事件通道。
///
/// 后台线程在 `tx` 全部 drop（协调器关闭）或 DBus 连接断开时自动退出。
#[cfg(target_os = "linux")]
pub fn start_dictation_signal_listener(
    tx: std::sync::mpsc::Sender<crate::hotkey::HotkeyEvent>,
) {
    use std::time::Duration;

    std::thread::Builder::new()
        .name("openless-fcitx-signal".into())
        .spawn(move || {
            let conn = match dbus::blocking::SyncConnection::new_session() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("[fcitx-hotkey] DBus session failed: {e}");
                    return;
                }
            };

            // 同时监听所有三个信号
            let rule = match dbus::message::MatchRule::parse(
                "type='signal',\
                 interface='org.fcitx.Fcitx.OpenLess1'",
            ) {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("[fcitx-hotkey] Invalid match rule: {e}");
                    return;
                }
            };

            let tx2 = tx.clone();
            let _match = match conn.add_match(rule, move |args: (u32, u32, bool), _conn, msg| {
                let (sym, states, is_press) = args;
                let member = msg.member();
                let member_str: String = member.as_ref().map(|m| m.to_string()).unwrap_or_default();
                log::debug!(
                    "[fcitx-hotkey] Signal {}: sym={}, states={}, isPress={}",
                    member_str, sym, states, is_press,
                );
                if let Some(member) = member {
                    if member == "DictationKeyEvent" {
                        let event = if is_press {
                            crate::hotkey::HotkeyEvent::Pressed
                        } else {
                            crate::hotkey::HotkeyEvent::Released
                        };
                        let _ = tx.send(event);
                    } else if member == "QaShortcutEvent" {
                        if is_press {
                            let _ = tx2.send(crate::hotkey::HotkeyEvent::QaShortcutPressed);
                        }
                    } else if member == "TranslationModifierEvent" {
                        if is_press {
                            let _ = tx2.send(crate::hotkey::HotkeyEvent::TranslationModifierPressed);
                        }
                    }
                }
                true
            }) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("[fcitx-hotkey] Failed to add match: {e}");
                    return;
                }
            };

            log::info!("[fcitx-hotkey] Listening for OpenLess1 signals");
            loop {
                if let Err(e) = conn.process(Duration::from_millis(500)) {
                    log::warn!("[fcitx-hotkey] DBus process error: {e}");
                    break;
                }
            }
        })
        .ok();
}
