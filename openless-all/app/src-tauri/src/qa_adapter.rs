//! Tauri-owned resources for the framework-independent QA service.
//!
//! Session phase, cancellation semantics and the message log belong to
//! `openless-core::QaService`. This module only captures host context, owns the
//! selection/focus handles and translates Core results.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use openless_core::{
    BackendError, BackendErrorCode, DictationContext, DictationStartOptions, QaInput, QaProgress,
    QaProgressSink, QaRuntimeAdapter, QaRuntimeCompletion, QaTurnRequest, QaTurnResult,
    RecordingProgressSink, SelectionCapture, SelectionVoiceEditRequest, SessionId,
};
use parking_lot::Mutex;

use crate::core_adapters::{AppHandleSlot, BackendSlot};

pub(crate) type SelectionVoiceTargetBinder = Arc<
    dyn Fn(
            openless_core::SessionId,
            crate::selection::SelectionInsertionTarget,
        ) -> Result<(), String>
        + Send
        + Sync,
>;

#[derive(Default)]
pub(crate) struct TauriQaHostContext {
    focus_target: Mutex<Option<usize>>,
    front_app: Mutex<Option<String>>,
    panel_visible: AtomicBool,
    selection_voice_target_binder: Mutex<Option<SelectionVoiceTargetBinder>>,
}

impl TauriQaHostContext {
    pub(crate) fn is_panel_visible(&self) -> bool {
        self.panel_visible.load(Ordering::Acquire)
    }

    pub(crate) fn prepare_show(&self) {
        let was_visible = self.panel_visible.swap(true, Ordering::AcqRel);
        if let Some(target) = crate::coordinator::capture_external_focus_target() {
            *self.focus_target.lock() = Some(target);
        } else if !was_visible {
            *self.focus_target.lock() = crate::coordinator::capture_focus_target();
        }
        if let Some(front_app) = crate::coordinator::capture_frontmost_app() {
            *self.front_app.lock() = Some(front_app);
        }
    }

    pub(crate) fn clear(&self) {
        self.panel_visible.store(false, Ordering::Release);
        *self.focus_target.lock() = None;
        *self.front_app.lock() = None;
    }

    /// Bind the narrow host operation needed to attach an opaque selection
    /// insertion target to a Core-owned preview. The QA adapter must not look
    /// up `Coordinator` through Tauri managed state; the coordinator installs
    /// this host-scoped callback over shared opaque-target state during setup.
    pub(crate) fn set_selection_voice_target_binder(&self, binder: SelectionVoiceTargetBinder) {
        *self.selection_voice_target_binder.lock() = Some(binder);
    }

    fn bind_selection_voice_target(
        &self,
        session_id: openless_core::SessionId,
        insertion_target: crate::selection::SelectionInsertionTarget,
    ) -> Result<(), BackendError> {
        let binder = self
            .selection_voice_target_binder
            .lock()
            .clone()
            .ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::Unsupported,
                    "selection voice target binding is unavailable",
                )
            })?;
        binder(session_id, insertion_target)
            .map_err(|message| BackendError::new(BackendErrorCode::Platform, message))
    }

    fn capture_turn(&self, app: &AppHandleSlot) -> TauriQaHostCapture {
        if let Some(target) = crate::coordinator::capture_external_focus_target() {
            *self.focus_target.lock() = Some(target);
        }
        let _ = crate::coordinator::restore_focus_target_if_possible(*self.focus_target.lock());
        let (selection, selection_target) = crate::selection::resolve_selection_workspace_capture();
        if let Some(app) = app.lock().clone() {
            crate::refocus_qa_window(&app);
        }
        let selection_text = selection.as_ref().map(|selection| selection.text.clone());
        let front_app = selection
            .and_then(|selection| selection.source_app)
            .or_else(|| self.front_app.lock().clone());
        TauriQaHostCapture {
            selection_text,
            selection_target,
            front_app,
        }
    }
}

struct TauriQaHostCapture {
    selection_text: Option<String>,
    selection_target: crate::selection::SelectionInsertionTarget,
    front_app: Option<String>,
}

pub(crate) struct TauriQaRuntimeAdapter {
    app: AppHandleSlot,
    backend: BackendSlot,
    credentials: Arc<dyn openless_core::CredentialStore>,
    host_context: Arc<TauriQaHostContext>,
    sessions: Arc<Mutex<HashMap<SessionId, Arc<TauriQaRuntimeSession>>>>,
}

