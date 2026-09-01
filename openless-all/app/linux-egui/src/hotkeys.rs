use std::sync::{Arc, Mutex};

use openless_core::{BackendError, BackendErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxHotkeyEvent {
    DictationPressed {
        symbol: u32,
        states: u32,
        at: std::time::Instant,
    },
    DictationReleased {
        symbol: u32,
        states: u32,
        at: std::time::Instant,
    },
    DictationCombined {
        symbol: u32,
        states: u32,
        at: std::time::Instant,
    },
    LessComputerPressed {
        symbol: u32,
        states: u32,
        at: std::time::Instant,
    },
    LessComputerReleased {
        symbol: u32,
        states: u32,
        at: std::time::Instant,
    },
    LessComputerCombined {
        symbol: u32,
        states: u32,
        at: std::time::Instant,
    },
    QaPressed,
    SelectionPolishPressed,
    TranslationPressed,
}

pub struct Fcitx5HotkeyListener {
    receiver: Mutex<std::sync::mpsc::Receiver<LinuxHotkeyEvent>>,
    error: Arc<Mutex<Option<BackendError>>>,
    #[cfg(target_os = "linux")]
    stop: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(target_os = "linux")]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fcitx5HotkeyListener {
    pub fn start() -> Result<Self, BackendError> {
        #[cfg(target_os = "linux")]
        {
            start_linux_listener()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "fcitx5 hotkey listener is only available on Linux",
            ))
        }
    }

    pub fn drain(&self, mut apply: impl FnMut(LinuxHotkeyEvent)) -> usize {
        let receiver = self
            .receiver
            .lock()
            .expect("fcitx5 hotkey receiver lock poisoned");
        let mut count = 0;
        while let Ok(event) = receiver.try_recv() {
            count += 1;
            apply(event);
        }
        count
    }

    pub fn take_error(&self) -> Option<BackendError> {
        self.error
            .lock()
            .expect("fcitx5 hotkey error lock poisoned")
            .take()
    }
}

