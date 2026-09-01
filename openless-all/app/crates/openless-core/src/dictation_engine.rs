//! Shared dictation pipeline orchestration.
//!
//! The pipeline owns provider/recorder ordering, terminal progress events and
//! cancellation guards. Native hosts only implement the narrow ports from
//! [`crate::ports`]; they never duplicate session or fallback decisions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use futures_util::future::BoxFuture;

use crate::dictation_context::DictationContext;
use crate::errors::{BackendError, BackendErrorCode};
use crate::ports::{
    ActiveRecording, AudioConsumer, AudioRecorder, DictationEngine, EngineFailure,
    EngineFailureStage, EngineProgress, EngineProgressSink, EngineResult, EngineStage,
    RecordingProgressSink, TextPolisher, TextStreamChunk, TextStreamSink, TranscriptionEngine,
    TranscriptionSession,
};
use crate::types::{PolishDelta, SessionId, TranscriptDelta};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolishFailurePolicy {
    Fail,
    UseRawText,
}

pub struct PipelineDictationEngine {
    recorder: Arc<dyn AudioRecorder>,
    transcription: Arc<dyn TranscriptionEngine>,
    polisher: Arc<dyn TextPolisher>,
    polish_failure_policy: PolishFailurePolicy,
    sessions: Arc<Mutex<HashMap<SessionId, Arc<PipelineSession>>>>,
}

struct PipelineSession {
    context: RwLock<Arc<DictationContext>>,
    cancelled: AtomicBool,
    finishing: AtomicBool,
    transcription_finished: AtomicBool,
    transcription_cancelled: AtomicBool,
    polishing: AtomicBool,
    polisher_cancelled: AtomicBool,
    resources: Mutex<PipelineResources>,
}

#[derive(Default)]
struct PipelineResources {
    recording: Option<Box<dyn ActiveRecording>>,
    transcription: Option<Arc<dyn TranscriptionSession>>,
}

impl PipelineSession {
    fn new(context: Arc<DictationContext>) -> Self {
        Self {
            context: RwLock::new(context),
            cancelled: AtomicBool::new(false),
            finishing: AtomicBool::new(false),
            transcription_finished: AtomicBool::new(false),
            transcription_cancelled: AtomicBool::new(false),
            polishing: AtomicBool::new(false),
            polisher_cancelled: AtomicBool::new(false),
            resources: Mutex::new(PipelineResources::default()),
        }
    }

    fn context(&self) -> Arc<DictationContext> {
        Arc::clone(&self.context.read().expect("pipeline context lock poisoned"))
    }

    fn update_context(&self, context: Arc<DictationContext>) -> Result<(), BackendError> {
        if self.finishing.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire) {
            return Err(BackendError::new(
                BackendErrorCode::InvalidState,
                "dictation pipeline context can only change before finalization",
            ));
        }
        *self
            .context
            .write()
            .expect("pipeline context lock poisoned") = context;
        Ok(())
    }
}

