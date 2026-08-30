//! Tauri-owned resources for the framework-independent QA service.
//!
//! Session phase, cancellation semantics and the message log belong to
//! `openless-core::QaService`. This module only captures host context, owns the
//! native recorder/provider handles and translates their results.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use openless_core::{
    ActiveRecording, AudioConsumer, AudioRecorder, BackendError, BackendErrorCode,
    DictationContext, DictationStartOptions, QaInput, QaProgress, QaProgressSink, QaRuntimeAdapter,
    QaRuntimeCompletion, QaTurnRequest, QaTurnResult, RecordingProgressSink, SelectionCapture,
    SelectionVoiceEditRequest, SessionId, TextStreamChunk, TextStreamSink, TranscriptionEngine,
    TranscriptionSession,
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
        let binder = self.selection_voice_target_binder.lock().clone().ok_or_else(|| {
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
    recorder: Arc<dyn AudioRecorder>,
    transcription: Arc<dyn TranscriptionEngine>,
    credentials: Arc<dyn openless_core::CredentialStore>,
    host_context: Arc<TauriQaHostContext>,
    sessions: Arc<Mutex<HashMap<SessionId, Arc<TauriQaRuntimeSession>>>>,
}

struct TauriQaRuntimeSession {
    context: Arc<DictationContext>,
    recording: Mutex<Option<Box<dyn ActiveRecording>>>,
    transcription: Mutex<Option<Arc<dyn TranscriptionSession>>>,
    pcm: Arc<TauriQaPcmBuffer>,
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

#[derive(Default)]
struct TauriQaPcmBuffer {
    bytes: Mutex<Vec<u8>>,
}

impl TauriQaPcmBuffer {
    fn snapshot(&self) -> Vec<u8> {
        self.bytes.lock().clone()
    }

    fn duration_ms(&self) -> u64 {
        (self.bytes.lock().len() as u64).saturating_mul(1_000)
            / (u64::from(openless_core::DICTATION_SAMPLE_RATE) * 2)
    }
}

impl AudioConsumer for TauriQaPcmBuffer {
    fn consume_pcm_chunk(&self, pcm: &[u8]) {
        self.bytes.lock().extend_from_slice(pcm);
    }
}

struct TauriQaAudioFanout {
    transcription: Option<Arc<dyn TranscriptionSession>>,
    pcm: Arc<TauriQaPcmBuffer>,
}

impl AudioConsumer for TauriQaAudioFanout {
    fn consume_pcm_chunk(&self, pcm: &[u8]) {
        if let Some(transcription) = &self.transcription {
            transcription.consume_pcm_chunk(pcm);
        }
        self.pcm.consume_pcm_chunk(pcm);
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

struct IgnoreTextStreamSink;

impl TextStreamSink for IgnoreTextStreamSink {
    fn publish(&self, _chunk: TextStreamChunk) -> Result<(), BackendError> {
        Ok(())
    }
}

impl TauriQaRuntimeAdapter {
    pub(crate) fn new(
        app: AppHandleSlot,
        backend: BackendSlot,
        recorder: Arc<dyn AudioRecorder>,
        transcription: Arc<dyn TranscriptionEngine>,
        credentials: Arc<dyn openless_core::CredentialStore>,
        host_context: Arc<TauriQaHostContext>,
    ) -> Self {
        Self {
            app,
            backend,
            recorder,
            transcription,
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
        voice_turn: bool,
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
            context,
            recording: Mutex::new(None),
            transcription: Mutex::new(None),
            pcm: Arc::new(TauriQaPcmBuffer::default()),
            audio_wav: Mutex::new(None),
            selection_text: capture.selection_text,
            selection_target: Mutex::new(Some(capture.selection_target)),
            front_app: capture.front_app,
            duration_ms: AtomicU64::new(0),
            voice_turn,
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

/// QA is ephemeral even when diagnostic recording is enabled for dictation.
/// Keep the archive handle before consuming the recording so every terminal
/// path can remove the file, including a recorder stop failure.
async fn stop_and_discard_recording(
    recording: Box<dyn ActiveRecording>,
) -> Result<(), BackendError> {
    let archive = recording.archive();
    let stop_result = recording.stop().await;
    let discard_result = if let Some(archive) = archive.filter(|archive| archive.is_available()) {
        archive.discard().await
    } else {
        Ok(())
    };
    match (stop_result, discard_result) {
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
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
            let session = adapter.capture_session(session_id, false).await?;
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
            let session = adapter.capture_session(session_id, true).await?;
            progress.publish(
                session_id,
                QaProgress::SelectionCaptured(session.selection_text.clone()),
            )?;
            let multimodal = session.context.pipeline_mode
                == openless_core::shared_types::PipelineMode::Multimodal;
            let transcription = if multimodal {
                None
            } else {
                match adapter
                    .transcription
                    .start(
                        session_id,
                        Arc::clone(&session.context),
                        Arc::new(IgnoreTextStreamSink),
                    )
                    .await
                {
                    Ok(transcription) => Some(transcription),
                    Err(error) => {
                        Self::remove_if_current(&adapter.sessions, session_id, &session);
                        return Err(error);
                    }
                }
            };
            if session.cancelled.load(Ordering::Acquire) {
                if let Some(transcription) = transcription {
                    let _ = transcription.cancel().await;
                }
                Self::remove_if_current(&adapter.sessions, session_id, &session);
                return Err(Self::cancelled_error());
            }
            *session.transcription.lock() = transcription.clone();
            let consumer: Arc<dyn AudioConsumer> = Arc::new(TauriQaAudioFanout {
                transcription,
                pcm: Arc::clone(&session.pcm),
            });
            let recording = match adapter
                .recorder
                .start(
                    session_id,
                    Arc::clone(&session.context),
                    consumer,
                    Arc::new(TauriQaRecordingProgress {
                        session_id,
                        progress,
                    }),
                )
                .await
            {
                Ok(recording) => recording,
                Err(error) => {
                    let transcription = session.transcription.lock().take();
                    if let Some(transcription) = transcription {
                        let _ = transcription.cancel().await;
                    }
                    Self::remove_if_current(&adapter.sessions, session_id, &session);
                    return Err(error);
                }
            };
            if session.cancelled.load(Ordering::Acquire) {
                let _ = stop_and_discard_recording(recording).await;
                let transcription = session.transcription.lock().take();
                if let Some(transcription) = transcription {
                    let _ = transcription.cancel().await;
                }
                Self::remove_if_current(&adapter.sessions, session_id, &session);
                return Err(Self::cancelled_error());
            }
            *session.recording.lock() = Some(recording);
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
            let recording = session.recording.lock().take().ok_or_else(|| {
                BackendError::new(BackendErrorCode::InvalidState, "QA recording is not ready")
            })?;
            stop_and_discard_recording(recording).await?;
            if session.cancelled.load(Ordering::Acquire) {
                return Err(Self::cancelled_error());
            }
            let duration_ms = session.pcm.duration_ms();
            session.duration_ms.store(duration_ms, Ordering::Release);
            if session.context.pipeline_mode
                == openless_core::shared_types::PipelineMode::Multimodal
            {
                let wav = openless_core::encode_dictation_wav(&session.pcm.snapshot())?;
                *session.audio_wav.lock() = Some(wav);
                return Ok(QaInput {
                    text: "（语音问题）".to_string(),
                    selection_text: session.selection_text.clone(),
                });
            }
            let transcription = session.transcription.lock().take().ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::InvalidState,
                    "QA transcription session is not ready",
                )
            })?;
            let transcript = transcription.finish().await?;
            session
                .duration_ms
                .store(transcript.duration_ms.max(duration_ms), Ordering::Release);
            Ok(QaInput {
                text: transcript.text,
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
                            source_app: session.context.polish.front_app.clone(),
                        },
                        instruction: request.input.text,
                    })
                    .await?;
                host_context
                    .bind_selection_voice_target(result.preview.session_id, target)?;
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
                Arc::clone(&session.context),
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
            Ok(QaRuntimeCompletion {
                duration_ms: session
                    .voice_turn
                    .then(|| session.duration_ms.load(Ordering::Acquire)),
                front_app: session.front_app.clone(),
                raw_transcript_override: (session.voice_turn
                    && session.context.pipeline_mode
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
            let recording = session.recording.lock().take();
            let transcription = session.transcription.lock().take();
            let mut first_error = None;
            if let Some(recording) = recording {
                if let Err(error) = stop_and_discard_recording(recording).await {
                    first_error = Some(error);
                }
            }
            if let Some(transcription) = transcription {
                if let Err(error) = transcription.cancel().await {
                    first_error.get_or_insert(error);
                }
            }
            match first_error {
                Some(error) => Err(error),
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
            recorder: Arc::clone(&self.recorder),
            transcription: Arc::clone(&self.transcription),
            credentials: Arc::clone(&self.credentials),
            host_context: Arc::clone(&self.host_context),
            sessions: Arc::clone(&self.sessions),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openless_core::RecordingArchive;
    use std::sync::atomic::AtomicUsize;

    struct FixtureArchive {
        available: AtomicBool,
        discards: Arc<AtomicUsize>,
        fail: bool,
    }

    impl RecordingArchive for FixtureArchive {
        fn is_available(&self) -> bool {
            self.available.load(Ordering::Acquire)
        }

        fn discard(&self) -> BoxFuture<'static, Result<(), BackendError>> {
            self.discards.fetch_add(1, Ordering::AcqRel);
            self.available.store(false, Ordering::Release);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(BackendError::new(
                        BackendErrorCode::Persistence,
                        "fixture archive discard failed",
                    ))
                } else {
                    Ok(())
                }
            })
        }
    }

    struct FixtureRecording {
        archive: Arc<FixtureArchive>,
        stops: Arc<AtomicUsize>,
        fail: bool,
    }

    impl ActiveRecording for FixtureRecording {
        fn archive(&self) -> Option<Arc<dyn RecordingArchive>> {
            Some(self.archive.clone())
        }

        fn stop(self: Box<Self>) -> BoxFuture<'static, Result<(), BackendError>> {
            self.stops.fetch_add(1, Ordering::AcqRel);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(BackendError::new(
                        BackendErrorCode::Platform,
                        "fixture recording stop failed",
                    ))
                } else {
                    Ok(())
                }
            })
        }
    }

    fn runtime_session() -> Arc<TauriQaRuntimeSession> {
        Arc::new(TauriQaRuntimeSession {
            context: Arc::new(DictationContext::default()),
            recording: Mutex::new(None),
            transcription: Mutex::new(None),
            pcm: Arc::new(TauriQaPcmBuffer::default()),
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

    #[tokio::test]
    async fn recording_archive_is_discarded_after_a_successful_stop() {
        let discards = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let archive = Arc::new(FixtureArchive {
            available: AtomicBool::new(true),
            discards: Arc::clone(&discards),
            fail: false,
        });
        let recording: Box<dyn ActiveRecording> = Box::new(FixtureRecording {
            archive,
            stops: Arc::clone(&stops),
            fail: false,
        });

        stop_and_discard_recording(recording).await.unwrap();

        assert_eq!(stops.load(Ordering::Acquire), 1);
        assert_eq!(discards.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn recording_archive_is_discarded_even_when_stop_fails() {
        let discards = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let archive = Arc::new(FixtureArchive {
            available: AtomicBool::new(true),
            discards: Arc::clone(&discards),
            fail: false,
        });
        let recording: Box<dyn ActiveRecording> = Box::new(FixtureRecording {
            archive,
            stops: Arc::clone(&stops),
            fail: true,
        });

        let error = stop_and_discard_recording(recording).await.unwrap_err();

        assert_eq!(error.code, BackendErrorCode::Platform);
        assert_eq!(stops.load(Ordering::Acquire), 1);
        assert_eq!(discards.load(Ordering::Acquire), 1);
    }
}