struct TauriQaRuntimeSession {
    context: Mutex<Option<Arc<DictationContext>>>,
    voice_capture: Mutex<Option<openless_core::QaVoiceCaptureSession>>,
    audio_wav: Mutex<Option<Vec<u8>>>,
    selection_text: Option<String>,
    selection_target: Mutex<Option<crate::selection::SelectionInsertionTarget>>,
    front_app: Option<String>,
    duration_ms: AtomicU64,
    voice_turn: bool,
    cancelled: Arc<AtomicBool>,
    edit_apply_available: AtomicBool,
    edit_revert_available: AtomicBool,
}

impl TauriQaRuntimeSession {
    fn context(&self) -> Result<Arc<DictationContext>, BackendError> {
        self.context.lock().clone().ok_or_else(|| {
            BackendError::new(
                BackendErrorCode::InvalidState,
                "QA session context is not ready",
            )
        })
    }
}

struct TauriQaRecordingProgress {
    session_id: SessionId,
    progress: Arc<dyn QaProgressSink>,
}

impl RecordingProgressSink for TauriQaRecordingProgress {
    fn publish_level(&self, _elapsed_ms: u64, level: f32) -> Result<(), BackendError> {
        self.progress.publish(
            self.session_id,
            QaProgress::RecordingLevel(level.clamp(0.0, 1.0)),
        )
    }
}

impl TauriQaRuntimeAdapter {
    pub(crate) fn new(
        app: AppHandleSlot,
        backend: BackendSlot,
        credentials: Arc<dyn openless_core::CredentialStore>,
        host_context: Arc<TauriQaHostContext>,
    ) -> Self {
        Self {
            app,
            backend,
            credentials,
            host_context,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn backend(&self) -> Result<Arc<openless_core::OpenLessBackend>, BackendError> {
        self.backend
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::InvalidState,
                    "core backend state is unavailable",
                )
            })
    }

    fn insert_session(
        sessions: &Arc<Mutex<HashMap<SessionId, Arc<TauriQaRuntimeSession>>>>,
        session_id: SessionId,
        session: Arc<TauriQaRuntimeSession>,
    ) -> Result<(), BackendError> {
        let mut sessions = sessions.lock();
        if sessions.contains_key(&session_id) {
            return Err(BackendError::new(
                BackendErrorCode::Busy,
                "QA runtime session already exists",
            ));
        }
        sessions.insert(session_id, session);
        Ok(())
    }

    fn remove_if_current(
        sessions: &Arc<Mutex<HashMap<SessionId, Arc<TauriQaRuntimeSession>>>>,
        session_id: SessionId,
        expected: &Arc<TauriQaRuntimeSession>,
    ) {
        let mut sessions = sessions.lock();
        if sessions
            .get(&session_id)
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            sessions.remove(&session_id);
        }
    }

    async fn capture_session(
        &self,
        session_id: SessionId,
    ) -> Result<Arc<TauriQaRuntimeSession>, BackendError> {
        let capture = self.host_context.capture_turn(&self.app);
        let context = self
            .backend()?
            .capture_host_dictation_context(DictationStartOptions {
                front_app: capture.front_app.clone(),
                ..DictationStartOptions::default()
            })
            .await?;
        let session = Arc::new(TauriQaRuntimeSession {
            context: Mutex::new(Some(context)),
            voice_capture: Mutex::new(None),
            audio_wav: Mutex::new(None),
            selection_text: capture.selection_text,
            selection_target: Mutex::new(Some(capture.selection_target)),
            front_app: capture.front_app,
            duration_ms: AtomicU64::new(0),
            voice_turn: false,
            cancelled: Arc::new(AtomicBool::new(false)),
            edit_apply_available: AtomicBool::new(false),
            edit_revert_available: AtomicBool::new(false),
        });
        Self::insert_session(&self.sessions, session_id, Arc::clone(&session))?;
        Ok(session)
    }

    fn cancelled_error() -> BackendError {
        BackendError::new(
            BackendErrorCode::Cancelled,
            "QA runtime session was cancelled",
        )
    }
}

