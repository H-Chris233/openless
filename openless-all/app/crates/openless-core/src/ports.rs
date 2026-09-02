use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use futures_util::future::BoxFuture;

use crate::dictation_context::DictationContext;
use crate::errors::{BackendError, BackendErrorCode};
use crate::types::{InsertStatus, PolishDelta, SessionId, TranscriptDelta};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAction {
    ShowMain,
    FocusMain,
    ShowDictationFeedback,
    HideDictationFeedback,
    ShowSelectionPreview,
    HideSelectionPreview,
    ShowQa,
    HideQa,
    ShowLessComputer,
    OpenExternalUrl(String),
    OpenSystemSettings(String),
    RequestRestart,
    Notify(String),
}

pub trait HostActions: Send + Sync {
    fn request(&self, action: HostAction) -> Result<(), BackendError>;
}

pub struct NoopHostActions;

impl HostActions for NoopHostActions {
    fn request(&self, _action: HostAction) -> Result<(), BackendError> {
        Ok(())
    }
}

/// Resolve a packaged resource without exposing a framework-specific resource
/// directory object to the core or UI.
pub trait ResourceResolver: Send + Sync {
    fn resolve(&self, relative: &Path) -> Result<PathBuf, BackendError>;
}

/// Directory-backed resolver shared by native hosts and tests.
#[derive(Debug, Clone)]
pub struct DirectoryResourceResolver {
    root: PathBuf,
}

impl DirectoryResourceResolver {
    pub fn new(root: PathBuf) -> Result<Self, BackendError> {
        if root.as_os_str().is_empty() {
            return Err(BackendError::new(
                crate::errors::BackendErrorCode::InvalidArgument,
                "resource root must not be empty",
            ));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl ResourceResolver for DirectoryResourceResolver {
    fn resolve(&self, relative: &Path) -> Result<PathBuf, BackendError> {
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(BackendError::new(
                crate::errors::BackendErrorCode::InvalidArgument,
                "resource path must be a non-empty relative path without parent traversal",
            ));
        }
        Ok(self.root.join(relative))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineResult {
    pub raw_text: String,
    pub polished_text: String,
    pub polish_source: Option<String>,
    pub duration_ms: u64,
    pub polish_failed: bool,
    pub asr_ms: Option<u64>,
    pub polish_ms: Option<u64>,
    pub has_audio_recording: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineFailureStage {
    Transcribing,
    Polishing,
}

#[derive(Debug, Clone)]
pub struct EngineFailure {
    pub error: BackendError,
    pub stage: EngineFailureStage,
    pub raw_text: Option<String>,
    pub duration_ms: Option<u64>,
    pub asr_ms: Option<u64>,
    pub polish_ms: Option<u64>,
    pub has_audio_recording: Option<bool>,
}

impl EngineFailure {
    pub fn new(error: BackendError, stage: EngineFailureStage) -> Self {
        Self {
            error,
            stage,
            raw_text: None,
            duration_ms: None,
            asr_ms: None,
            polish_ms: None,
            has_audio_recording: None,
        }
    }
}

impl From<BackendError> for EngineFailure {
    fn from(error: BackendError) -> Self {
        Self::new(error, EngineFailureStage::Transcribing)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolishOutput {
    pub text: String,
    pub source_text: Option<String>,
}

impl PolishOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            source_text: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineStage {
    Transcribing,
    Polishing,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineProgress {
    RecordingLevel { elapsed_ms: u64, level: f32 },
    Stage(EngineStage),
    TranscriptDelta(TranscriptDelta),
    PolishDelta(PolishDelta),
}

pub trait EngineProgressSink: Send + Sync {
    fn publish(&self, session_id: SessionId, progress: EngineProgress) -> Result<(), BackendError>;
}

pub trait DictationEngine: Send + Sync {
    fn start(
        &self,
        session_id: SessionId,
        context: Arc<DictationContext>,
        progress: Arc<dyn EngineProgressSink>,
    ) -> BoxFuture<'static, Result<(), BackendError>>;

    fn finish(
        &self,
        session_id: SessionId,
        progress: std::sync::Arc<dyn EngineProgressSink>,
    ) -> BoxFuture<'static, Result<EngineResult, EngineFailure>>;

    /// Replace the immutable session snapshot before finalization when a host
    /// action (currently Android's finish-and-translate gesture) is only known
    /// at stop time. Implementations that retain the context must override this
    /// method; settings are never re-read here.
    fn update_context(
        &self,
        _session_id: SessionId,
        _context: Arc<DictationContext>,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        Box::pin(async {
            Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "dictation engine does not support session context updates",
            ))
        })
    }

    /// Start only the provider-facing transcription side of a session.
    ///
    /// Voice Agent hosts feed canonical PCM themselves and therefore do not
    /// need the normal recorder/polisher pipeline. Implementations that own a
    /// transcription router can expose the same session-pinned provider here.
    fn start_transcription(
        &self,
        _session_id: SessionId,
        _context: Arc<DictationContext>,
        _partials: Arc<dyn TextStreamSink>,
    ) -> BoxFuture<'static, Result<Arc<dyn TranscriptionSession>, BackendError>> {
        Box::pin(async {
            Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "dictation engine does not expose a standalone transcription session",
            ))
        })
    }

    fn start_voice_capture(
        &self,
        _session_id: SessionId,
        _context: Arc<DictationContext>,
        _partials: Arc<dyn TextStreamSink>,
        _progress: Arc<dyn RecordingProgressSink>,
    ) -> BoxFuture<'static, Result<VoiceCapture, BackendError>> {
        Box::pin(async {
            Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "dictation engine does not expose a voice capture session",
            ))
        })
    }

