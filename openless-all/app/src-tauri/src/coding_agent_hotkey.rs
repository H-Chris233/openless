//! 快速 Agent 的两个全局组合键监听器（面板键 + 快取用键）。
//!
//! 镜像 `qa_hotkey.rs`，但同时注册两个 binding，事件区分来源
//! （[`CodingAgentHotkeyEvent::PanelPressed`] / [`QuickPressed`]）。
//! toggle / 生命周期由 coordinator 解释。

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use parking_lot::Mutex;

use crate::global_hotkey_runtime::{GlobalHotkeyRuntime, RegisteredHotkey};
use crate::shortcut_binding::{parse_global_hotkey, ShortcutBindingError};
use crate::types::ShortcutBinding;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingAgentHotkeyEvent {
    /// 面板键：语音 Agent（录音→ASR→Claude→面板）。
    PanelPressed,
    /// 快取用键：选中文本→Claude→回插光标。
    QuickPressed,
}

#[derive(Debug, thiserror::Error)]
pub enum CodingAgentHotkeyError {
    #[error("不支持的修饰键: {0}")]
    UnsupportedModifier(String),
    #[error("不支持的主键: {0}")]
    UnsupportedKey(String),
    #[error("注册全局快捷键失败: {0}")]
    RegisterFailed(String),
    #[error("初始化全局快捷键管理器失败: {0}")]
    ManagerInitFailed(String),
}

/// 同时持有面板键与快取用键的注册句柄；`Drop` 时全部反注册。
pub struct CodingAgentHotkeyMonitor {
    inner: Arc<Inner>,
}

struct Inner {
    registered: Mutex<Vec<RegisteredHotkey>>,
}

// 与 qa_hotkey.rs::Inner 同款理由：global-hotkey 句柄是 OS 进程级资源，
// coordinator 需要把它放进 async 上下文，手动标记 Send/Sync。
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl CodingAgentHotkeyMonitor {
    /// 注册 panel / quick 两个组合键（任一为 `None` 即跳过）。每次按下边沿向 `tx`
    /// 投递对应事件。需从主线程调用（macOS 的 global-hotkey 要求）。
    pub fn start(
        panel: Option<ShortcutBinding>,
        quick: Option<ShortcutBinding>,
        tx: Sender<CodingAgentHotkeyEvent>,
    ) -> Result<Self, CodingAgentHotkeyError> {
        let runtime = GlobalHotkeyRuntime::shared()
            .map_err(|e| CodingAgentHotkeyError::ManagerInitFailed(e.to_string()))?;
        let mut registered = Vec::new();

        for (binding, event) in [
            (panel, CodingAgentHotkeyEvent::PanelPressed),
            (quick, CodingAgentHotkeyEvent::QuickPressed),
        ] {
            let Some(binding) = binding else { continue };
            // 单个 binding 无效（如单修饰键 Right ⌘）只跳过它，绝不让一个坏键拖垮
            // 另一个有效键的注册，也避免 supervisor 因为 Err 而陷入无限重试。
            let hotkey = match parse_binding(&binding) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("[coding-agent] 跳过无法解析的快捷键 ({event:?}): {e}");
                    continue;
                }
            };
            let (reg, rx) = match runtime.register(hotkey) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("[coding-agent] 注册快捷键失败 ({event:?}): {e}");
                    continue;
                }
            };
            spawn_forward(reg.hotkey().id(), rx, tx.clone(), event);
            registered.push(reg);
        }

        Ok(Self {
            inner: Arc::new(Inner {
                registered: Mutex::new(registered),
            }),
        })
    }
}

impl Drop for CodingAgentHotkeyMonitor {
    fn drop(&mut self) {
        self.inner.registered.lock().clear();
    }
}

/// 单个 agent 绑定是否能被 global-hotkey 注册。单修饰键（如 Right ⌘）无法注册成
/// 全局快捷键 → 视为无效，需要回退到有效组合键。
fn agent_binding_is_registrable(binding: &ShortcutBinding) -> bool {
    parse_global_hotkey(binding).is_ok()
}

/// 把可能无效的面板键 / 快取键换成有效默认组合键（⌘⇧Enter / ⌘⇧J）。
///
/// 历史版本或手滑可能把 agent 热键存成单修饰键，导致注册无限失败、按下毫无反应。
/// 返回 `(panel, quick, changed)`；`changed` 为 true 时调用方应持久化并通知前端。
pub(crate) fn sanitize_agent_hotkeys(
    panel: Option<ShortcutBinding>,
    quick: Option<ShortcutBinding>,
) -> (Option<ShortcutBinding>, Option<ShortcutBinding>, bool) {
    let mut changed = false;
    let panel = match panel {
        Some(b) if !agent_binding_is_registrable(&b) => {
            changed = true;
            crate::types::default_coding_agent_panel_hotkey()
        }
        other => other,
    };
    let quick = match quick {
        Some(b) if !agent_binding_is_registrable(&b) => {
            changed = true;
            crate::types::default_coding_agent_quick_hotkey()
        }
        other => other,
    };
    (panel, quick, changed)
}