impl PipelineDictationEngine {
    pub fn new(
        recorder: Arc<dyn AudioRecorder>,
        transcription: Arc<dyn TranscriptionEngine>,
        polisher: Arc<dyn TextPolisher>,
    ) -> Self {
        Self {
            recorder,
            transcription,
            polisher,
            polish_failure_policy: PolishFailurePolicy::UseRawText,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_polish_failure_policy(mut self, policy: PolishFailurePolicy) -> Self {
        self.polish_failure_policy = policy;
        self
    }
}

impl DictationEngine for PipelineDictationEngine {
    fn start(
        &self,
        session_id: SessionId,
        context: Arc<DictationContext>,
        progress: Arc<dyn EngineProgressSink>,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let recorder = Arc::clone(&self.recorder);
        let transcription_engine = Arc::clone(&self.transcription);
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move {
            let session = Arc::new(PipelineSession::new(Arc::clone(&context)));
            {
                let mut active = sessions.lock().expect("pipeline session lock poisoned");
                match active.entry(session_id) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(Arc::clone(&session));
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {
                        return Err(BackendError::new(
                            BackendErrorCode::Busy,
                            "dictation pipeline session already exists",
                        ));
                    }
                }
            }

            let transcript_partials: Arc<dyn TextStreamSink> =
                Arc::new(TranscriptProgressForwarder {
                    session_id,
                    progress: Arc::clone(&progress),
                });
            let transcription = match transcription_engine
                .start(session_id, Arc::clone(&context), transcript_partials)
                .await
            {
                Ok(transcription) => transcription,
                Err(error) => {
                    remove_session(&sessions, session_id, &session);
                    return Err(error);
                }
            };

            {
                let mut resources = session
                    .resources
                    .lock()
                    .expect("pipeline resource lock poisoned");
                if !session.cancelled.load(Ordering::Acquire) {
                    resources.transcription = Some(Arc::clone(&transcription));
                }
            }
            if session.cancelled.load(Ordering::Acquire) {
                let cancel_result =
                    cancel_transcription_once(&session, Arc::clone(&transcription)).await;
                remove_session(&sessions, session_id, &session);
                cancel_result?;
                return Err(cancelled_error(
                    "dictation cancelled while ASR was starting",
                ));
            }

            let audio_consumer: Arc<dyn AudioConsumer> = Arc::new(SessionAudioConsumer {
                session: Arc::clone(&transcription),
            });
            let recording_progress: Arc<dyn RecordingProgressSink> =
                Arc::new(RecordingProgressForwarder {
                    session_id,
                    progress,
                });
            let recording = match recorder
                .start(session_id, context, audio_consumer, recording_progress)
                .await
            {
                Ok(recording) => recording,
                Err(error) => {
                    let _ = cancel_transcription_once(&session, transcription).await;
                    remove_session(&sessions, session_id, &session);
                    return Err(error);
                }
            };

            let mut recording = Some(recording);
            {
                let mut resources = session
                    .resources
                    .lock()
                    .expect("pipeline resource lock poisoned");
                if !session.cancelled.load(Ordering::Acquire) {
                    resources.recording = recording.take();
                }
            }
            if let Some(recording) = recording {
                let stop_result = recording.stop().await;
                let cancel_result = cancel_transcription_once(&session, transcription).await;
                remove_session(&sessions, session_id, &session);
                stop_result?;
                cancel_result?;
                return Err(cancelled_error(
                    "dictation cancelled while the recorder was starting",
                ));
            }
            Ok(())
        })
    }