    #[doc(hidden)]
    fn start_audio_capture(
        &self,
        _session_id: SessionId,
        _context: Arc<DictationContext>,
        _progress: Arc<dyn RecordingProgressSink>,
    ) -> BoxFuture<'static, Result<AudioCapture, BackendError>> {
        Box::pin(async {
            Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "dictation engine does not expose a recorder-only voice capture",
            ))
        })
    }

    /// Feed canonical PCM into an active externally sourced session.
    fn feed_audio(&self, _session_id: SessionId, _pcm: &[u8]) -> Result<(), BackendError> {
        Err(BackendError::new(
            BackendErrorCode::Unsupported,
            "dictation engine does not support external audio",
        ))
    }

    fn cancel(&self, session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>>;
}

pub struct VoiceCapture {
    pub recording: Box<dyn ActiveRecording>,
    pub transcription: Arc<dyn TranscriptionSession>,
}

pub struct AudioCapture {
    pub recording: Box<dyn ActiveRecording>,
    pub pcm: Arc<CapturedPcm>,
}

#[derive(Default)]
pub struct CapturedPcm {
    bytes: std::sync::Mutex<Vec<u8>>,
}

impl CapturedPcm {
    pub fn snapshot(&self) -> Vec<u8> {
        self.bytes
            .lock()
            .expect("captured PCM lock poisoned")
            .clone()
    }

    pub fn duration_ms(&self) -> u64 {
        (self.bytes.lock().expect("captured PCM lock poisoned").len() as u64).saturating_mul(1_000)
            / (u64::from(crate::DICTATION_SAMPLE_RATE) * 2)
    }
}

impl AudioConsumer for CapturedPcm {
    fn consume_pcm_chunk(&self, pcm: &[u8]) {
        self.bytes
            .lock()
            .expect("captured PCM lock poisoned")
            .extend_from_slice(pcm);
    }
}

/// Sink for canonical 16 kHz / mono / signed 16-bit little-endian PCM chunks.
pub trait AudioConsumer: Send + Sync {
    fn consume_pcm_chunk(&self, pcm: &[u8]);
}

/// Recording-time progress. Implementations must keep callbacks non-blocking.
pub trait RecordingProgressSink: Send + Sync {
    fn publish_level(&self, elapsed_ms: u64, level: f32) -> Result<(), BackendError>;
}

/// A recoverable recording archive owned by the platform adapter.
///
/// The handle outlives [`ActiveRecording::stop`] so the pipeline can preserve
/// failed recordings while discarding successful recordings according to the
/// immutable session policy.
pub trait RecordingArchive: Send + Sync {
    fn is_available(&self) -> bool;

    fn discard(&self) -> BoxFuture<'static, Result<(), BackendError>>;
}

/// One active recording resource. `stop` consumes the handle so release can
/// happen at most once even when finish and cancel race.
pub trait ActiveRecording: Send {
    /// Returns the exact archive created for this recording. `None` means the
    /// adapter does not expose archive capability.
    fn archive(&self) -> Option<Arc<dyn RecordingArchive>> {
        None
    }

    fn stop(self: Box<Self>) -> BoxFuture<'static, Result<(), BackendError>>;
}