impl QaRuntimeAdapter for TauriQaRuntimeAdapter {
    fn prepare_text(
        &self,
        session_id: SessionId,
        text: String,
    ) -> BoxFuture<'static, Result<QaInput, BackendError>> {
        let adapter = self.clone();
        Box::pin(async move {
            let session = adapter.capture_session(session_id).await?;
            Ok(QaInput {
                text,
                selection_text: session.selection_text.clone(),
            })
        })
    }

    fn start_recording(
        &self,
        session_id: SessionId,
        progress: Arc<dyn QaProgressSink>,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let adapter = self.clone();
        Box::pin(async move {
            let capture = adapter.host_context.capture_turn(&adapter.app);
            let session = Arc::new(TauriQaRuntimeSession {
                context: Mutex::new(None),
                voice_capture: Mutex::new(None),
                audio_wav: Mutex::new(None),
                selection_text: capture.selection_text,
                selection_target: Mutex::new(Some(capture.selection_target)),
                front_app: capture.front_app.clone(),
                duration_ms: AtomicU64::new(0),
                voice_turn: true,
                cancelled: Arc::new(AtomicBool::new(false)),
                edit_apply_available: AtomicBool::new(false),
                edit_revert_available: AtomicBool::new(false),
            });
            Self::insert_session(&adapter.sessions, session_id, Arc::clone(&session))?;
            if let Err(error) = progress.publish(
                session_id,
                QaProgress::SelectionCaptured(session.selection_text.clone()),
            ) {
                Self::remove_if_current(&adapter.sessions, session_id, &session);
                return Err(error);
            }
            let backend = match adapter.backend() {
                Ok(backend) => backend,
                Err(error) => {
                    Self::remove_if_current(&adapter.sessions, session_id, &session);
                    return Err(error);
                }
            };
            let voice_capture = match backend
                .start_qa_voice_capture(
                    session_id,
                    DictationStartOptions {
                        front_app: capture.front_app,
                        ..DictationStartOptions::default()
                    },
                    Arc::new(TauriQaRecordingProgress {
                        session_id,
                        progress,
                    }),
                )
                .await
            {
                Ok(capture) => capture,
                Err(error) => {
                    Self::remove_if_current(&adapter.sessions, session_id, &session);
                    return Err(error);
                }
            };
            *session.context.lock() = Some(voice_capture.context());
            *session.voice_capture.lock() = Some(voice_capture);
            if session.cancelled.load(Ordering::Acquire) {
                let voice_capture = session.voice_capture.lock().take();
                if let Some(voice_capture) = voice_capture {
                    let _ = voice_capture.cancel().await;
                }
                Self::remove_if_current(&adapter.sessions, session_id, &session);
                return Err(Self::cancelled_error());
            }
            Ok(())
        })
    }

    fn finish_recording(
        &self,
        session_id: SessionId,
    ) -> BoxFuture<'static, Result<QaInput, BackendError>> {
        let session = self.sessions.lock().get(&session_id).cloned();
        Box::pin(async move {
            let session = session.ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::Cancelled,
                    "QA runtime session is no longer active",
                )
            })?;
            let voice_capture = session.voice_capture.lock().take().ok_or_else(|| {
                BackendError::new(BackendErrorCode::InvalidState, "QA recording is not ready")
            })?;
            let result = voice_capture.finish().await?;
            if session.cancelled.load(Ordering::Acquire) {
                return Err(Self::cancelled_error());
            }
            session
                .duration_ms
                .store(result.duration_ms, Ordering::Release);
            *session.audio_wav.lock() = result.audio_wav;
            Ok(QaInput {
                text: result
                    .transcript
                    .unwrap_or_else(|| "（语音问题）".to_string()),
                selection_text: session.selection_text.clone(),
            })
        })
    }

    fn answer(
        &self,
        request: QaTurnRequest,
        progress: Arc<dyn QaProgressSink>,
    ) -> BoxFuture<'static, Result<QaTurnResult, BackendError>> {
        let session = self.sessions.lock().get(&request.session_id).cloned();
        let backend_slot = Arc::clone(&self.backend);
        let credentials = Arc::clone(&self.credentials);
        let host_context = Arc::clone(&self.host_context);
        Box::pin(async move {
            let session = session.ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::Cancelled,
                    "QA runtime session is no longer active",
                )
            })?;
            if session.cancelled.load(Ordering::Acquire) {
                return Err(Self::cancelled_error());
            }
            let context = session.context()?;
            if request.edit_instruction_mode {
                let selection_text = request
                    .input
                    .selection_text
                    .clone()
                    .filter(|text| !text.trim().is_empty())
                    .ok_or_else(|| {
                        BackendError::new(
                            BackendErrorCode::InvalidArgument,
                            "no selection is available for editing",
                        )
                    })?;
                let target = session.selection_target.lock().take().ok_or_else(|| {
                    BackendError::new(
                        BackendErrorCode::Platform,
                        "selection edit target is unavailable",
                    )
                })?;
                let backend = backend_slot
                    .lock()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .ok_or_else(|| {
                        BackendError::new(
                            BackendErrorCode::InvalidState,
                            "core backend state is unavailable",
                        )
                    })?;
                let result = backend
                    .services()
                    .selection_voice
                    .edit_preview(SelectionVoiceEditRequest {
                        owner_session_id: request.conversation_id,
                        capture: SelectionCapture {
                            text: selection_text,
                            source_app: context.polish.front_app.clone(),
                        },
                        instruction: request.input.text,
                    })
                    .await?;
                host_context.bind_selection_voice_target(result.preview.session_id, target)?;
                session.edit_apply_available.store(true, Ordering::Release);
                session
                    .edit_revert_available
                    .store(result.replaced_existing, Ordering::Release);
                return Ok(QaTurnResult {
                    answer: result.answer_text(),
                });
            }
            let audio_wav = session.audio_wav.lock().take();
            let answer = openless_core::answer_qa_with_context(
                credentials,
                context,
                request.messages,
                audio_wav,
                request.session_id,
                progress,
                Arc::clone(&session.cancelled),
            )
            .await?;
            Ok(QaTurnResult { answer })
        })
    }

    fn complete(
        &self,
        session_id: SessionId,
    ) -> BoxFuture<'static, Result<QaRuntimeCompletion, BackendError>> {
        let session = self.sessions.lock().remove(&session_id);
        Box::pin(async move {
            let session = session.ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::Cancelled,
                    "QA runtime session is no longer active",
                )
            })?;
            let context = session.context()?;
            Ok(QaRuntimeCompletion {
                duration_ms: session
                    .voice_turn
                    .then(|| session.duration_ms.load(Ordering::Acquire)),
                front_app: session.front_app.clone(),
                raw_transcript_override: (session.voice_turn
                    && context.pipeline_mode
                        == openless_core::shared_types::PipelineMode::Multimodal)
                    .then(String::new),
                edit_apply_available: session.edit_apply_available.load(Ordering::Acquire),
                edit_revert_available: session.edit_revert_available.load(Ordering::Acquire),
            })
        })
    }

    fn cancel(&self, session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>> {
        let session = self.sessions.lock().remove(&session_id);
        Box::pin(async move {
            let Some(session) = session else {
                return Ok(());
            };
            session.cancelled.store(true, Ordering::Release);
            let voice_capture = session.voice_capture.lock().take();
            match voice_capture {
                Some(voice_capture) => voice_capture.cancel().await,
                None => Ok(()),
            }
        })
    }
}