    fn start_transcription(
        &self,
        session_id: SessionId,
        context: Arc<DictationContext>,
        partials: Arc<dyn TextStreamSink>,
    ) -> BoxFuture<'static, Result<Arc<dyn TranscriptionSession>, BackendError>> {
        self.transcription.start(session_id, context, partials)
    }

    fn finish(
        &self,
        session_id: SessionId,
        progress: Arc<dyn EngineProgressSink>,
    ) -> BoxFuture<'static, Result<EngineResult, EngineFailure>> {
        let sessions = Arc::clone(&self.sessions);
        let polisher = Arc::clone(&self.polisher);
        let policy = self.polish_failure_policy;
        Box::pin(async move {
            let session = find_session(&sessions, session_id)?;
            if session.finishing.swap(true, Ordering::AcqRel) {
                return Err(BackendError::new(
                    BackendErrorCode::Busy,
                    "dictation pipeline is already finishing",
                )
                .into());
            }
            if session.cancelled.load(Ordering::Acquire) {
                remove_session(&sessions, session_id, &session);
                return Err(cancelled_error("dictation was cancelled before finishing").into());
            }
            let context = session.context();

            let (recording, transcription) = {
                let mut resources = session
                    .resources
                    .lock()
                    .expect("pipeline resource lock poisoned");
                (resources.recording.take(), resources.transcription.clone())
            };
            let recording = recording.ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::InvalidState,
                    "dictation recorder has not started",
                )
            })?;
            let transcription = transcription.ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::InvalidState,
                    "transcription session has not started",
                )
            })?;

            let archive = recording.archive();
            let mut has_audio_recording = archive.as_ref().map(|archive| archive.is_available());
            if let Err(error) = recording.stop().await {
                let _ = cancel_transcription_once(&session, transcription).await;
                remove_session(&sessions, session_id, &session);
                let mut failure = EngineFailure::new(error, EngineFailureStage::Transcribing);
                failure.has_audio_recording = has_audio_recording;
                return Err(failure);
            }
            if session.cancelled.load(Ordering::Acquire) {
                let _ = cancel_transcription_once(&session, transcription).await;
                remove_session(&sessions, session_id, &session);
                return Err(cancelled_error(
                    "dictation was cancelled before transcription finished",
                )
                .into());
            }

            publish_progress(
                &session,
                session_id,
                &progress,
                EngineProgress::Stage(EngineStage::Transcribing),
            )?;
            let asr_started = std::time::Instant::now();
            let transcript = match transcription.finish().await {
                Ok(transcript) => {
                    session
                        .transcription_finished
                        .store(true, Ordering::Release);
                    transcript
                }
                Err(error) => {
                    let asr_ms = Some(asr_started.elapsed().as_millis() as u64);
                    let cancelled = session.cancelled.load(Ordering::Acquire);
                    let _ = cancel_transcription_once(&session, transcription).await;
                    remove_session(&sessions, session_id, &session);
                    let error = if cancelled {
                        cancelled_error("dictation was cancelled while transcription was finishing")
                    } else {
                        error
                    };
                    let mut failure = EngineFailure::new(error, EngineFailureStage::Transcribing);
                    failure.asr_ms = asr_ms;
                    failure.has_audio_recording = has_audio_recording;
                    return Err(failure);
                }
            };
            let asr_ms = Some(asr_started.elapsed().as_millis() as u64);
            if session.cancelled.load(Ordering::Acquire) {
                remove_session(&sessions, session_id, &session);
                return Err(cancelled_error(
                    "dictation was cancelled after transcription finished",
                )
                .into());
            }
            if !context.record_audio_for_debug && !transcript.text.trim().is_empty() {
                if let Some(archive) = archive.as_ref() {
                    if archive.is_available() {
                        let _ = archive.discard().await;
                    }
                    has_audio_recording = Some(archive.is_available());
                }
            }
            publish_progress(
                &session,
                session_id,
                &progress,
                EngineProgress::TranscriptDelta(TranscriptDelta {
                    text: transcript.text.clone(),
                    offset: 0,
                    is_final: true,
                }),
            )?;
            let (polish_output, polish_failed, polish_ms) = if context.uses_llm_polisher() {
                publish_progress(
                    &session,
                    session_id,
                    &progress,
                    EngineProgress::Stage(EngineStage::Polishing),
                )?;

                session.polishing.store(true, Ordering::Release);
                if session.cancelled.load(Ordering::Acquire) {
                    let _ = cancel_polisher_once(&session, &polisher, session_id).await;
                    remove_session(&sessions, session_id, &session);
                    return Err(cancelled_error(
                        "dictation was cancelled before polishing started",
                    )
                    .into());
                }
                let polish_partials: Arc<dyn TextStreamSink> = Arc::new(PolishProgressForwarder {
                    session_id,
                    progress: Arc::clone(&progress),
                });
                let polish_started = std::time::Instant::now();
                let result = match polisher
                    .polish(
                        session_id,
                        context,
                        transcript.text.clone(),
                        polish_partials,
                    )
                    .await
                {
                    Ok(text) => (text, false),
                    Err(error) if can_fallback_to_raw(policy, &error) => (
                        crate::ports::PolishOutput::text(transcript.text.clone()),
                        true,
                    ),
                    Err(error) => {
                        let polish_ms = Some(polish_started.elapsed().as_millis() as u64);
                        let cancelled = session.cancelled.load(Ordering::Acquire);
                        let _ = cancel_polisher_once(&session, &polisher, session_id).await;
                        remove_session(&sessions, session_id, &session);
                        let error = if cancelled {
                            cancelled_error("dictation was cancelled while polishing was running")
                        } else {
                            error
                        };
                        let mut failure = EngineFailure::new(error, EngineFailureStage::Polishing);
                        failure.raw_text = Some(transcript.text.clone());
                        failure.duration_ms = Some(transcript.duration_ms);
                        failure.asr_ms = asr_ms;
                        failure.polish_ms = polish_ms;
                        failure.has_audio_recording = has_audio_recording;
                        return Err(failure);
                    }
                };
                let polish_ms = Some(polish_started.elapsed().as_millis() as u64);
                session.polishing.store(false, Ordering::Release);
                (result.0, result.1, polish_ms)
            } else {
                (
                    crate::ports::PolishOutput::text(transcript.text.clone()),
                    false,
                    None,
                )
            };
            if session.cancelled.load(Ordering::Acquire) {
                remove_session(&sessions, session_id, &session);
                return Err(
                    cancelled_error("dictation was cancelled after polishing finished").into(),
                );
            }
            publish_progress(
                &session,
                session_id,
                &progress,
                EngineProgress::PolishDelta(PolishDelta {
                    text: polish_output.text.clone(),
                    offset: 0,
                    is_final: true,
                }),
            )?;

            remove_session(&sessions, session_id, &session);
            Ok(EngineResult {
                raw_text: transcript.text,
                polished_text: polish_output.text,
                polish_source: polish_output.source_text,
                duration_ms: transcript.duration_ms,
                polish_failed,
                asr_ms,
                polish_ms,
                has_audio_recording,
            })
        })
    }

    fn update_context(
        &self,
        session_id: SessionId,
        context: Arc<DictationContext>,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let session = self
            .sessions
            .lock()
            .expect("pipeline session lock poisoned")
            .get(&session_id)
            .cloned();
        Box::pin(async move {
            let session = session.ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::InvalidState,
                    "dictation pipeline session is not active",
                )
            })?;
            session.update_context(context)
        })
    }

    fn feed_audio(&self, session_id: SessionId, pcm: &[u8]) -> Result<(), BackendError> {
        self.recorder.feed_pcm(session_id, pcm)
    }

    fn cancel(&self, session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>> {
        let sessions = Arc::clone(&self.sessions);
        let polisher = Arc::clone(&self.polisher);
        Box::pin(async move {
            let Some(session) = sessions
                .lock()
                .expect("pipeline session lock poisoned")
                .get(&session_id)
                .cloned()
            else {
                return Ok(());
            };
            if session.cancelled.swap(true, Ordering::AcqRel) {
                return Ok(());
            }

            let (recording, transcription) = {
                let mut resources = session
                    .resources
                    .lock()
                    .expect("pipeline resource lock poisoned");
                (resources.recording.take(), resources.transcription.clone())
            };
            let mut first_error = None;
            if let Some(recording) = recording {
                retain_first_error(&mut first_error, recording.stop().await);
            }
            if let Some(transcription) = transcription {
                retain_first_error(
                    &mut first_error,
                    cancel_transcription_once(&session, transcription).await,
                );
            }
            if session.polishing.load(Ordering::Acquire) {
                retain_first_error(
                    &mut first_error,
                    cancel_polisher_once(&session, &polisher, session_id).await,
                );
            }
            remove_session(&sessions, session_id, &session);
            match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}

fn find_session(
    sessions: &Arc<Mutex<HashMap<SessionId, Arc<PipelineSession>>>>,
    session_id: SessionId,
) -> Result<Arc<PipelineSession>, BackendError> {
    sessions
        .lock()
        .expect("pipeline session lock poisoned")
        .get(&session_id)
        .cloned()
        .ok_or_else(|| {
            BackendError::new(
                BackendErrorCode::InvalidState,
                "dictation pipeline session is not active",
            )
        })
}

fn remove_session(
    sessions: &Arc<Mutex<HashMap<SessionId, Arc<PipelineSession>>>>,
    session_id: SessionId,
    expected: &Arc<PipelineSession>,
) {
    let mut sessions = sessions.lock().expect("pipeline session lock poisoned");
    if sessions
        .get(&session_id)
        .is_some_and(|current| Arc::ptr_eq(current, expected))
    {
        sessions.remove(&session_id);
    }
}

async fn cancel_transcription_once(
    session: &Arc<PipelineSession>,
    transcription: Arc<dyn TranscriptionSession>,
) -> Result<(), BackendError> {
    if session.transcription_finished.load(Ordering::Acquire)
        || session.transcription_cancelled.swap(true, Ordering::AcqRel)
    {
        return Ok(());
    }
    transcription.cancel().await
}

async fn cancel_polisher_once(
    session: &Arc<PipelineSession>,
    polisher: &Arc<dyn TextPolisher>,
    session_id: SessionId,
) -> Result<(), BackendError> {
    if session.polisher_cancelled.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    polisher.cancel(session_id).await
}

fn publish_progress(
    session: &Arc<PipelineSession>,
    session_id: SessionId,
    progress: &Arc<dyn EngineProgressSink>,
    event: EngineProgress,
) -> Result<(), BackendError> {
    if session.cancelled.load(Ordering::Acquire) {
        return Err(cancelled_error(
            "dictation progress arrived after cancellation",
        ));
    }
    progress.publish(session_id, event)
}

fn can_fallback_to_raw(policy: PolishFailurePolicy, error: &BackendError) -> bool {
    policy == PolishFailurePolicy::UseRawText
        && matches!(
            error.code,
            BackendErrorCode::Provider | BackendErrorCode::Unsupported
        )
}

fn cancelled_error(message: &'static str) -> BackendError {
    BackendError::new(BackendErrorCode::Cancelled, message)
}

fn retain_first_error(first_error: &mut Option<BackendError>, result: Result<(), BackendError>) {
    if first_error.is_none() {
        if let Err(error) = result {
            *first_error = Some(error);
        }
    }
}

struct SessionAudioConsumer {
    session: Arc<dyn TranscriptionSession>,
}

impl AudioConsumer for SessionAudioConsumer {
    fn consume_pcm_chunk(&self, pcm: &[u8]) {
        self.session.consume_pcm_chunk(pcm);
    }
}

struct RecordingProgressForwarder {
    session_id: SessionId,
    progress: Arc<dyn EngineProgressSink>,
}

impl RecordingProgressSink for RecordingProgressForwarder {
    fn publish_level(&self, elapsed_ms: u64, level: f32) -> Result<(), BackendError> {
        self.progress.publish(
            self.session_id,
            EngineProgress::RecordingLevel {
                elapsed_ms,
                level: level.clamp(0.0, 1.0),
            },
        )
    }
}

struct TranscriptProgressForwarder {
    session_id: SessionId,
    progress: Arc<dyn EngineProgressSink>,
}

impl TextStreamSink for TranscriptProgressForwarder {
    fn publish(&self, chunk: TextStreamChunk) -> Result<(), BackendError> {
        self.progress.publish(
            self.session_id,
            EngineProgress::TranscriptDelta(TranscriptDelta {
                text: chunk.text,
                offset: chunk.offset,
                is_final: false,
            }),
        )
    }
}

struct PolishProgressForwarder {
    session_id: SessionId,
    progress: Arc<dyn EngineProgressSink>,
}

impl TextStreamSink for PolishProgressForwarder {
    fn publish(&self, chunk: TextStreamChunk) -> Result<(), BackendError> {
        self.progress.publish(
            self.session_id,
            EngineProgress::PolishDelta(PolishDelta {
                text: chunk.text,
                offset: chunk.offset,
                is_final: false,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::ports::{RecordingArchive, TranscriptOutput};

    #[derive(Default)]
    struct RecordingProgress {
        events: Mutex<Vec<EngineProgress>>,
    }

    impl EngineProgressSink for RecordingProgress {
        fn publish(
            &self,
            _session_id: SessionId,
            progress: EngineProgress,
        ) -> Result<(), BackendError> {
            self.events.lock().unwrap().push(progress);
            Ok(())
        }
    }

    struct FixtureRecording {
        stops: Arc<AtomicUsize>,
        archive: Arc<FixtureArchive>,
    }

    struct FixtureArchive {
        available: AtomicBool,
        discards: Arc<AtomicUsize>,
    }

    impl RecordingArchive for FixtureArchive {
        fn is_available(&self) -> bool {
            self.available.load(Ordering::Acquire)
        }

        fn discard(&self) -> BoxFuture<'static, Result<(), BackendError>> {
            self.discards.fetch_add(1, Ordering::AcqRel);
            self.available.store(false, Ordering::Release);
            Box::pin(async { Ok(()) })
        }
    }

    impl ActiveRecording for FixtureRecording {
        fn archive(&self) -> Option<Arc<dyn RecordingArchive>> {
            Some(self.archive.clone())
        }

        fn stop(self: Box<Self>) -> BoxFuture<'static, Result<(), BackendError>> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    struct FixtureRecorder {
        stops: Arc<AtomicUsize>,
        archive: Arc<FixtureArchive>,
        fail: bool,
    }

    impl AudioRecorder for FixtureRecorder {
        fn start(
            &self,
            _session_id: SessionId,
            _context: Arc<DictationContext>,
            consumer: Arc<dyn AudioConsumer>,
            progress: Arc<dyn RecordingProgressSink>,
        ) -> BoxFuture<'static, Result<Box<dyn ActiveRecording>, BackendError>> {
            let stops = Arc::clone(&self.stops);
            let archive = Arc::clone(&self.archive);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    return Err(BackendError::new(
                        BackendErrorCode::Platform,
                        "fixture recorder failed",
                    ));
                }
                consumer.consume_pcm_chunk(&[1, 0, 2, 0]);
                progress.publish_level(25, 1.5)?;
                Ok(Box::new(FixtureRecording { stops, archive }) as Box<dyn ActiveRecording>)
            })
        }
    }

    struct FixtureTranscriptionSession {
        pcm: Arc<Mutex<Vec<u8>>>,
        cancels: Arc<AtomicUsize>,
        finish_entered: Option<Arc<tokio::sync::Notify>>,
        finish_release: Option<Arc<tokio::sync::Notify>>,
    }

    impl AudioConsumer for FixtureTranscriptionSession {
        fn consume_pcm_chunk(&self, pcm: &[u8]) {
            self.pcm.lock().unwrap().extend_from_slice(pcm);
        }
    }

    impl TranscriptionSession for FixtureTranscriptionSession {
        fn finish(&self) -> BoxFuture<'static, Result<TranscriptOutput, BackendError>> {
            let entered = self.finish_entered.clone();
            let release = self.finish_release.clone();
            Box::pin(async move {
                if let Some(entered) = entered {
                    entered.notify_one();
                }
                if let Some(release) = release {
                    release.notified().await;
                }
                Ok(TranscriptOutput {
                    text: "raw text".to_string(),
                    duration_ms: 25,
                })
            })
        }

        fn cancel(&self) -> BoxFuture<'static, Result<(), BackendError>> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    struct FixtureTranscriber {
        session: Arc<FixtureTranscriptionSession>,
    }

    impl TranscriptionEngine for FixtureTranscriber {
        fn start(
            &self,
            _session_id: SessionId,
            _context: Arc<DictationContext>,
            partials: Arc<dyn TextStreamSink>,
        ) -> BoxFuture<'static, Result<Arc<dyn TranscriptionSession>, BackendError>> {
            let session = Arc::clone(&self.session);
            Box::pin(async move {
                partials.publish(TextStreamChunk {
                    text: "raw".to_string(),
                    offset: 0,
                })?;
                Ok(session as Arc<dyn TranscriptionSession>)
            })
        }
    }

    struct FixturePolisher {
        result: Result<crate::ports::PolishOutput, BackendError>,
        calls: Arc<AtomicUsize>,
        cancels: Arc<AtomicUsize>,
        contexts: Arc<Mutex<Vec<Arc<DictationContext>>>>,
    }

    impl TextPolisher for FixturePolisher {
        fn polish(
            &self,
            _session_id: SessionId,
            context: Arc<DictationContext>,
            _raw_text: String,
            partials: Arc<dyn TextStreamSink>,
        ) -> BoxFuture<'static, Result<crate::ports::PolishOutput, BackendError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.contexts.lock().unwrap().push(context);
            let result = self.result.clone();
            Box::pin(async move {
                partials.publish(TextStreamChunk {
                    text: "polished".to_string(),
                    offset: 0,
                })?;
                result
            })
        }

        fn cancel(&self, _session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    struct FixtureParts {
        engine: PipelineDictationEngine,
        progress: Arc<RecordingProgress>,
        pcm: Arc<Mutex<Vec<u8>>>,
        recorder_stops: Arc<AtomicUsize>,
        archive_discards: Arc<AtomicUsize>,
        transcription_cancels: Arc<AtomicUsize>,
        polish_calls: Arc<AtomicUsize>,
        polish_contexts: Arc<Mutex<Vec<Arc<DictationContext>>>>,
    }

    fn fixture_engine(
        recorder_fails: bool,
        polish_result: Result<crate::ports::PolishOutput, BackendError>,
        finish_entered: Option<Arc<tokio::sync::Notify>>,
        finish_release: Option<Arc<tokio::sync::Notify>>,
    ) -> FixtureParts {
        let pcm = Arc::new(Mutex::new(Vec::new()));
        let recorder_stops = Arc::new(AtomicUsize::new(0));
        let archive_discards = Arc::new(AtomicUsize::new(0));
        let archive = Arc::new(FixtureArchive {
            available: AtomicBool::new(true),
            discards: Arc::clone(&archive_discards),
        });
        let transcription_cancels = Arc::new(AtomicUsize::new(0));
        let polish_calls = Arc::new(AtomicUsize::new(0));
        let polish_contexts = Arc::new(Mutex::new(Vec::new()));
        let transcriber = Arc::new(FixtureTranscriber {
            session: Arc::new(FixtureTranscriptionSession {
                pcm: Arc::clone(&pcm),
                cancels: Arc::clone(&transcription_cancels),
                finish_entered,
                finish_release,
            }),
        });
        let engine = PipelineDictationEngine::new(
            Arc::new(FixtureRecorder {
                stops: Arc::clone(&recorder_stops),
                archive,
                fail: recorder_fails,
            }),
            transcriber,
            Arc::new(FixturePolisher {
                result: polish_result,
                calls: Arc::clone(&polish_calls),
                cancels: Arc::new(AtomicUsize::new(0)),
                contexts: Arc::clone(&polish_contexts),
            }),
        );
        FixtureParts {
            engine,
            progress: Arc::new(RecordingProgress::default()),
            pcm,
            recorder_stops,
            archive_discards,
            transcription_cancels,
            polish_calls,
            polish_contexts,
        }
    }

    #[tokio::test]
    async fn pipeline_streams_pcm_progress_and_terminal_deltas() {
        let fixture = fixture_engine(
            false,
            Ok(crate::ports::PolishOutput::text("polished text")),
            None,
            None,
        );
        let session_id = SessionId::new();
        fixture
            .engine
            .start(
                session_id,
                Arc::new(DictationContext::default()),
                fixture.progress.clone(),
            )
            .await
            .unwrap();
        let result = fixture
            .engine
            .finish(session_id, fixture.progress.clone())
            .await
            .unwrap();

        assert_eq!(&*fixture.pcm.lock().unwrap(), &[1, 0, 2, 0]);
        assert_eq!(fixture.recorder_stops.load(Ordering::SeqCst), 1);
        assert_eq!(result.raw_text, "raw text");
        assert_eq!(result.polished_text, "polished text");
        let events = fixture.progress.events.lock().unwrap();
        assert!(events.contains(&EngineProgress::RecordingLevel {
            elapsed_ms: 25,
            level: 1.0,
        }));
        assert!(
            events.contains(&EngineProgress::TranscriptDelta(TranscriptDelta {
                text: "raw text".to_string(),
                offset: 0,
                is_final: true,
            }))
        );
        assert!(events.contains(&EngineProgress::PolishDelta(PolishDelta {
            text: "polished text".to_string(),
            offset: 0,
            is_final: true,
        })));
    }

    #[tokio::test]
    async fn pipeline_uses_the_updated_context_when_finalizing() {
        let fixture = fixture_engine(
            false,
            Ok(crate::ports::PolishOutput::text("translated text")),
            None,
            None,
        );
        let session_id = SessionId::new();
        let initial = Arc::new(DictationContext::default());
        fixture
            .engine
            .start(session_id, initial.clone(), fixture.progress.clone())
            .await
            .unwrap();
        let mut updated = (*initial).clone();
        updated.polish.translation_active = true;
        let updated = Arc::new(updated);

        fixture
            .engine
            .update_context(session_id, updated.clone())
            .await
            .unwrap();
        fixture
            .engine
            .finish(session_id, fixture.progress.clone())
            .await
            .unwrap();

        let contexts = fixture.polish_contexts.lock().unwrap();
        assert_eq!(contexts.as_slice(), &[updated]);
    }

    #[tokio::test]
    async fn pipeline_rejects_context_updates_after_finalization_starts() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let fixture = fixture_engine(
            false,
            Ok(crate::ports::PolishOutput::text("polished text")),
            Some(Arc::clone(&entered)),
            Some(Arc::clone(&release)),
        );
        let session_id = SessionId::new();
        fixture
            .engine
            .start(
                session_id,
                Arc::new(DictationContext::default()),
                fixture.progress.clone(),
            )
            .await
            .unwrap();
        let finish = fixture.engine.finish(session_id, fixture.progress.clone());
        let finish_task = tokio::spawn(finish);
        entered.notified().await;

        let error = fixture
            .engine
            .update_context(session_id, Arc::new(DictationContext::default()))
            .await
            .expect_err("context must be frozen after finish starts");
        assert_eq!(error.code, BackendErrorCode::InvalidState);

        release.notify_one();
        finish_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn successful_transcription_discards_archive_when_debug_recording_is_disabled() {
        let fixture = fixture_engine(
            false,
            Ok(crate::ports::PolishOutput::text("polished text")),
            None,
            None,
        );
        let session_id = SessionId::new();
        fixture
            .engine
            .start(
                session_id,
                Arc::new(DictationContext::default()),
                fixture.progress.clone(),
            )
            .await
            .unwrap();

        let result = fixture
            .engine
            .finish(session_id, fixture.progress.clone())
            .await
            .unwrap();

        assert_eq!(fixture.archive_discards.load(Ordering::Acquire), 1);
        assert_eq!(result.has_audio_recording, Some(false));
    }

    #[tokio::test]
    async fn successful_transcription_preserves_archive_when_debug_recording_is_enabled() {
        let fixture = fixture_engine(
            false,
            Ok(crate::ports::PolishOutput::text("polished text")),
            None,
            None,
        );
        let session_id = SessionId::new();
        let context = DictationContext {
            record_audio_for_debug: true,
            ..DictationContext::default()
        };
        fixture
            .engine
            .start(session_id, Arc::new(context), fixture.progress.clone())
            .await
            .unwrap();

        let result = fixture
            .engine
            .finish(session_id, fixture.progress.clone())
            .await
            .unwrap();

        assert_eq!(fixture.archive_discards.load(Ordering::Acquire), 0);
        assert_eq!(result.has_audio_recording, Some(true));
    }

    #[tokio::test]
    async fn recorder_start_failure_cancels_transcription_exactly_once() {
        let fixture = fixture_engine(
            false,
            Ok(crate::ports::PolishOutput::text("unused")),
            None,
            None,
        );
        let failing = fixture_engine(
            true,
            Ok(crate::ports::PolishOutput::text("unused")),
            None,
            None,
        );
        let session_id = SessionId::new();
        let error = failing
            .engine
            .start(
                session_id,
                Arc::new(DictationContext::default()),
                failing.progress.clone(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, BackendErrorCode::Platform);
        assert_eq!(failing.transcription_cancels.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.transcription_cancels.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn provider_polish_failure_uses_raw_text_fallback() {
        let fixture = fixture_engine(
            false,
            Err(BackendError::new(
                BackendErrorCode::Provider,
                "fixture polish failure",
            )),
            None,
            None,
        );
        let session_id = SessionId::new();
        fixture
            .engine
            .start(
                session_id,
                Arc::new(DictationContext::default()),
                fixture.progress.clone(),
            )
            .await
            .unwrap();
        let result = fixture
            .engine
            .finish(session_id, fixture.progress.clone())
            .await
            .unwrap();
        assert_eq!(result.raw_text, "raw text");
        assert_eq!(result.polished_text, "raw text");
    }

    #[tokio::test]
    async fn duplicate_start_keeps_the_original_pipeline_session() {
        let fixture = fixture_engine(
            false,
            Ok(crate::ports::PolishOutput::text("polished text")),
            None,
            None,
        );
        let session_id = SessionId::new();
        fixture
            .engine
            .start(
                session_id,
                Arc::new(DictationContext::default()),
                fixture.progress.clone(),
            )
            .await
            .unwrap();

        let error = fixture
            .engine
            .start(
                session_id,
                Arc::new(DictationContext::default()),
                fixture.progress.clone(),
            )
            .await
            .expect_err("duplicate session must be rejected");
        assert_eq!(error.code, BackendErrorCode::Busy);

        let result = fixture
            .engine
            .finish(session_id, fixture.progress.clone())
            .await
            .expect("the original pipeline session must remain active");
        assert_eq!(result.polished_text, "polished text");
    }

    #[tokio::test]
    async fn builtin_raw_mode_bypasses_the_polisher() {
        let fixture = fixture_engine(
            false,
            Ok(crate::ports::PolishOutput::text("must not be used")),
            None,
            None,
        );
        let session_id = SessionId::new();
        let mut context = DictationContext::default();
        context.polish.mode = crate::types::PolishMode::Raw;
        context.polish.style_system_prompt =
            crate::style_packs::default_style_system_prompt_for_mode(crate::types::PolishMode::Raw);
        fixture
            .engine
            .start(session_id, Arc::new(context), fixture.progress.clone())
            .await
            .unwrap();
        let result = fixture
            .engine
            .finish(session_id, fixture.progress.clone())
            .await
            .unwrap();

        assert_eq!(result.polished_text, "raw text");
        assert_eq!(fixture.polish_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancel_while_asr_finishes_rejects_late_terminal_progress() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let fixture = fixture_engine(
            false,
            Ok(crate::ports::PolishOutput::text("polished text")),
            Some(Arc::clone(&entered)),
            Some(Arc::clone(&release)),
        );
        let session_id = SessionId::new();
        fixture
            .engine
            .start(
                session_id,
                Arc::new(DictationContext::default()),
                fixture.progress.clone(),
            )
            .await
            .unwrap();
        let finish = fixture.engine.finish(session_id, fixture.progress.clone());
        let finish_task = tokio::spawn(finish);
        entered.notified().await;
        fixture.engine.cancel(session_id).await.unwrap();
        release.notify_one();

        let error = finish_task.await.unwrap().unwrap_err();
        assert_eq!(error.error.code, BackendErrorCode::Cancelled);
        assert_eq!(fixture.transcription_cancels.load(Ordering::SeqCst), 1);
        let events = fixture.progress.events.lock().unwrap();
        assert!(!events.iter().any(|event| matches!(
            event,
            EngineProgress::TranscriptDelta(TranscriptDelta { is_final: true, .. })
        )));
    }
}