/// Platform audio capture adapter. The core owns the canonical PCM contract;
/// each host owns device selection, permissions, resampling and the native
/// stream implementation.
pub trait AudioRecorder: Send + Sync {
    fn start(
        &self,
        session_id: SessionId,
        context: Arc<DictationContext>,
        consumer: Arc<dyn AudioConsumer>,
        progress: Arc<dyn RecordingProgressSink>,
    ) -> BoxFuture<'static, Result<Box<dyn ActiveRecording>, BackendError>>;

    /// Feed canonical PCM into an active externally sourced recording.
    fn feed_pcm(&self, _session_id: SessionId, _pcm: &[u8]) -> Result<(), BackendError> {
        Err(BackendError::new(
            BackendErrorCode::Unsupported,
            "audio recorder does not support external PCM",
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextStreamChunk {
    pub text: String,
    pub offset: u64,
}

/// Optional non-final provider deltas. The pipeline owns the final delta so
/// every implementation has identical terminal-event semantics.
pub trait TextStreamSink: Send + Sync {
    fn publish(&self, chunk: TextStreamChunk) -> Result<(), BackendError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptOutput {
    pub text: String,
    pub duration_ms: u64,
}

/// One provider transcription session that receives PCM while recording.
pub trait TranscriptionSession: AudioConsumer {
    fn asr_call_label(&self) -> Option<crate::auxiliary::AsrCallLabel> {
        None
    }

    fn finish(&self) -> BoxFuture<'static, Result<TranscriptOutput, BackendError>>;
    fn cancel(&self) -> BoxFuture<'static, Result<(), BackendError>>;
}

pub trait TranscriptionEngine: Send + Sync {
    fn start(
        &self,
        session_id: SessionId,
        context: Arc<DictationContext>,
        partials: Arc<dyn TextStreamSink>,
    ) -> BoxFuture<'static, Result<Arc<dyn TranscriptionSession>, BackendError>>;
}

pub trait TextPolisher: Send + Sync {
    fn polish(
        &self,
        session_id: SessionId,
        context: Arc<DictationContext>,
        raw_text: String,
        partials: Arc<dyn TextStreamSink>,
    ) -> BoxFuture<'static, Result<PolishOutput, BackendError>>;

    fn cancel(&self, session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>>;
}

pub trait TextInserter: Send + Sync {
    fn begin(
        &self,
        session_id: SessionId,
        context: Arc<DictationContext>,
    ) -> BoxFuture<'static, Result<Arc<dyn TextInsertionSession>, BackendError>>;
}

pub trait TextInsertionSession: Send + Sync {
    fn write(&self, text: String) -> BoxFuture<'static, Result<InsertWriteResult, BackendError>>;
    fn finish(&self, final_text: String)
        -> BoxFuture<'static, Result<InsertOutcome, BackendError>>;
    fn cancel(&self) -> BoxFuture<'static, Result<(), BackendError>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertWriteResult {
    pub written_chars: usize,
}

pub struct UnsupportedTextInserter;

impl TextInserter for UnsupportedTextInserter {
    fn begin(
        &self,
        _session_id: SessionId,
        _context: Arc<DictationContext>,
    ) -> BoxFuture<'static, Result<Arc<dyn TextInsertionSession>, BackendError>> {
        Box::pin(async {
            Err(BackendError::new(
                crate::errors::BackendErrorCode::Unsupported,
                "text inserter is not configured",
            ))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InsertOutcome {
    Inserted,
    CopiedFallback,
}

impl InsertOutcome {
    pub fn into_status(self) -> InsertStatus {
        match self {
            Self::Inserted => InsertStatus::Inserted,
            Self::CopiedFallback => InsertStatus::CopiedFallback,
        }
    }
}

pub fn boxed<F, T>(future: F) -> BoxFuture<'static, T>
where
    F: Future<Output = T> + Send + 'static,
{
    Box::pin(future)
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    #[test]
    fn directory_resolver_rejects_absolute_and_parent_paths() {
        let resolver = DirectoryResourceResolver::new(PathBuf::from("resources")).unwrap();
        assert_eq!(
            resolver.resolve(Path::new("models/card.json")).unwrap(),
            PathBuf::from("resources/models/card.json")
        );
        assert_eq!(
            resolver.resolve(Path::new("../secret")).unwrap_err().code,
            crate::errors::BackendErrorCode::InvalidArgument
        );
        let absolute = if cfg!(windows) {
            PathBuf::from("C:/secret")
        } else {
            PathBuf::from("/secret")
        };
        assert_eq!(
            resolver.resolve(&absolute).unwrap_err().code,
            crate::errors::BackendErrorCode::InvalidArgument
        );
    }
}