impl Clone for TauriQaRuntimeAdapter {
    fn clone(&self) -> Self {
        Self {
            app: Arc::clone(&self.app),
            backend: Arc::clone(&self.backend),
            credentials: Arc::clone(&self.credentials),
            host_context: Arc::clone(&self.host_context),
            sessions: Arc::clone(&self.sessions),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_session() -> Arc<TauriQaRuntimeSession> {
        Arc::new(TauriQaRuntimeSession {
            context: Mutex::new(Some(Arc::new(DictationContext::default()))),
            voice_capture: Mutex::new(None),
            audio_wav: Mutex::new(None),
            selection_text: None,
            selection_target: Mutex::new(Some(Default::default())),
            front_app: None,
            duration_ms: AtomicU64::new(0),
            voice_turn: false,
            cancelled: Arc::new(AtomicBool::new(false)),
            edit_apply_available: AtomicBool::new(false),
            edit_revert_available: AtomicBool::new(false),
        })
    }

    #[test]
    fn duplicate_session_is_rejected_without_replacing_the_original_owner() {
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let session_id = SessionId::new();
        let original = runtime_session();
        let replacement = runtime_session();

        TauriQaRuntimeAdapter::insert_session(&sessions, session_id, Arc::clone(&original))
            .unwrap();
        let error =
            TauriQaRuntimeAdapter::insert_session(&sessions, session_id, replacement).unwrap_err();

        assert_eq!(error.code, BackendErrorCode::Busy);
        assert!(Arc::ptr_eq(
            sessions.lock().get(&session_id).unwrap(),
            &original
        ));
    }

    #[test]
    fn host_visibility_follows_show_and_clear() {
        let context = TauriQaHostContext::default();
        assert!(!context.is_panel_visible());

        context.prepare_show();
        assert!(context.is_panel_visible());

        context.clear();
        assert!(!context.is_panel_visible());
    }

    #[test]
    fn selection_target_binding_uses_the_narrow_host_callback() {
        let context = TauriQaHostContext::default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&calls);
        context.set_selection_voice_target_binder(Arc::new(move |session_id, _target| {
            observed.lock().push(session_id);
            Ok(())
        }));

        let session_id = openless_core::SessionId::new();
        context
            .bind_selection_voice_target(session_id, Default::default())
            .unwrap();

        assert_eq!(*calls.lock(), vec![session_id]);
    }
}