#[cfg(target_os = "linux")]
impl Drop for Fcitx5HotkeyListener {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(target_os = "linux")]
fn start_linux_listener() -> Result<Fcitx5HotkeyListener, BackendError> {
    use std::sync::atomic::AtomicBool;

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let error = Arc::new(Mutex::new(None));
    let stop_for_thread = Arc::clone(&stop);
    let error_for_thread = Arc::clone(&error);
    let thread = std::thread::Builder::new()
        .name("openless-fcitx5-hotkeys".to_string())
        .spawn(move || {
            run_listener(event_tx, startup_tx, stop_for_thread, error_for_thread);
        })
        .map_err(|error| {
            BackendError::new(
                BackendErrorCode::Platform,
                format!("failed to spawn fcitx5 hotkey listener: {error}"),
            )
        })?;
    match startup_rx.recv() {
        Ok(Ok(())) => Ok(Fcitx5HotkeyListener {
            receiver: Mutex::new(event_rx),
            error,
            stop,
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(error) => {
            let _ = thread.join();
            Err(BackendError::new(
                BackendErrorCode::Platform,
                format!("fcitx5 hotkey listener exited during startup: {error}"),
            ))
        }
    }
}

#[cfg(target_os = "linux")]
fn run_listener(
    events: std::sync::mpsc::Sender<LinuxHotkeyEvent>,
    startup: std::sync::mpsc::SyncSender<Result<(), BackendError>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    error: Arc<Mutex<Option<BackendError>>>,
) {
    let connection = match dbus::blocking::SyncConnection::new_session() {
        Ok(connection) => connection,
        Err(dbus_error) => {
            let _ = startup.send(Err(dbus_backend_error(dbus_error)));
            return;
        }
    };
    let rule = match dbus::message::MatchRule::parse(&format!(
        "type='signal',interface='{}'",
        crate::fcitx5::INTERFACE
    )) {
        Ok(rule) => rule.static_clone(),
        Err(parse_error) => {
            let _ = startup.send(Err(BackendError::new(
                BackendErrorCode::Internal,
                format!("invalid fcitx5 hotkey signal rule: {parse_error}"),
            )));
            return;
        }
    };
    let stop_for_match = Arc::clone(&stop);
    let signal_match =
        match connection.add_match(rule, move |args: (u32, u32, bool), _, message| {
            let member = message
                .member()
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            if let Some(event) =
                event_from_signal(&member, args.0, args.1, args.2, std::time::Instant::now())
            {
                if events.send(event).is_err() {
                    stop_for_match.store(true, std::sync::atomic::Ordering::Release);
                    return false;
                }
            }
            true
        }) {
            Ok(signal_match) => signal_match,
            Err(dbus_error) => {
                let _ = startup.send(Err(dbus_backend_error(dbus_error)));
                return;
            }
        };
    if startup.send(Ok(())).is_err() {
        return;
    }

    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        if let Err(dbus_error) = connection.process(std::time::Duration::from_millis(250)) {
            *error.lock().expect("fcitx5 hotkey error lock poisoned") =
                Some(dbus_backend_error(dbus_error));
            break;
        }
    }
    let _ = connection.remove_match(signal_match);
}

#[cfg(any(target_os = "linux", test))]
fn event_from_signal(
    member: &str,
    symbol: u32,
    states: u32,
    is_press: bool,
    at: std::time::Instant,
) -> Option<LinuxHotkeyEvent> {
    match (member, is_press) {
        ("DictationKeyEvent", true) => {
            Some(LinuxHotkeyEvent::DictationPressed { symbol, states, at })
        }
        ("DictationKeyEvent", false) => {
            Some(LinuxHotkeyEvent::DictationReleased { symbol, states, at })
        }
        ("DictationKeyCombined", true) => {
            Some(LinuxHotkeyEvent::DictationCombined { symbol, states, at })
        }
        ("LessComputerKeyEvent", true) => {
            Some(LinuxHotkeyEvent::LessComputerPressed { symbol, states, at })
        }
        ("LessComputerKeyEvent", false) => {
            Some(LinuxHotkeyEvent::LessComputerReleased { symbol, states, at })
        }
        ("LessComputerKeyCombined", true) => {
            Some(LinuxHotkeyEvent::LessComputerCombined { symbol, states, at })
        }
        ("QaShortcutEvent", true) => Some(LinuxHotkeyEvent::QaPressed),
        ("SelectionPolishEvent", true) => Some(LinuxHotkeyEvent::SelectionPolishPressed),
        ("TranslationModifierEvent", true) => Some(LinuxHotkeyEvent::TranslationPressed),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn dbus_backend_error(error: dbus::Error) -> BackendError {
    BackendError::new(
        BackendErrorCode::Platform,
        format!("fcitx5 hotkey DBus listener failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_protocol_maps_press_release_and_action_events() {
        let at = std::time::Instant::now();
        assert_eq!(
            event_from_signal("DictationKeyEvent", 1, 2, true, at),
            Some(LinuxHotkeyEvent::DictationPressed {
                symbol: 1,
                states: 2,
                at,
            })
        );
        assert_eq!(
            event_from_signal("DictationKeyEvent", 1, 2, false, at),
            Some(LinuxHotkeyEvent::DictationReleased {
                symbol: 1,
                states: 2,
                at,
            })
        );
        assert_eq!(
            event_from_signal("QaShortcutEvent", 0, 0, true, at),
            Some(LinuxHotkeyEvent::QaPressed)
        );
        assert!(matches!(
            event_from_signal("LessComputerKeyEvent", 3, 4, true, at),
            Some(LinuxHotkeyEvent::LessComputerPressed {
                symbol: 3,
                states: 4,
                ..
            })
        ));
        assert!(matches!(
            event_from_signal("LessComputerKeyEvent", 3, 4, false, at),
            Some(LinuxHotkeyEvent::LessComputerReleased {
                symbol: 3,
                states: 4,
                ..
            })
        ));
        assert!(matches!(
            event_from_signal("LessComputerKeyCombined", 3, 4, true, at),
            Some(LinuxHotkeyEvent::LessComputerCombined {
                symbol: 3,
                states: 4,
                ..
            })
        ));
        assert_eq!(event_from_signal("Unknown", 0, 0, true, at), None);
        assert_eq!(event_from_signal("QaShortcutEvent", 0, 0, false, at), None);
    }
}
