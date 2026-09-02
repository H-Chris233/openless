use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::{qa::handle_qa_option_edge, Inner};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LessComputerEventReplay {
    pub(crate) events: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) oldest_sequence: Option<u64>,
    pub(crate) latest_sequence: u64,
    pub(crate) truncated: bool,
}

pub(crate) fn less_computer_event_replay_after(
    backend: &openless_core::OpenLessBackend,
    sequence: u64,
) -> LessComputerEventReplay {
    let replay = backend.replay_events_after(sequence);
    let mut events: Vec<serde_json::Value> = replay
        .events
        .into_iter()
        .filter_map(|event| match event.kind {
            openless_core::BackendEventKind::LessComputerEvent(event) => {
                serde_json::to_value(event).ok()
            }
            _ => None,
        })
        .collect();
    if let Some(index) = events.iter().rposition(|event| {
        event.get("kind").and_then(serde_json::Value::as_str) == Some("user")
            && event.get("fresh").and_then(serde_json::Value::as_bool) == Some(true)
    }) {
        events.drain(0..index);
    }
    LessComputerEventReplay {
        events,
        oldest_sequence: replay.oldest_sequence,
        latest_sequence: replay.latest_sequence,
        truncated: replay.truncated,
    }
}

async fn dispatch(
    inner: &Arc<Inner>,
    edge: openless_core::DictationHotkeyEdge,
) -> Result<openless_core::CliDispatchOutcome, openless_core::BackendError> {
    if !inner.backend.snapshot().running {
        inner.backend.start().await?;
    }
    let translation_requested = inner.translation_active.load(Ordering::SeqCst);
    inner
        .backend
        .dispatch_dictation_hotkey_edge_with_session_options(
            edge,
            openless_core::DictationHotkeyDispatchOptions {
                start: openless_core::DictationStartOptions {
                    translation_requested,
                    ..openless_core::DictationStartOptions::default()
                },
                stop: openless_core::DictationStopOptions {
                    translation_requested: translation_requested.then_some(true),
                },
            },
        )
        .await
}

fn finish_bookkeeping(inner: &Arc<Inner>) {
    inner.translation_active.store(false, Ordering::SeqCst);
    *inner.session_cooldown_until.lock() =
        Some(std::time::Instant::now() + std::time::Duration::from_millis(450));
}

pub(super) async fn handle_pressed_edge(
    inner: &Arc<Inner>,
    pressed_at: std::time::Instant,
    press_id: u64,
) {
    if inner.hotkey_trigger_held.swap(true, Ordering::SeqCst) {
        return;
    }
    inner
        .hotkey_press_generation
        .store(press_id, Ordering::SeqCst);
    inner.hotkey_press_began_session.store(0, Ordering::SeqCst);

    let now = std::time::Instant::now();
    let debounced = {
        let mut last = inner.last_hotkey_dispatch_at.lock();
        let debounced = last.is_some_and(|last| {
            now.saturating_duration_since(last) < std::time::Duration::from_millis(250)
        });
        if !debounced {
            *last = Some(now);
        }
        debounced
    };
    if debounced {
        return;
    }

    if inner.qa_context.is_panel_visible()
        && inner.backend.snapshot().dictation.phase == openless_core::DictationPhase::Idle
    {
        handle_qa_option_edge(inner).await;
        return;
    }
    match dispatch(
        inner,
        openless_core::DictationHotkeyEdge::Pressed { at: pressed_at },
    )
    .await
    {
        Ok(openless_core::CliDispatchOutcome::DictationStarted(_)) => {
            inner
                .hotkey_press_began_session
                .store(press_id, Ordering::SeqCst);
        }
        Ok(openless_core::CliDispatchOutcome::DictationCompleted(_))
        | Ok(openless_core::CliDispatchOutcome::DictationCancelled) => finish_bookkeeping(inner),
        Ok(_) => {}
        Err(error) => log::warn!("[coord] core dictation press failed: {error}"),
    }
}

pub(super) async fn handle_released_edge(inner: &Arc<Inner>, released_at: std::time::Instant) {
    if !inner.hotkey_trigger_held.swap(false, Ordering::SeqCst) {
        return;
    }
    match dispatch(
        inner,
        openless_core::DictationHotkeyEdge::Released { at: released_at },
    )
    .await
    {
        Ok(openless_core::CliDispatchOutcome::DictationCompleted(_))
        | Ok(openless_core::CliDispatchOutcome::DictationCancelled) => finish_bookkeeping(inner),
        Ok(_) => {}
        Err(error) => log::warn!("[coord] core dictation release failed: {error}"),
    }
}

pub(super) fn handle_trigger_combined(inner: &Arc<Inner>, press_id: u64) {
    if inner.hotkey_press_generation.load(Ordering::SeqCst) != press_id
        || inner.hotkey_press_began_session.swap(0, Ordering::SeqCst) != press_id
    {
        return;
    }
    inner.hotkey_trigger_held.store(false, Ordering::SeqCst);
    let result = inner.host.block_on(dispatch(
        inner,
        openless_core::DictationHotkeyEdge::Combined,
    ));
    if let Err(error) = result {
        log::warn!("[coord] core dictation combo cancel failed: {error}");
    }
    finish_bookkeeping(inner);
}

#[cfg(any(debug_assertions, test))]
pub(super) async fn handle_pressed(
    inner: &Arc<Inner>,
    pressed_at: std::time::Instant,
    press_id: u64,
) {
    handle_pressed_edge(inner, pressed_at, press_id).await;
}

#[cfg(any(debug_assertions, test))]
pub(super) async fn handle_released(inner: &Arc<Inner>, released_at: std::time::Instant) {
    handle_released_edge(inner, released_at).await;
}

pub(super) async fn cancel_active_session(inner: &Arc<Inner>) -> bool {
    let less_computer = inner.less_computer_voice.lock().take();
    if let Some(session) = less_computer {
        let _ = session.cancel().await;
        inner.host.hide_less_computer_glow();
        return true;
    }
    #[cfg(all(not(mobile), target_os = "windows"))]
    {
        let capture = inner.selection_voice_capture.lock().take();
        if let Some(capture) = capture {
            let session_id = capture.session_id();
            let _ = capture.cancel().await;
            let _ = inner
                .backend
                .services()
                .selection_voice
                .cancel(Some(session_id))
                .await;
            return true;
        }
    }
    if let Ok(snapshot) = inner.backend.services().qa.snapshot().await {
        if snapshot.phase != openless_core::QaPhase::Idle {
            let _ = inner
                .backend
                .services()
                .qa
                .cancel(snapshot.session_id)
                .await;
            return true;
        }
    }
    let session_id = inner.backend.snapshot().dictation.session_id;
    match inner.backend.cancel_dictation(session_id).await {
        Ok(()) => {
            finish_bookkeeping(inner);
            true
        }
        Err(error) if error.code == openless_core::BackendErrorCode::InvalidState => false,
        Err(error) => {
            log::warn!("[coord] core dictation cancel failed: {error}");
            false
        }
    }
}

#[cfg(target_os = "windows")]
pub(super) fn windows_sendinput_options_from_prefs(
    preferences: &crate::types::UserPreferences,
) -> crate::unicode_keystroke::WindowsSendInputOptions {
    crate::unicode_keystroke::WindowsSendInputOptions {
        newline_mode: preferences.windows_sendinput_newline_mode,
    }
}
