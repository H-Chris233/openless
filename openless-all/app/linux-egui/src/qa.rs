use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use futures_util::future::BoxFuture;
use openless_core::{
    BackendError, BackendErrorCode, CredentialStore, DictationContext, DictationStartOptions,
    OpenLessBackend, QaInput, QaProgress, QaProgressSink, QaRuntimeAdapter, QaRuntimeCompletion,
    QaTurnRequest, QaTurnResult, RecordingProgressSink, SessionId,
};

pub(crate) type LinuxBackendSlot = Arc<Mutex<Weak<OpenLessBackend>>>;

pub(crate) fn backend_slot() -> LinuxBackendSlot {
    Arc::new(Mutex::new(Weak::new()))
}

pub(crate) fn bind_backend(slot: &LinuxBackendSlot, backend: &Arc<OpenLessBackend>) {
    *slot.lock().expect("Linux backend slot lock poisoned") = Arc::downgrade(backend);
}

#[derive(Clone)]
pub struct LinuxQaRuntime {
    // The backend owns this adapter through QaService. A Weak slot breaks that
    // ownership cycle while still letting host effects reuse Core's canonical
    // context/audio entry points after construction has completed.
    backend: LinuxBackendSlot,
    credentials: Arc<dyn CredentialStore>,
    // Core owns the QA phase, Busy rule and terminal event. This table owns
    // only opaque Host resources that must survive across async adapter calls.
    sessions: Arc<Mutex<HashMap<SessionId, Arc<LinuxQaSession>>>>,
}

struct LinuxQaSession {
    context: Mutex<Option<Arc<DictationContext>>>,
    voice_capture: Mutex<Option<openless_core::QaVoiceCaptureSession>>,
    audio_wav: Mutex<Option<Vec<u8>>>,
    selection_text: Option<String>,
    duration_ms: AtomicU64,
    voice_turn: bool,
    cancelled: Arc<AtomicBool>,
}

impl LinuxQaSession {
    fn context(&self) -> Result<Arc<DictationContext>, BackendError> {
        self.context
            .lock()
            .expect("Linux QA context lock poisoned")
            .clone()
            .ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::InvalidState,
                    "Linux QA session context is unavailable",
                )
            })
    }
}

struct LinuxQaRecordingProgress {
    session_id: SessionId,
    progress: Arc<dyn QaProgressSink>,
}

impl RecordingProgressSink for LinuxQaRecordingProgress {
    fn publish_level(&self, _elapsed_ms: u64, level: f32) -> Result<(), BackendError> {
        self.progress.publish(
            self.session_id,
            QaProgress::RecordingLevel(level.clamp(0.0, 1.0)),
        )
    }
}