fn spawn_forward(
    id: u32,
    rx: Receiver<GlobalHotKeyEvent>,
    tx: Sender<CodingAgentHotkeyEvent>,
    event: CodingAgentHotkeyEvent,
) {
    let _ = std::thread::Builder::new()
        .name("openless-coding-agent-hotkey-forward".into())
        .spawn(move || forward_loop(id, rx, tx, event));
}

fn forward_loop(
    hotkey_id: u32,
    rx: Receiver<GlobalHotKeyEvent>,
    tx: Sender<CodingAgentHotkeyEvent>,
    event: CodingAgentHotkeyEvent,
) {
    while let Ok(global_event) = rx.recv() {
        if global_event.id() != hotkey_id {
            continue;
        }
        if !matches!(global_event.state(), HotKeyState::Pressed) {
            continue;
        }
        if tx.send(event).is_err() {
            break;
        }
    }
}

fn parse_binding(
    binding: &ShortcutBinding,
) -> Result<global_hotkey::hotkey::HotKey, CodingAgentHotkeyError> {
    parse_global_hotkey(binding).map_err(|e| match e {
        ShortcutBindingError::UnsupportedModifier(m) => {
            CodingAgentHotkeyError::UnsupportedModifier(m)
        }
        ShortcutBindingError::UnsupportedKey(k) => CodingAgentHotkeyError::UnsupportedKey(k),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use global_hotkey::hotkey::Code;

    #[test]
    fn parses_default_panel_binding() {
        let binding = ShortcutBinding {
            primary: "Enter".into(),
            modifiers: vec!["cmd".into(), "shift".into()],
        };
        let parsed = parse_binding(&binding).expect("Enter binding parses");
        assert_eq!(parsed.key, Code::Enter);
    }

    #[test]
    fn parses_default_quick_binding() {
        let binding = ShortcutBinding {
            primary: "J".into(),
            modifiers: vec!["cmd".into(), "shift".into()],
        };
        let parsed = parse_binding(&binding).expect("J binding parses");
        assert_eq!(parsed.key, Code::KeyJ);
    }

    #[test]
    fn forward_loop_filters_other_ids_and_releases() {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        event_tx
            .send(GlobalHotKeyEvent { id: 1, state: HotKeyState::Pressed })
            .unwrap();
        event_tx
            .send(GlobalHotKeyEvent { id: 7, state: HotKeyState::Released })
            .unwrap();
        event_tx
            .send(GlobalHotKeyEvent { id: 7, state: HotKeyState::Pressed })
            .unwrap();
        drop(event_tx);
        forward_loop(7, event_rx, out_tx, CodingAgentHotkeyEvent::QuickPressed);
        assert_eq!(out_rx.recv().unwrap(), CodingAgentHotkeyEvent::QuickPressed);
        assert!(out_rx.try_recv().is_err());
    }

    fn bare_right_command() -> ShortcutBinding {
        ShortcutBinding { primary: "RightCommand".into(), modifiers: vec![] }
    }

    #[test]
    fn sanitize_replaces_bare_modifier_panel() {
        // 复现用户的故障：把面板键存成单修饰键 Right ⌘。
        let (panel, quick, changed) = sanitize_agent_hotkeys(Some(bare_right_command()), None);
        assert!(changed, "无效绑定必须触发自愈");
        assert_eq!(panel, crate::types::default_coding_agent_panel_hotkey());
        // 修好后必须能被 global-hotkey 真正注册。
        assert!(parse_global_hotkey(&panel.unwrap()).is_ok());
        assert_eq!(quick, None, "没配的键保持 None，不应凭空生成");
    }

    #[test]
    fn sanitize_replaces_bare_modifier_quick() {
        let (panel, quick, changed) = sanitize_agent_hotkeys(None, Some(bare_right_command()));
        assert!(changed);
        assert_eq!(panel, None);
        assert_eq!(quick, crate::types::default_coding_agent_quick_hotkey());
        assert!(parse_global_hotkey(&quick.unwrap()).is_ok());
    }

    #[test]
    fn sanitize_keeps_valid_combo_untouched() {
        let panel = Some(ShortcutBinding {
            primary: "Enter".into(),
            modifiers: vec!["cmd".into(), "shift".into()],
        });
        let quick = Some(ShortcutBinding {
            primary: "J".into(),
            modifiers: vec!["cmd".into(), "shift".into()],
        });
        let (p, q, changed) = sanitize_agent_hotkeys(panel.clone(), quick.clone());
        assert!(!changed, "有效组合键不应被改动");
        assert_eq!(p, panel);
        assert_eq!(q, quick);
    }

    #[test]
    fn sanitize_none_is_noop() {
        let (p, q, changed) = sanitize_agent_hotkeys(None, None);
        assert!(!changed);
        assert_eq!(p, None);
        assert_eq!(q, None);
    }
}
