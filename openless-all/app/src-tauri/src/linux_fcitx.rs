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
        crate::types::HotkeyTrigger::Fn => KEYSYM_CONTROL_R,
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
        crate::types::HotkeyTrigger::Fn => "Control_R",
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

/// 将 ShortcutBinding 转换为 fcitx5 Key::parse 格式的字符串。
///
/// 例如 `modifiers: ["Ctrl", "Alt"], primary: "d"` → `"Control+Alt+d"`。
pub fn binding_to_fcitx_key_string(binding: &crate::types::ShortcutBinding) -> String {
    let mut parts: Vec<String> = Vec::new();
    for m in &binding.modifiers {
        let lower = m.to_lowercase();
        let normalized = match lower.as_str() {
            "ctrl" | "control" => "Control",
            "alt" | "option" | "opt" => "Alt",
            "shift" => "Shift",
            "super" | "meta" | "cmd" | "win" | "command" => "Super",
            other => other,
        };
        if !parts.contains(&normalized.to_string()) {
            parts.push(normalized.to_string());
        }
    }
    // 主键：取小写，去掉 "Key" 前缀（如 "KeyD" → "d"）
    let primary = binding.primary.trim();
    let primary = if let Some(stripped) = primary.strip_prefix("Key") {
        stripped.to_lowercase()
    } else {
        primary.to_lowercase()
    };
    if primary.is_empty() {
        return String::new();
    }
    if parts.is_empty() {
        primary
    } else {
        format!("{}+{}", parts.join("+"), primary)
    }
}

/// 通过 fcitx5 插件的 SetCustomDictationTrigger 方法设置自定义组合键。
pub fn set_custom_dictation_trigger(key_string: &str) -> Result<(), String> {
    let conn = dbus::blocking::Connection::new_session()
        .map_err(|e| format!("dbus session: {e}"))?;
    let msg = dbus::Message::new_method_call(
        DEST, PATH, IFACE, "SetCustomDictationTrigger",
    )
    .map_err(|e| format!("build msg: {e}"))?
    .append1(key_string);
    conn.send_with_reply_and_block(msg, TIMEOUT)
        .map_err(|e| format!("SetCustomDictationTrigger: {e}"))?;
    Ok(())
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

/// 通过 fcitx5 插件在候选词列表下方显示状态文本（不干扰输入法预编辑）。
pub fn set_aux_down(text: &str) -> Result<(), String> {
    let conn = dbus::blocking::Connection::new_session()
        .map_err(|e| format!("dbus session: {e}"))?;
    let msg = dbus::Message::new_method_call(DEST, PATH, IFACE, "SetAuxDown")
        .map_err(|e| format!("build msg: {e}"))?
        .append1(text);
    conn.send_with_reply_and_block(msg, TIMEOUT)
        .map_err(|e| format!("SetAuxDown: {e}"))?;
    Ok(())
}

/// 清除 fcitx5 插件候选词列表下方状态文本。
pub fn clear_aux_down() -> Result<(), String> {
    let conn = dbus::blocking::Connection::new_session()
        .map_err(|e| format!("dbus session: {e}"))?;
    let msg = dbus::Message::new_method_call(DEST, PATH, IFACE, "ClearAuxDown")
        .map_err(|e| format!("build msg: {e}"))?;
    conn.send_with_reply_and_block(msg, TIMEOUT)
        .map_err(|e| format!("ClearAuxDown: {e}"))?;
    Ok(())
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
///
/// 如果 fcitx5 尚未启动，线程会每 3 秒重试同步热键绑定，直到 fcitx5 可用。
/// 同时监听 `NameOwnerChanged` 信号以在 fcitx5 重启后重新同步。
#[cfg(target_os = "linux")]
pub fn start_dictation_signal_listener(
    tx: std::sync::mpsc::Sender<crate::hotkey::HotkeyEvent>,
    binding: crate::types::HotkeyBinding,
    qa_trigger: Option<crate::types::HotkeyTrigger>,
    translation_trigger: Option<crate::types::HotkeyTrigger>,
    custom_trigger_key: Option<String>,
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

            // 同时监听所有三个 OpenLess 信号
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

            // 监听 fcitx5 的 NameOwnerChanged 信号，用于在 fcitx5 重启后重新同步。
            // dbus crate 的 MatchRule::parse 不支持 arg0 过滤，在回调里做匹配。
            let fcitx_rule = match dbus::message::MatchRule::parse(
                "type='signal',\
                 sender='org.freedesktop.DBus',\
                 interface='org.freedesktop.DBus',\
                 member='NameOwnerChanged'",
            ) {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("[fcitx-hotkey] Invalid fcitx5 name watch rule: {e}");
                    return;
                }
            };

            // NOTE: NameOwnerChanged 捕获的是线程启动时的绑定快照。用户在
            // OpenLess 运行时改了快捷键且 fcitx5 恰好重启，重连会写入旧绑定。
            // 这是一个低概率场景（需要两个操作同时发生），暂时保留快照语义。
            // 要彻底解决需要把 Arc<PreferencesStore> 传给监听线程做实时读取。
            let binding_for_name = binding.clone();
            let custom_for_name = custom_trigger_key.clone();
            let qa_for_name = qa_trigger;
            let trans_for_name = translation_trigger;
            let _name_match = match conn.add_match(fcitx_rule, move |args: (String, String, String), _conn, _msg| {
                let (name, _old_owner, new_owner) = args;
                if name != "org.fcitx.Fcitx5" { return true; }
                if !new_owner.is_empty() {
                    // fcitx5 已启动（或重启），重新同步所有快捷键绑定。
                    // 把延迟+同步挪到独立线程：add_match 回调跑在 DBus 事件循环
                    // 线程里，sleep 会阻塞所有信号处理。
                    log::info!("[fcitx-hotkey] fcitx5 appeared on DBus, re-syncing bindings");
                    let b = binding_for_name.clone();
                    let c = custom_for_name.clone();
                    let q = qa_for_name;
                    let t = trans_for_name;
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(1)); // 等插件完全加载
                        resync_main_binding(&b, c.as_deref());
                        sync_qa_binding(q);
                        sync_translation_binding(t);
                    });
                }
                true
            }) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("[fcitx-hotkey] Failed to add fcitx5 name watch: {e}");
                    return;
                }
            };

            // 初始同步：等待 fcitx5 可用（最多重试 10 次，每次 3 秒）。
            for attempt in 0..10 {
                if fcitx5_name_has_owner(&conn) {
                    log::info!("[fcitx-hotkey] fcitx5 available, syncing initial bindings (attempt {attempt})");
                    resync_main_binding(&binding, custom_trigger_key.as_deref());
                    sync_qa_binding(qa_trigger);
                    sync_translation_binding(translation_trigger);
                    break;
                }
                if attempt == 0 {
                    log::info!("[fcitx-hotkey] fcitx5 not yet available, will retry...");
                }
                std::thread::sleep(Duration::from_secs(3));
            }

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