impl LinuxQaRuntime {
    pub(crate) fn new(backend: LinuxBackendSlot, credentials: Arc<dyn CredentialStore>) -> Self {
        Self {
            backend,
            credentials,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn backend(&self) -> Result<Arc<OpenLessBackend>, BackendError> {
        self.backend
            .lock()
            .expect("Linux backend slot lock poisoned")
            .upgrade()
            .ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::InvalidState,
                    "Linux Core backend is not bound yet",
                )
            })
    }

    fn selection_text(session_id: SessionId) -> Option<String> {
        crate::fcitx5::capture_selection_target(&session_id.to_string())
            .ok()
            .filter(|text| !text.trim().is_empty())
    }

    fn insert_session(
        &self,
        session_id: SessionId,
        session: Arc<LinuxQaSession>,
    ) -> Result<(), BackendError> {
        let mut sessions = self
            .sessions
            .lock()
            .expect("Linux QA session lock poisoned");
        if sessions.contains_key(&session_id) {
            return Err(BackendError::new(
                BackendErrorCode::Busy,
                "Linux QA runtime session already exists",
            ));
        }
        sessions.insert(session_id, session);
        Ok(())
    }

    fn session(&self, session_id: SessionId) -> Result<Arc<LinuxQaSession>, BackendError> {
        self.sessions
            .lock()
            .expect("Linux QA session lock poisoned")
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::Cancelled,
                    "Linux QA runtime session is no longer active",
                )
            })
    }

    fn remove(&self, session_id: SessionId) -> Option<Arc<LinuxQaSession>> {
        self.sessions
            .lock()
            .expect("Linux QA session lock poisoned")
            .remove(&session_id)
    }

    async fn capture_text_session(
        &self,
        session_id: SessionId,
    ) -> Result<Arc<LinuxQaSession>, BackendError> {
        let context = self
            .backend()?
            .capture_host_dictation_context(DictationStartOptions::default())
            .await?;
        let session = Arc::new(LinuxQaSession {
            context: Mutex::new(Some(context)),
            voice_capture: Mutex::new(None),
            audio_wav: Mutex::new(None),
            selection_text: Self::selection_text(session_id),
            duration_ms: AtomicU64::new(0),
            voice_turn: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        self.insert_session(session_id, Arc::clone(&session))?;
        Ok(session)
    }

    fn cancelled_error() -> BackendError {
        BackendError::new(
            BackendErrorCode::Cancelled,
            "Linux QA runtime session was cancelled",
        )
    }
}

impl QaRuntimeAdapter for LinuxQaRuntime {
    fn prepare_text(
        &self,
        session_id: SessionId,
        text: String,
    ) -> BoxFuture<'static, Result<QaInput, BackendError>> {
        let runtime = self.clone();
        Box::pin(async move {
            let session = runtime.capture_text_session(session_id).await?;
            Ok(QaInput {
                text,
                selection_text: session.selection_text.clone(),
                selection_source_app: None,
            })
        })
    }

    fn start_recording(
        &self,
        session_id: SessionId,
        progress: Arc<dyn QaProgressSink>,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let runtime = self.clone();
        Box::pin(async move {
            let selection_text = Self::selection_text(session_id);
            let session = Arc::new(LinuxQaSession {
                context: Mutex::new(None),
                voice_capture: Mutex::new(None),
                audio_wav: Mutex::new(None),
                selection_text: selection_text.clone(),
                duration_ms: AtomicU64::new(0),
                voice_turn: true,
                cancelled: Arc::new(AtomicBool::new(false)),
            });
            runtime.insert_session(session_id, Arc::clone(&session))?;
            progress.publish(session_id, QaProgress::SelectionCaptured(selection_text))?;
            let capture = match runtime
                .backend()?
                .start_qa_voice_capture(
                    session_id,
                    DictationStartOptions::default(),
                    Arc::new(LinuxQaRecordingProgress {
                        session_id,
                        progress,
                    }),
                )
                .await
            {
                Ok(capture) => capture,
                Err(error) => {
                    runtime.remove(session_id);
                    return Err(error);
                }
            };
            *session
                .context
                .lock()
                .expect("Linux QA context lock poisoned") = Some(capture.context());
            *session
                .voice_capture
                .lock()
                .expect("Linux QA voice capture lock poisoned") = Some(capture);
            Ok(())
        })
    }

    fn finish_recording(
        &self,
        session_id: SessionId,
    ) -> BoxFuture<'static, Result<QaInput, BackendError>> {
        let session = self.session(session_id);
        Box::pin(async move {
            let session = session?;
            // `take()` transfers the recorder/transcriber lease to this one
            // finish operation. A concurrent Core cancel can remove the table
            // entry and flip `cancelled`, but it cannot finish the lease twice.
            let capture = session
                .voice_capture
                .lock()
                .expect("Linux QA voice capture lock poisoned")
                .take()
                .ok_or_else(|| {
                    BackendError::new(
                        BackendErrorCode::InvalidState,
                        "Linux QA recording is not ready",
                    )
                })?;
            let result = capture.finish().await?;
            if session.cancelled.load(Ordering::Acquire) {
                return Err(Self::cancelled_error());
            }
            session
                .duration_ms
                .store(result.duration_ms, Ordering::Release);
            *session
                .audio_wav
                .lock()
                .expect("Linux QA audio lock poisoned") = result.audio_wav;
            Ok(QaInput {
                text: result
                    .transcript
                    .unwrap_or_else(|| "（语音问题）".to_string()),
                selection_text: session.selection_text.clone(),
                selection_source_app: None,
            })
        })
    }

    fn answer(
        &self,
        request: QaTurnRequest,
        progress: Arc<dyn QaProgressSink>,
    ) -> BoxFuture<'static, Result<QaTurnResult, BackendError>> {
        let session = self.session(request.session_id);
        let credentials = Arc::clone(&self.credentials);
        Box::pin(async move {
            let session = session?;
            if session.cancelled.load(Ordering::Acquire) {
                return Err(Self::cancelled_error());
            }
            let audio_wav = session
                .audio_wav
                .lock()
                .expect("Linux QA audio lock poisoned")
                .take();
            let answer = openless_core::answer_qa_with_context(
                credentials,
                session.context()?,
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

    fn bind_selection_voice_target(
        &self,
        qa_session_id: SessionId,
        selection_voice_session_id: SessionId,
    ) -> Result<(), BackendError> {
        crate::fcitx5::rekey_selection_target(
            &qa_session_id.to_string(),
            &selection_voice_session_id.to_string(),
        )
    }

    fn complete(
        &self,
        session_id: SessionId,
    ) -> BoxFuture<'static, Result<QaRuntimeCompletion, BackendError>> {
        let session = self.remove(session_id);
        Box::pin(async move {
            let session = session.ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::Cancelled,
                    "Linux QA runtime session is no longer active",
                )
            })?;
            let _ = crate::fcitx5::cancel_selection_target(&session_id.to_string());
            let context = session.context()?;
            Ok(QaRuntimeCompletion {
                duration_ms: session
                    .voice_turn
                    .then(|| session.duration_ms.load(Ordering::Acquire)),
                raw_transcript_override: (session.voice_turn
                    && context.pipeline_mode
                        == openless_core::shared_types::PipelineMode::Multimodal)
                    .then(String::new),
                ..QaRuntimeCompletion::default()
            })
        })
    }

    fn cancel(&self, session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>> {
        let session = self.remove(session_id);
        Box::pin(async move {
            let Some(session) = session else {
                return Ok(());
            };
            let _ = crate::fcitx5::cancel_selection_target(&session_id.to_string());
            // Publish cancellation before taking the capture so a finish that
            // already owns the lease still rejects its late provider result.
            session.cancelled.store(true, Ordering::Release);
            let capture = session
                .voice_capture
                .lock()
                .expect("Linux QA voice capture lock poisoned")
                .take();
            match capture {
                Some(capture) => capture.cancel().await,
                None => Ok(()),
            }
        })
    }
}