/// AppImage / 便携版：每次启动时从 bundled resources 复制插件到
/// `~/.local/lib/fcitx5/` 和 `~/.local/share/fcitx5/addon/`，始终覆盖已有文件。
///
/// 这确保 AppImage 版本与插件版本一致——插件新增 DBus 方法时旧 .so 不会缺少符号。
/// 系统路径（deb/rpm 安装）不会被覆盖。
/// 安装后需要用户重启 fcitx5（`fcitx5 -r`）才能加载新插件。
#[cfg(target_os = "linux")]
pub fn ensure_plugin_installed(app: &tauri::AppHandle) {
    use tauri::Manager;

    let resource_dir = match app.path().resource_dir() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("[fcitx-install] Cannot resolve resource dir: {e}");
            return;
        }
    };

    let so_src = resource_dir.join("linux-fcitx5-plugin").join("libopenless.so");
    if !so_src.exists() {
        log::info!(
            "[fcitx-install] Bundled plugin not found at {:?} — not an AppImage or plugin not bundled",
            so_src
        );
        return;
    }

    let Ok(home) = std::env::var("HOME") else {
        log::warn!("[fcitx-install] Cannot determine HOME dir");
        return;
    };
    let home = std::path::PathBuf::from(home);

    let lib_dir = home.join(".local").join("lib").join("fcitx5");
    let addon_dir = home.join(".local").join("share").join("fcitx5").join("addon");

    if let Err(e) = std::fs::create_dir_all(&lib_dir) {
        log::warn!("[fcitx-install] Failed to create {:?}: {e}", lib_dir);
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&addon_dir) {
        log::warn!("[fcitx-install] Failed to create {:?}: {e}", addon_dir);
        return;
    }

    let so_dest = lib_dir.join("libopenless.so");
    if let Err(e) = std::fs::copy(&so_src, &so_dest) {
        log::warn!("[fcitx-install] Failed to copy plugin .so: {e}");
        return;
    }
    log::info!("[fcitx-install] Installed plugin .so to {:?}", so_dest);

    let config_content = format!(
        concat!(
            "[Addon]\n",
            "Name=OpenLess\n",
            "Name[zh_CN]=OpenLess 听写辅助\n",
            "Comment=OpenLess dictation commit helper\n",
            "Comment[zh_CN]=供 OpenLess 听写提交文字的 DBus 接口及快捷键监听\n",
            "Category=Module\n",
            "Type=SharedLibrary\n",
            "Library={}\n",
            "Version=1.0.0\n",
            "OnDemand=False\n",
            "Configurable=False\n",
            "\n",
            "[Addon/Dependencies]\n",
            "0=core\n",
            "1=dbus\n",
        ),
        so_dest.display()
    );

    let conf_dest = addon_dir.join("openless.conf");
    if let Err(e) = std::fs::write(&conf_dest, &config_content) {
        log::warn!("[fcitx-install] Failed to write addon config: {e}");
        return;
    }
    log::info!("[fcitx-install] Installed addon config to {:?}", conf_dest);
    log::info!(
        "[fcitx-install] Done. Run `fcitx5 -r` to load the plugin, then restart OpenLess."
    );
}

/// 同步主听写热键：自定义组合键走 SetCustomDictationTrigger，预设修饰键走 SetHotkeyRaw。
fn resync_main_binding(binding: &crate::types::HotkeyBinding, custom_trigger_key: Option<&str>) {
    if let Some(key_string) = custom_trigger_key {
        if !key_string.is_empty() {
            match set_custom_dictation_trigger(key_string) {
                Ok(()) => log::info!("[fcitx] Resynced custom dictation trigger '{key_string}'"),
                Err(e) => log::warn!("[fcitx] Failed to resync custom dictation trigger: {e}"),
            }
            return;
        }
    }
    sync_binding_to_plugin(binding);
}

/// 检查 fcitx5 是否在 DBus 上注册了名称（即 fcitx5 进程是否在运行且 DBus 模块已加载）。
fn fcitx5_name_has_owner(conn: &dbus::blocking::SyncConnection) -> bool {
    use dbus::blocking::BlockingSender;
    let msg = match dbus::Message::new_method_call(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
    ) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let msg = msg.append1("org.fcitx.Fcitx5");
    match conn.send_with_reply_and_block(msg, Duration::from_secs(1)) {
        Ok(reply) => reply.read1::<bool>().unwrap_or(false),
        Err(_) => false,
    }
}
