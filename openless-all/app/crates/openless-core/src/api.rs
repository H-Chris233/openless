use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::activity::ActivityDay;
use crate::coding_agent::{
    normalize_coding_agent_executable, normalize_coding_agent_workdir,
    normalize_less_computer_permission_mode, resolve_coding_agent_model, CodingAgentProvider,
};
use crate::config::{BackendConfig, BackendDependencies, Clock, SystemClock};
use crate::correction::apply_correction_rules;
use crate::credentials::{
    ChannelKind, ChannelMutation, ChannelMutationResult, ChannelSummary, CredentialKey,
    ProviderSlot, SecretValue,
};
use crate::dictation_context::{
    DictationAudioSource, DictationContext, DictationProviderInvocations, DictationStartOptions,
    DictationStopOptions,
};
use crate::domains::{LessComputerRunRequest, LessComputerRunResult};
use crate::errors::{BackendError, BackendErrorCode};
use crate::events::{
    BackendEventKind, BackendEventPublisher, EventBus, EventReplay, EventSubscription,
};
use crate::ports::{
    EngineFailureStage, EngineProgress, EngineProgressSink, EngineStage, HostAction, InsertOutcome,
};
use crate::shared_types::{
    CredentialsStatus, PendingCorrection, UserPreferences, LEARNED_VOCAB_NOTE,
    MAX_PENDING_CORRECTIONS,
};
use crate::style_pack_store::sync_style_pack_preferences;
use crate::style_packs::StylePack;
use crate::types::{
    CorrectionRule, DictationPhase, DictationResult, DictationSession, DictationStateSnapshot,
    DictionaryEntry, HistoryChange, HistoryInsertStatus, HistorySource, PreferencesChange,
    SessionId, StylePackChange, VocabPresetStore, VocabularyChange,
};
use crate::vocabulary::DictionaryStore;
use crate::{ActivityStore, CorrectionRuleStore, HistoryStore, PreferencesStore, StylePackStore};

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendSnapshot {
    pub running: bool,
    pub dictation: DictationStateSnapshot,
    #[serde(default)]
    pub vocabulary_revision: u64,
    #[serde(default)]
    pub history_revision: u64,
    #[serde(default)]
    pub style_pack_revision: u64,
    #[serde(default)]
    pub preferences_revision: u64,
    #[serde(default)]
    pub credentials: CredentialsStatus,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupSnapshot {
    pub backend: BackendSnapshot,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum CliDispatchOutcome {
    DictationStarted(SessionId),
    DictationCompleted(DictationResult),
    QaToggled,
    DictationCancelled,
    Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictationHotkeyEdge {
    Pressed { at: std::time::Instant },
    Released { at: std::time::Instant },
    Combined,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DictationHotkeyDispatchOptions {
    pub start: DictationStartOptions,
    pub stop: DictationStopOptions,
}

/// Repository bundle shared by every host-facing facade in one process.
///
/// Tauri's compatibility coordinator and the new core facade must use the same
/// instances; opening the same JSON files twice would create divergent
/// in-memory snapshots even though the paths are identical.
#[derive(Clone)]
pub struct BackendRepositories {
    pub preferences: Arc<PreferencesStore>,
    pub history: Arc<HistoryStore>,
    pub activity: Arc<ActivityStore>,
    pub vocabulary: Arc<DictionaryStore>,
    pub correction_rules: Arc<CorrectionRuleStore>,
    pub style_packs: Arc<StylePackStore>,
}

impl BackendRepositories {
    pub fn open(data_dir: &std::path::Path) -> Result<Self, BackendError> {
        let preferences = Arc::new(PreferencesStore::open(data_dir.join("preferences.json"))?);
        let style_packs = Arc::new(
            StylePackStore::at_data_dir_with_preferences(data_dir, &preferences.get())
                .unwrap_or_else(|_| StylePackStore::in_memory()),
        );
        let mut preference_snapshot = preferences.get();
        if sync_style_pack_preferences(&mut preference_snapshot, &style_packs.list()?) {
            preferences.set(preference_snapshot)?;
        }
        Ok(Self {
            preferences,
            history: Arc::new(HistoryStore::at_data_dir(data_dir)),
            activity: Arc::new(
                ActivityStore::at_data_dir(data_dir).unwrap_or_else(|_| ActivityStore::in_memory()),
            ),
            vocabulary: Arc::new(DictionaryStore::at_data_dir(data_dir)),
            correction_rules: Arc::new(CorrectionRuleStore::at_data_dir(data_dir)),
            style_packs,
        })
    }
}

struct MutableState {
    running: bool,
    dictation: DictationStateSnapshot,
    dictation_context: Option<Arc<DictationContext>>,
    credentials: CredentialsStatus,
}

struct BackendEngineProgress {
    events: Arc<EventBus>,
    state: Arc<RwLock<MutableState>>,
    phase_changed: Arc<tokio::sync::Notify>,
}

impl EngineProgressSink for BackendEngineProgress {
    fn publish(&self, session_id: SessionId, progress: EngineProgress) -> Result<(), BackendError> {
        match progress {
            EngineProgress::RecordingLevel { elapsed_ms, level } => {
                let mut state = self.state.write().expect("backend state lock poisoned");
                ensure_active_session(&state, session_id)?;
                if !matches!(
                    state.dictation.phase,
                    DictationPhase::Starting | DictationPhase::Recording
                ) {
                    return Err(BackendError::new(
                        BackendErrorCode::InvalidState,
                        "recording progress arrived after recording stopped",
                    ));
                }
                let level = if level.is_finite() {
                    level.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if state.dictation.elapsed_ms != elapsed_ms || state.dictation.level != level {
                    state.dictation.elapsed_ms = elapsed_ms;
                    state.dictation.level = level;
                    self.events.publish(
                        Some(session_id),
                        BackendEventKind::DictationStateChanged(state.dictation.clone()),
                    );
                }
            }
            EngineProgress::Stage(stage) => {
                let mut state = self.state.write().expect("backend state lock poisoned");
                ensure_active_session(&state, session_id)?;
                let phase = match stage {
                    EngineStage::Transcribing => DictationPhase::Transcribing,
                    EngineStage::Polishing => DictationPhase::Polishing,
                };
                if state.dictation.phase != phase {
                    state.dictation.phase = phase;
                    self.events.publish(
                        Some(session_id),
                        BackendEventKind::DictationStateChanged(state.dictation.clone()),
                    );
                    self.phase_changed.notify_waiters();
                }
            }
            EngineProgress::TranscriptDelta(delta) => {
                let state = self.state.read().expect("backend state lock poisoned");
                ensure_active_session(&state, session_id)?;
                drop(state);
                self.events
                    .publish(Some(session_id), BackendEventKind::TranscriptDelta(delta));
            }
            EngineProgress::PolishDelta(delta) => {
                let state = self.state.read().expect("backend state lock poisoned");
                ensure_active_session(&state, session_id)?;
                drop(state);
                self.events
                    .publish(Some(session_id), BackendEventKind::PolishDelta(delta));
            }
        }
        Ok(())
    }
}

pub struct OpenLessBackend {
    config: BackendConfig,
    deps: BackendDependencies,
    clock: Arc<dyn Clock>,
    events: Arc<EventBus>,
    state: Arc<RwLock<MutableState>>,
    phase_changed: Arc<tokio::sync::Notify>,
    hotkey_press_at: Mutex<Option<std::time::Instant>>,
    vocabulary: Arc<DictionaryStore>,
    correction_rules: Arc<CorrectionRuleStore>,
    vocabulary_revision: Arc<AtomicU64>,
    history: Arc<HistoryStore>,
    history_revision: Arc<AtomicU64>,
    activity: Arc<ActivityStore>,
    style_packs: Arc<StylePackStore>,
    style_pack_revision: Arc<AtomicU64>,
    preferences: Arc<PreferencesStore>,
    preferences_revision: Arc<AtomicU64>,
    settings_write_gate: Mutex<()>,
    pending_corrections: Arc<Mutex<Vec<PendingCorrection>>>,
}

struct HistoryProviderAttribution {
    asr_provider: Option<String>,
    asr_model: Option<String>,
    llm_provider: Option<String>,
    llm_model: Option<String>,
    asr_ms: Option<u64>,
    polish_ms: Option<u64>,
}

fn settings_transaction_error(
    mut primary: BackendError,
    compensation_errors: Vec<BackendError>,
) -> BackendError {
    if compensation_errors.is_empty() {
        return primary;
    }
    primary.details = Some(serde_json::json!({
        "primaryError": primary.clone(),
        "compensationErrors": compensation_errors,
    }));
    primary
}

impl HistoryProviderAttribution {
    fn from_context(
        context: &DictationContext,
        llm_used: bool,
        asr_ms: Option<u64>,
        polish_ms: Option<u64>,
    ) -> Self {
        match context.pipeline_mode {
            crate::shared_types::PipelineMode::Traditional => Self {
                asr_provider: Some(context.asr.provider_id.clone()),
                asr_model: context.asr.model.clone(),
                llm_provider: llm_used.then(|| context.llm.provider_id.clone()),
                llm_model: llm_used.then(|| context.llm.model.clone()).flatten(),
                asr_ms,
                polish_ms,
            },
            crate::shared_types::PipelineMode::Multimodal => Self {
                asr_provider: None,
                asr_model: None,
                llm_provider: Some(context.omni.provider_id.clone()),
                llm_model: context.omni.model.clone(),
                asr_ms: None,
                polish_ms,
            },
        }
    }
}

impl OpenLessBackend {
    pub fn new(config: BackendConfig, deps: BackendDependencies) -> Result<Self, BackendError> {
        if config.data_dir.as_os_str().is_empty() {
            return Err(BackendError::new(
                BackendErrorCode::InvalidArgument,
                "data directory is required",
            ));
        }
        let repositories = BackendRepositories::open(&config.data_dir)?;
        Self::new_with_repositories_and_clock(config, deps, repositories, Arc::new(SystemClock))
    }

    pub fn new_with_clock(
        config: BackendConfig,
        deps: BackendDependencies,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, BackendError> {
        if config.data_dir.as_os_str().is_empty() {
            return Err(BackendError::new(
                BackendErrorCode::InvalidArgument,
                "data directory is required",
            ));
        }
        let repositories = BackendRepositories::open(&config.data_dir)?;
        Self::new_with_repositories_and_clock(config, deps, repositories, clock)
    }

    pub fn new_with_repositories(
        config: BackendConfig,
        deps: BackendDependencies,
        repositories: BackendRepositories,
    ) -> Result<Self, BackendError> {
        Self::new_with_repositories_and_clock(config, deps, repositories, Arc::new(SystemClock))
    }

    pub fn new_with_repositories_and_clock(
        config: BackendConfig,
        mut deps: BackendDependencies,
        repositories: BackendRepositories,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, BackendError> {
        if config.data_dir.as_os_str().is_empty() {
            return Err(BackendError::new(
                BackendErrorCode::InvalidArgument,
                "data directory is required",
            ));
        }
        let events = Arc::new(EventBus::new(256));
        let preferences_revision = Arc::new(AtomicU64::new(0));
        let style_pack_revision = Arc::new(AtomicU64::new(0));
        let history_revision = Arc::new(AtomicU64::new(0));
        let vocabulary_revision = Arc::new(AtomicU64::new(0));
        deps.services.selection_voice =
            Arc::new(crate::selection_voice_service::SelectionVoiceService::new(
                BackendEventPublisher::new(Arc::clone(&events)),
                Arc::clone(&repositories.preferences),
                Arc::clone(&repositories.history),
                Arc::clone(&history_revision),
                Arc::clone(&clock),
                Arc::clone(&repositories.vocabulary),
                Arc::clone(&vocabulary_revision),
                Arc::clone(&repositories.correction_rules),
                Arc::clone(&repositories.activity),
                Arc::clone(&deps.credential_store),
                deps.selection_polisher.clone(),
            ));
        if let Some(runtime) = deps.local_asr_runtime.take() {
            deps.services.local_asr = Arc::new(crate::local_asr_service::LocalAsrService::new(
                Arc::clone(&repositories.preferences),
                runtime,
                BackendEventPublisher::new(Arc::clone(&events)),
                Arc::clone(&preferences_revision),
            ));
        }
        if let Some(marketplace_config) = deps.marketplace_config.take() {
            deps.services.marketplace = Arc::new(crate::marketplace::MarketplaceService::new(
                marketplace_config,
                Arc::clone(&deps.credential_store),
                Arc::clone(&repositories.preferences),
                Arc::clone(&repositories.style_packs),
                BackendEventPublisher::new(Arc::clone(&events)),
                Arc::clone(&style_pack_revision),
            )?);
        }
        match (
            deps.selection_runtime.take(),
            deps.selection_polisher.take(),
        ) {
            (Some(runtime), Some(polisher)) => {
                deps.services.selection =
                    Arc::new(crate::selection_service::SelectionService::new(
                        crate::selection_service::SelectionServiceDependencies {
                            preferences: Arc::clone(&repositories.preferences),
                            style_packs: Arc::clone(&repositories.style_packs),
                            runtime,
                            polisher,
                            host_actions: Arc::clone(&deps.host_actions),
                            events: BackendEventPublisher::new(Arc::clone(&events)),
                            history: Arc::clone(&repositories.history),
                            history_revision: Arc::clone(&history_revision),
                            clock: Arc::clone(&clock),
                            vocabulary: Arc::clone(&repositories.vocabulary),
                            vocabulary_revision: Arc::clone(&vocabulary_revision),
                            correction_rules: Arc::clone(&repositories.correction_rules),
                            activity: Arc::clone(&repositories.activity),
                            credential_store: Arc::clone(&deps.credential_store),
                        },
                    ));
            }
            (None, None) => {}
            _ => {
                return Err(BackendError::new(
                    BackendErrorCode::InvalidArgument,
                    "selection runtime and polisher must be configured together",
                ));
            }
        }
        if let Some(runtime) = deps.qa_runtime.take() {
            deps.services.qa = Arc::new(crate::qa_service::QaService::new_with_persistence(
                runtime,
                Arc::clone(&deps.host_actions),
                Arc::clone(&repositories.preferences),
                Arc::clone(&repositories.history),
                Arc::clone(&history_revision),
                Arc::clone(&clock),
                Arc::clone(&deps.services.selection_voice),
            ));
        }
        if let Some((polisher, transcription)) = deps.services.take_auxiliary_runtime() {
            deps.services.auxiliary = Arc::new(crate::auxiliary::AuxiliaryService::new(
                Arc::clone(&repositories.preferences),
                Arc::clone(&repositories.style_packs),
                Arc::clone(&repositories.vocabulary),
                Arc::clone(&deps.credential_store),
                polisher,
                transcription,
                Arc::clone(&deps.task_spawner),
            ));
        }
        deps.services
            .qa
            .bind_event_publisher(BackendEventPublisher::new(Arc::clone(&events)));
        deps.services
            .less_computer
            .bind_event_publisher(BackendEventPublisher::new(Arc::clone(&events)));
        deps.services
            .remote_input
            .bind_event_publisher(BackendEventPublisher::new(Arc::clone(&events)));
        Ok(Self {
            config,
            deps,
            clock,
            events,
            state: Arc::new(RwLock::new(MutableState {
                running: false,
                dictation: DictationStateSnapshot::default(),
                dictation_context: None,
                credentials: CredentialsStatus::default(),
            })),
            phase_changed: Arc::new(tokio::sync::Notify::new()),
            hotkey_press_at: Mutex::new(None),
            vocabulary: repositories.vocabulary,
            correction_rules: repositories.correction_rules,
            vocabulary_revision,
            history: repositories.history,
            history_revision,
            activity: repositories.activity,
            style_packs: repositories.style_packs,
            style_pack_revision,
            preferences: repositories.preferences,
            preferences_revision,
            settings_write_gate: Mutex::new(()),
            pending_corrections: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn repositories(&self) -> BackendRepositories {
        BackendRepositories {
            preferences: Arc::clone(&self.preferences),
            history: Arc::clone(&self.history),
            activity: Arc::clone(&self.activity),
            vocabulary: Arc::clone(&self.vocabulary),
            correction_rules: Arc::clone(&self.correction_rules),
            style_packs: Arc::clone(&self.style_packs),
        }
    }

    pub fn config(&self) -> &BackendConfig {
        &self.config
    }

    /// Return the versioned domain interfaces used by non-Tauri hosts.
    ///
    /// Each service is an independently replaceable port.  A missing adapter
    /// returns `BackendErrorCode::Unsupported`; callers never need to inspect
    /// the concrete implementation.
    pub fn services(&self) -> &crate::domains::BackendServices {
        &self.deps.services
    }

    /// Reserve one Less Computer voice-capture session before a host starts
    /// its recorder/native ASR resources.
    ///
    /// Core owns the session lease and cancellation identity; the host owns
    /// only the platform capture handles.  The same `session_id` must later be
    /// passed to [`Self::submit_less_computer_with_session`], or released with
    /// [`Self::abort_less_computer_capture`].
    pub fn begin_less_computer_capture(&self, session_id: SessionId) -> Result<(), BackendError> {
        if !self.get_preferences().coding_agent_enabled {
            return Err(BackendError::new(
                BackendErrorCode::PermissionDenied,
                "Less Computer is disabled",
            )
            .retryable(false));
        }
        let dictation_phase = self.snapshot().dictation.phase;
        if dictation_phase != DictationPhase::Idle {
            return Err(BackendError::new(
                BackendErrorCode::Busy,
                "dictation is already active",
            ));
        }
        self.deps.services.less_computer.begin_capture(session_id)
    }

    /// Return the current Less Computer capture/run session, if any.
    pub fn less_computer_active_session(&self) -> Option<SessionId> {
        self.deps.services.less_computer.active_session()
    }

    /// Return whether Core has received cancellation for the host capture.
    pub fn less_computer_capture_cancelled(&self, session_id: SessionId) -> bool {
        self.deps
            .services
            .less_computer
            .capture_cancelled(session_id)
    }

    /// Release a capture lease that did not reach Agent submission.
    pub fn abort_less_computer_capture(&self, session_id: SessionId) -> Result<(), BackendError> {
        self.deps.services.less_computer.abort_capture(session_id)
    }

    /// Run one Less Computer turn using the preferences snapshot owned by
    /// Core. Hosts pass only user text; provider, model, permission, workdir,
    /// continuation and guard policy are resolved here before reaching the
    /// native runtime Adapter.
    pub async fn submit_less_computer(
        &self,
        transcript: String,
    ) -> Result<LessComputerRunResult, BackendError> {
        self.submit_less_computer_with_session(SessionId::new(), transcript)
            .await
    }

    /// Run one Less Computer turn with a host-owned session identifier.
    ///
    /// Audio-capable hosts use this overload so a physical hotkey release or
    /// Esc can cancel the same Core run that owns the transcript.  The host
    /// still supplies no provider policy; all preferences and safety rules are
    /// resolved here exactly as in [`Self::submit_less_computer`].
    pub async fn submit_less_computer_with_session(
        &self,
        session_id: SessionId,
        transcript: String,
    ) -> Result<LessComputerRunResult, BackendError> {
        let preferences = self.get_preferences();
        if !preferences.coding_agent_enabled {
            return Err(BackendError::new(
                BackendErrorCode::PermissionDenied,
                "Less Computer is disabled",
            )
            .retryable(false));
        }
        let provider = CodingAgentProvider::from_pref(&preferences.coding_agent_provider);
        let executable = Some(normalize_coding_agent_executable(
            provider,
            preferences.coding_agent_exe.clone(),
        )?);
        let workdir = normalize_coding_agent_workdir(
            preferences.coding_agent_workdir.clone(),
            self.config.home_dir.clone(),
        )?;
        let request = LessComputerRunRequest {
            session_id,
            transcript,
            provider,
            executable,
            model: resolve_coding_agent_model(provider, preferences.coding_agent_model.clone()),
            permission_mode: normalize_less_computer_permission_mode(
                provider,
                &preferences.coding_agent_permission_mode,
            ),
            workdir,
            continue_session: false,
            continuation_context: None,
            approved_patterns: Vec::new(),
        };
        self.deps.services.less_computer.submit(request).await
    }

    /// Cancel a Less Computer run through its instance-local Core state.
    pub async fn cancel_less_computer(
        &self,
        session_id: Option<SessionId>,
    ) -> Result<(), BackendError> {
        self.deps.services.less_computer.cancel(session_id).await
    }

    /// Capture the same immutable provider/preferences snapshot used by the
    /// main dictation pipeline for a host-owned auxiliary audio flow such as
    /// QA. This is an adapter seam, not a UI use-case.
    #[doc(hidden)]
    pub async fn capture_host_dictation_context(
        &self,
        options: DictationStartOptions,
    ) -> Result<Arc<DictationContext>, BackendError> {
        Ok(Arc::new(self.capture_dictation_context(&options).await?))
    }

    pub fn subscribe(&self) -> EventSubscription {
        self.events.subscribe()
    }

    /// Replay the bounded instance-local event tail after `sequence`.
    ///
    /// Hosts use this after a cold UI mount or a lag notification. A truncated
    /// result means the caller must rebuild its complete view model from the
    /// current snapshots before applying the returned tail.
    pub fn replay_events_after(&self, sequence: u64) -> EventReplay {
        self.events.replay_after(sequence)
    }

    /// Return a typed publisher for platform/transport Adapters that need to
    /// report progress or capability changes on the backend event stream.
    pub fn event_publisher(&self) -> BackendEventPublisher {
        BackendEventPublisher::new(Arc::clone(&self.events))
    }

    pub fn request_host_action(&self, action: HostAction) -> Result<(), BackendError> {
        self.deps.host_actions.request(action)
    }

    fn engine_progress_sink(&self) -> Arc<dyn EngineProgressSink> {
        Arc::new(BackendEngineProgress {
            events: Arc::clone(&self.events),
            state: Arc::clone(&self.state),
            phase_changed: Arc::clone(&self.phase_changed),
        })
    }

    pub async fn start(&self) -> Result<StartupSnapshot, BackendError> {
        let credentials = self
            .deps
            .credential_store
            .status(self.get_preferences())
            .await?;
        let mut state = self.state.write().expect("backend state lock poisoned");
        state.credentials = credentials;
        if state.running {
            return Ok(StartupSnapshot {
                backend: BackendSnapshot {
                    running: true,
                    dictation: state.dictation.clone(),
                    vocabulary_revision: self.vocabulary_revision.load(Ordering::Acquire),
                    history_revision: self.history_revision.load(Ordering::Acquire),
                    style_pack_revision: self.style_pack_revision.load(Ordering::Acquire),
                    preferences_revision: self.preferences_revision.load(Ordering::Acquire),
                    credentials: state.credentials.clone(),
                },
            });
        }
        state.running = true;
        self.events.publish(None, BackendEventKind::BackendStarted);
        Ok(StartupSnapshot {
            backend: BackendSnapshot {
                running: true,
                dictation: state.dictation.clone(),
                vocabulary_revision: self.vocabulary_revision.load(Ordering::Acquire),
                history_revision: self.history_revision.load(Ordering::Acquire),
                style_pack_revision: self.style_pack_revision.load(Ordering::Acquire),
                preferences_revision: self.preferences_revision.load(Ordering::Acquire),
                credentials: state.credentials.clone(),
            },
        })
    }

    pub async fn shutdown(&self) -> Result<(), BackendError> {
        let active_session = {
            let mut state = self.state.write().expect("backend state lock poisoned");
            if !state.running {
                return Ok(());
            }
            let active_session = state.dictation.session_id;
            self.events.publish(None, BackendEventKind::BackendStopping);
            if active_session.is_some() {
                state.dictation.phase = DictationPhase::Cancelled;
                self.events.publish(
                    state.dictation.session_id,
                    BackendEventKind::DictationStateChanged(state.dictation.clone()),
                );
            }
            state.running = false;
            state.dictation = DictationStateSnapshot::default();
            state.dictation_context = None;
            self.phase_changed.notify_waiters();
            active_session
        };
        let selection_result = match self.deps.services.selection.snapshot().await {
            Ok(snapshot)
                if matches!(
                    snapshot.phase,
                    crate::domains::SelectionPhase::Capturing
                        | crate::domains::SelectionPhase::Preview
                        | crate::domains::SelectionPhase::Applying
                ) =>
            {
                self.deps
                    .services
                    .selection
                    .cancel(snapshot.session_id)
                    .await
            }
            Ok(_) => Ok(()),
            Err(error) if error.code == BackendErrorCode::Unsupported => Ok(()),
            Err(error) => Err(error),
        };
        let selection_voice_result = match self.deps.services.selection_voice.snapshot().await {
            Ok(snapshot)
                if matches!(
                    snapshot.phase,
                    crate::domains::SelectionVoicePhase::Recording
                        | crate::domains::SelectionVoicePhase::Processing
                        | crate::domains::SelectionVoicePhase::AwaitingIntent
                        | crate::domains::SelectionVoicePhase::Preview
                        | crate::domains::SelectionVoicePhase::Applying
                ) =>
            {
                self.deps
                    .services
                    .selection_voice
                    .cancel(snapshot.session_id)
                    .await
            }
            Ok(_) => Ok(()),
            Err(error) if error.code == BackendErrorCode::Unsupported => Ok(()),
            Err(error) => Err(error),
        };
        let qa_result = match self.deps.services.qa.snapshot().await {
            Ok(snapshot)
                if matches!(
                    snapshot.phase,
                    crate::domains::QaPhase::Recording
                        | crate::domains::QaPhase::Thinking
                        | crate::domains::QaPhase::AwaitingApproval
                ) =>
            {
                self.deps.services.qa.cancel(snapshot.session_id).await
            }
            Ok(_) => Ok(()),
            Err(error) if error.code == BackendErrorCode::Unsupported => Ok(()),
            Err(error) => Err(error),
        };
        let remote_input_result = match self.deps.services.remote_input.status() {
            Ok(status) if status.enabled || status.running => {
                self.deps
                    .services
                    .remote_input
                    .configure(crate::domains::RemoteInputConfig {
                        enabled: false,
                        port: status.port,
                    })
                    .await
            }
            Ok(_) => Ok(()),
            Err(error) if error.code == BackendErrorCode::Unsupported => Ok(()),
            Err(error) => Err(error),
        };
        if let Some(session_id) = active_session {
            let cancel_result = self.cancel_session_adapters(session_id).await;
            let host_result = self
                .deps
                .host_actions
                .request(HostAction::HideDictationFeedback);
            cancel_result?;
            host_result?;
        }
        selection_result?;
        selection_voice_result?;
        qa_result?;
        self.deps.services.less_computer.cancel(None).await?;
        self.deps.services.less_computer.dismiss();
        self.dismiss_pending_corrections();
        remote_input_result
    }

    pub fn snapshot(&self) -> BackendSnapshot {
        let state = self.state.read().expect("backend state lock poisoned");
        BackendSnapshot {
            running: state.running,
            dictation: state.dictation.clone(),
            vocabulary_revision: self.vocabulary_revision.load(Ordering::Acquire),
            history_revision: self.history_revision.load(Ordering::Acquire),
            style_pack_revision: self.style_pack_revision.load(Ordering::Acquire),
            preferences_revision: self.preferences_revision.load(Ordering::Acquire),
            credentials: state.credentials.clone(),
        }
    }

    /// Dispatch a launcher/single-instance intent through the same state
    /// machine and domain Interfaces used by normal host calls.
    pub async fn dispatch_cli_intent(
        &self,
        intent: crate::cli::CliIntent,
    ) -> Result<CliDispatchOutcome, BackendError> {
        match intent {
            crate::cli::CliIntent::ToggleDictation => match self.snapshot().dictation.phase {
                DictationPhase::Idle => self
                    .start_dictation()
                    .await
                    .map(CliDispatchOutcome::DictationStarted),
                DictationPhase::Starting | DictationPhase::Recording => self
                    .stop_dictation()
                    .await
                    .map(CliDispatchOutcome::DictationCompleted),
                DictationPhase::Transcribing
                | DictationPhase::Polishing
                | DictationPhase::Inserting
                | DictationPhase::Completed
                | DictationPhase::Cancelled
                | DictationPhase::Failed => Ok(CliDispatchOutcome::Noop),
            },
            crate::cli::CliIntent::ToggleQa => {
                self.deps.services.qa.toggle_recording().await?;
                Ok(CliDispatchOutcome::QaToggled)
            }
            crate::cli::CliIntent::CancelDictation => {
                let session_id = self.snapshot().dictation.session_id;
                if session_id.is_none() {
                    return Ok(CliDispatchOutcome::Noop);
                }
                self.cancel_dictation(session_id).await?;
                Ok(CliDispatchOutcome::DictationCancelled)
            }
        }
    }

    /// Apply physical dictation-key edges using the shared hotkey-mode rules.
    ///
    /// Native listeners provide monotonic event timestamps so queued events do
    /// not change Auto mode's short-press/hold decision.
    pub async fn dispatch_dictation_hotkey_edge(
        &self,
        edge: DictationHotkeyEdge,
    ) -> Result<CliDispatchOutcome, BackendError> {
        self.dispatch_dictation_hotkey_edge_with_session_options(
            edge,
            DictationHotkeyDispatchOptions::default(),
        )
        .await
    }

    /// Apply a physical dictation-key edge with host-captured start options.
    ///
    /// This is primarily used by native hotkey adapters that receive a
    /// translation modifier before the dictation press. The options are only
    /// consumed when this edge actually starts a new session.
    pub async fn dispatch_dictation_hotkey_edge_with_options(
        &self,
        edge: DictationHotkeyEdge,
        options: DictationStartOptions,
    ) -> Result<CliDispatchOutcome, BackendError> {
        self.dispatch_dictation_hotkey_edge_with_session_options(
            edge,
            DictationHotkeyDispatchOptions {
                start: options,
                stop: DictationStopOptions::default(),
            },
        )
        .await
    }

    /// Apply a physical hotkey edge with host-captured start and stop options.
    ///
    /// Start options are consumed only when the edge creates a session. Stop
    /// options are consumed only when it finalizes one. This preserves desktop
    /// translation-modifier semantics without making the host duplicate the
    /// Toggle/Hold/Auto state machine.
    pub async fn dispatch_dictation_hotkey_edge_with_session_options(
        &self,
        edge: DictationHotkeyEdge,
        options: DictationHotkeyDispatchOptions,
    ) -> Result<CliDispatchOutcome, BackendError> {
        use crate::shared_types::HotkeyMode;

        const AUTO_HOLD_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(350);
        let mode = self.get_preferences().hotkey.mode;
        let phase = self.snapshot().dictation.phase;
        match edge {
            DictationHotkeyEdge::Combined => {
                *self
                    .hotkey_press_at
                    .lock()
                    .expect("hotkey timestamp lock poisoned") = None;
                if matches!(phase, DictationPhase::Starting | DictationPhase::Recording) {
                    let active = self.snapshot().dictation.session_id;
                    self.cancel_dictation(active).await?;
                    Ok(CliDispatchOutcome::DictationCancelled)
                } else {
                    Ok(CliDispatchOutcome::Noop)
                }
            }
            DictationHotkeyEdge::Pressed { at } => match mode {
                HotkeyMode::Hold if phase == DictationPhase::Idle => self
                    .start_dictation_with_options(options.start)
                    .await
                    .map(CliDispatchOutcome::DictationStarted),
                HotkeyMode::Hold => Ok(CliDispatchOutcome::Noop),
                HotkeyMode::Auto if phase == DictationPhase::Idle => {
                    *self
                        .hotkey_press_at
                        .lock()
                        .expect("hotkey timestamp lock poisoned") = Some(at);
                    self.start_dictation_with_options(options.start)
                        .await
                        .map(CliDispatchOutcome::DictationStarted)
                }
                HotkeyMode::Auto
                    if matches!(phase, DictationPhase::Starting | DictationPhase::Recording) =>
                {
                    *self
                        .hotkey_press_at
                        .lock()
                        .expect("hotkey timestamp lock poisoned") = None;
                    self.stop_dictation_with_options(options.stop)
                        .await
                        .map(CliDispatchOutcome::DictationCompleted)
                }
                HotkeyMode::Auto => Ok(CliDispatchOutcome::Noop),
                HotkeyMode::Toggle | HotkeyMode::DoubleClick => match phase {
                    DictationPhase::Idle => self
                        .start_dictation_with_options(options.start)
                        .await
                        .map(CliDispatchOutcome::DictationStarted),
                    DictationPhase::Starting | DictationPhase::Recording => self
                        .stop_dictation_with_options(options.stop)
                        .await
                        .map(CliDispatchOutcome::DictationCompleted),
                    DictationPhase::Transcribing
                    | DictationPhase::Polishing
                    | DictationPhase::Inserting
                    | DictationPhase::Completed
                    | DictationPhase::Cancelled
                    | DictationPhase::Failed => Ok(CliDispatchOutcome::Noop),
                },
            },
            DictationHotkeyEdge::Released { at } => match mode {
                HotkeyMode::Toggle | HotkeyMode::DoubleClick => Ok(CliDispatchOutcome::Noop),
                HotkeyMode::Hold => {
                    if matches!(phase, DictationPhase::Starting | DictationPhase::Recording) {
                        self.stop_dictation_with_options(options.stop)
                            .await
                            .map(CliDispatchOutcome::DictationCompleted)
                    } else {
                        Ok(CliDispatchOutcome::Noop)
                    }
                }
                HotkeyMode::Auto => {
                    let pressed_at = self
                        .hotkey_press_at
                        .lock()
                        .expect("hotkey timestamp lock poisoned")
                        .take();
                    let held_long = pressed_at.is_some_and(|pressed| {
                        at.saturating_duration_since(pressed) >= AUTO_HOLD_THRESHOLD
                    });
                    if held_long
                        && matches!(phase, DictationPhase::Starting | DictationPhase::Recording)
                    {
                        self.stop_dictation_with_options(options.stop)
                            .await
                            .map(CliDispatchOutcome::DictationCompleted)
                    } else {
                        Ok(CliDispatchOutcome::Noop)
                    }
                }
            },
        }
    }

    /// Update the only session field that may change after start: whether the
    /// final polish operation translates the transcript.
    pub async fn update_dictation_translation_requested(
        &self,
        requested: bool,
    ) -> Result<(), BackendError> {
        let (session_id, context) = {
            let state = self.state.read().expect("backend state lock poisoned");
            ensure_running(&state)?;
            if !matches!(
                state.dictation.phase,
                DictationPhase::Starting | DictationPhase::Recording
            ) {
                return Err(BackendError::new(
                    BackendErrorCode::InvalidState,
                    "dictation translation can only change before finalization",
                ));
            }
            let session_id = state.dictation.session_id.ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::InvalidState,
                    "no active dictation session",
                )
            })?;
            let context = state
                .dictation_context
                .as_ref()
                .ok_or_else(|| {
                    BackendError::new(
                        BackendErrorCode::Internal,
                        "active dictation session has no captured context",
                    )
                })?
                .with_translation_requested(requested);
            (session_id, Arc::new(context))
        };

        self.deps
            .dictation_engine
            .update_context(session_id, Arc::clone(&context))
            .await?;

        let mut state = self.state.write().expect("backend state lock poisoned");
        if state.dictation.session_id != Some(session_id)
            || !matches!(
                state.dictation.phase,
                DictationPhase::Starting | DictationPhase::Recording
            )
        {
            return Err(BackendError::new(
                BackendErrorCode::Cancelled,
                "dictation session changed while translation was updating",
            ));
        }
        state.dictation_context = Some(context);
        if state.dictation.translation_active != requested {
            state.dictation.translation_active = requested;
            self.events.publish(
                Some(session_id),
                BackendEventKind::DictationStateChanged(state.dictation.clone()),
            );
        }
        Ok(())
    }

    pub async fn get_credentials_status(&self) -> Result<CredentialsStatus, BackendError> {
        let status = self
            .deps
            .credential_store
            .status(self.get_preferences())
            .await?;
        self.state
            .write()
            .expect("backend state lock poisoned")
            .credentials = status.clone();
        Ok(status)
    }

    pub async fn read_credential(
        &self,
        key: CredentialKey,
    ) -> Result<Option<SecretValue>, BackendError> {
        self.deps.credential_store.read(key).await
    }

    pub async fn set_credential(
        &self,
        key: CredentialKey,
        value: SecretValue,
    ) -> Result<CredentialsStatus, BackendError> {
        self.deps.credential_store.write(key, value).await?;
        self.refresh_and_publish_credentials().await
    }

    pub async fn remove_credential(
        &self,
        key: CredentialKey,
    ) -> Result<CredentialsStatus, BackendError> {
        self.deps.credential_store.remove(key).await?;
        self.refresh_and_publish_credentials().await
    }

    pub async fn list_channels(
        &self,
        kind: ChannelKind,
    ) -> Result<Vec<ChannelSummary>, BackendError> {
        self.deps.credential_store.list_channels(kind).await
    }

    pub async fn create_channel(
        &self,
        kind: ChannelKind,
        provider_type: String,
        name: String,
    ) -> Result<String, BackendError> {
        match self
            .apply_channel_mutation(ChannelMutation::Create {
                kind,
                provider_type,
                name,
            })
            .await?
        {
            ChannelMutationResult::Created(id) => Ok(id),
            _ => Err(BackendError::new(
                BackendErrorCode::Internal,
                "credential store returned an invalid create-channel result",
            )),
        }
    }

    pub async fn set_channel_provider_type(
        &self,
        kind: ChannelKind,
        id: String,
        provider_type: String,
    ) -> Result<(), BackendError> {
        self.apply_channel_mutation(ChannelMutation::SetProviderType {
            kind,
            id,
            provider_type,
        })
        .await
        .map(|_| ())
    }

    pub async fn delete_channel_if_blank(
        &self,
        kind: ChannelKind,
        id: String,
    ) -> Result<bool, BackendError> {
        match self
            .apply_channel_mutation(ChannelMutation::DeleteIfBlank { kind, id })
            .await?
        {
            ChannelMutationResult::DeletedIfBlank(deleted) => Ok(deleted),
            _ => Err(BackendError::new(
                BackendErrorCode::Internal,
                "credential store returned an invalid draft-cleanup result",
            )),
        }
    }

    pub async fn rename_channel(
        &self,
        kind: ChannelKind,
        id: String,
        name: String,
    ) -> Result<(), BackendError> {
        self.apply_channel_mutation(ChannelMutation::Rename { kind, id, name })
            .await
            .map(|_| ())
    }

    pub async fn delete_channel(&self, kind: ChannelKind, id: String) -> Result<(), BackendError> {
        self.apply_channel_mutation(ChannelMutation::Delete { kind, id })
            .await
            .map(|_| ())
    }

    pub async fn set_channel_enabled(
        &self,
        kind: ChannelKind,
        id: String,
        enabled: bool,
    ) -> Result<(), BackendError> {
        self.apply_channel_mutation(ChannelMutation::SetEnabled { kind, id, enabled })
            .await
            .map(|_| ())
    }

    pub async fn reorder_channels(
        &self,
        kind: ChannelKind,
        ids: Vec<String>,
    ) -> Result<(), BackendError> {
        self.apply_channel_mutation(ChannelMutation::Reorder { kind, ids })
            .await
            .map(|_| ())
    }

    pub async fn record_channel_test(
        &self,
        kind: ChannelKind,
        id: String,
        ok: bool,
        latency_ms: Option<u32>,
        error: Option<String>,
    ) -> Result<(), BackendError> {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        self.apply_channel_mutation(ChannelMutation::RecordTest {
            kind,
            id,
            ok,
            latency_ms,
            at,
            error,
        })
        .await
        .map(|_| ())
    }

    pub async fn active_provider(&self, slot: ProviderSlot) -> Result<String, BackendError> {
        self.deps.credential_store.active_provider(slot).await
    }

    pub async fn set_active_provider(
        &self,
        slot: ProviderSlot,
        provider_id: String,
    ) -> Result<CredentialsStatus, BackendError> {
        if provider_id.trim().is_empty() {
            return Err(BackendError::new(
                BackendErrorCode::InvalidArgument,
                "provider id must not be blank",
            ));
        }
        self.deps
            .credential_store
            .set_active_provider(slot, provider_id)
            .await?;
        self.refresh_and_publish_credentials().await
    }

    async fn apply_channel_mutation(
        &self,
        mutation: ChannelMutation,
    ) -> Result<ChannelMutationResult, BackendError> {
        let result = self.deps.credential_store.mutate_channel(mutation).await?;
        self.refresh_and_publish_credentials().await?;
        Ok(result)
    }

    async fn refresh_and_publish_credentials(&self) -> Result<CredentialsStatus, BackendError> {
        let status = self
            .deps
            .credential_store
            .status(self.get_preferences())
            .await?;
        self.state
            .write()
            .expect("backend state lock poisoned")
            .credentials = status.clone();
        self.events
            .publish(None, BackendEventKind::CredentialsChanged(status.clone()));
        Ok(status)
    }

    pub fn get_preferences(&self) -> UserPreferences {
        self.preferences.get()
    }

    #[cfg(test)]
    pub(crate) fn set_preferences(
        &self,
        mut preferences: UserPreferences,
    ) -> Result<(), BackendError> {
        sync_style_pack_preferences(&mut preferences, &self.style_packs.list()?);
        self.preferences.set(preferences)?;
        self.publish_preferences_changed();
        Ok(())
    }

    /// Persist a host-facing settings document after applying the shared
    /// shortcut compatibility and collision rules.
    #[cfg(test)]
    pub(crate) fn set_preferences_validated(
        &self,
        mut preferences: UserPreferences,
    ) -> Result<(), BackendError> {
        crate::sync_dictation_hotkey_legacy_fields(&mut preferences);
        crate::reject_hotkey_collisions(&preferences).map_err(|message| {
            BackendError::new(crate::BackendErrorCode::InvalidArgument, message)
        })?;
        self.set_preferences(preferences)
    }

    #[cfg(test)]
    pub(crate) fn set_preferences_preserving_style(
        &self,
        preferences: UserPreferences,
    ) -> Result<(), BackendError> {
        self.preferences
            .set_preserving_current_style_preferences(preferences)?;
        self.publish_preferences_changed();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_preferences_preserving_style_validated(
        &self,
        mut preferences: UserPreferences,
    ) -> Result<(), BackendError> {
        crate::sync_dictation_hotkey_legacy_fields(&mut preferences);
        crate::reject_hotkey_collisions(&preferences).map_err(|message| {
            BackendError::new(crate::BackendErrorCode::InvalidArgument, message)
        })?;
        self.set_preferences_preserving_style(preferences)
    }

    pub fn update_settings<R: crate::SettingsRuntime + ?Sized>(
        &self,
        mut preferences: UserPreferences,
        options: crate::SettingsUpdateOptions,
        runtime: &R,
    ) -> Result<crate::SettingsUpdateOutcome, BackendError> {
        let _write_guard = self
            .settings_write_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(expected) = options.expected_preferences_revision {
            let actual = self.preferences_revision.load(Ordering::Acquire);
            if actual != expected {
                return Err(BackendError {
                    code: BackendErrorCode::Busy,
                    message: "settings changed since the submitted document was read".into(),
                    retryable: true,
                    details: Some(serde_json::json!({
                        "expectedPreferencesRevision": expected,
                        "actualPreferencesRevision": actual,
                    })),
                });
            }
        }
        let mut previous = self.preferences.get();
        crate::sync_dictation_hotkey_legacy_fields(&mut previous);
        crate::sync_dictation_hotkey_legacy_fields(&mut preferences);
        if options.preserve_current_style {
            preferences.preserve_style_preferences_from(&previous);
        }
        sync_style_pack_preferences(&mut preferences, &self.style_packs.list()?);

        let reconciled_hotkey_count = match crate::reject_hotkey_collisions(&preferences) {
            Ok(()) => 0,
            Err(message)
                if options.collision_policy == crate::SettingsCollisionPolicy::Reconcile =>
            {
                let adjusted = crate::reconcile_hotkey_collisions(&mut preferences, &previous);
                crate::reject_hotkey_collisions(&preferences).map_err(|leftover| {
                    BackendError::new(
                        BackendErrorCode::InvalidArgument,
                        format!(
                            "{message}; reconciled {adjusted} shortcuts but validation still failed: {leftover}"
                        ),
                    )
                })?;
                adjusted
            }
            Err(message) => {
                return Err(BackendError::new(
                    BackendErrorCode::InvalidArgument,
                    message,
                ));
            }
        };

        let effects = crate::SettingsEffectPlan::between(&previous, &preferences);
        let mut receipt = match runtime.prepare(&effects) {
            Ok(receipt) => receipt,
            Err(failure) => {
                let compensation = runtime.restore(&effects, &failure.receipt).err();
                return Err(settings_transaction_error(
                    failure.error,
                    compensation.into_iter().collect(),
                ));
            }
        };

        if let Err(failure) = runtime.commit(&effects, &mut receipt) {
            for effect in failure.receipt.applied {
                if !receipt.applied.contains(&effect) {
                    receipt.applied.push(effect);
                }
            }
            let compensation_errors = runtime
                .restore(&effects, &receipt)
                .err()
                .into_iter()
                .collect();
            return Err(settings_transaction_error(
                failure.error,
                compensation_errors,
            ));
        }

        if let Err(error) = self.preferences.set(preferences.clone()) {
            let compensation = runtime.restore(&effects, &receipt).err();
            return Err(settings_transaction_error(
                error,
                compensation.into_iter().collect(),
            ));
        }

        self.publish_preferences_changed();
        Ok(crate::SettingsUpdateOutcome {
            preferences,
            reconciled_hotkey_count,
            effects,
        })
    }

    fn publish_preferences_changed(&self) {
        let revision = self.preferences_revision.fetch_add(1, Ordering::AcqRel) + 1;
        self.events.publish(
            None,
            BackendEventKind::PreferencesChanged(PreferencesChange { revision }),
        );
    }

    pub fn list_style_packs(&self, active_id: &str) -> Result<Vec<StylePack>, BackendError> {
        self.style_packs.list_with_active(active_id)
    }

    /// Return settings-page prompt diagnostics assembled by Core. The DTO is
    /// owned and safe for any host to render; hosts must not duplicate prompt
    /// composition or hotword filtering.
    pub fn preview_style_pack_runtime(
        &self,
        style_pack: &StylePack,
    ) -> crate::style_packs::StylePackRuntimeDiagnostics {
        let preferences = self.get_preferences();
        let hotwords = self
            .list_vocabulary()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.enabled)
            .map(|entry| entry.phrase)
            .collect();
        crate::style_packs::build_style_pack_runtime_diagnostics(style_pack, &preferences, hotwords)
    }

    /// Persist the microphone selected by a host-owned menu or device picker.
    ///
    /// This focused use-case keeps callers away from whole-document writes and
    /// shares the settings write gate with validated settings transactions.
    pub fn select_microphone_device(&self, device_name: String) -> Result<(), BackendError> {
        let _write_guard = self
            .settings_write_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut preferences = self.preferences.get();
        preferences.microphone_device_name = device_name;
        self.preferences.set(preferences)?;
        self.publish_preferences_changed();
        Ok(())
    }

    /// Select the previous enabled style pack in the stable store order.
    ///
    /// Returns `None` when cycling is not meaningful (zero or one enabled pack).
    /// Window feedback and tray refresh remain host responsibilities.
    pub fn activate_previous_style_pack(&self) -> Result<Option<StylePack>, BackendError> {
        let _write_guard = self
            .settings_write_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut preferences = self.preferences.get();
        let packs = self
            .style_packs
            .list_with_active(&preferences.active_style_pack_id)?;
        let enabled = packs
            .into_iter()
            .filter(|pack| pack.enabled)
            .collect::<Vec<_>>();
        if enabled.len() <= 1 {
            return Ok(None);
        }
        let current_index = enabled
            .iter()
            .position(|pack| pack.id == preferences.active_style_pack_id)
            .unwrap_or(0);
        let next_index = if current_index == 0 {
            enabled.len() - 1
        } else {
            current_index - 1
        };
        let mut selected = enabled[next_index].clone();
        preferences.active_style_pack_id = selected.id.clone();
        sync_style_pack_preferences(&mut preferences, &enabled);
        self.preferences.set(preferences)?;
        self.publish_preferences_changed();
        selected.active = true;
        Ok(Some(selected))
    }

    pub fn get_style_pack(&self, id: &str) -> Result<StylePack, BackendError> {
        self.style_packs.get(id)
    }

    pub fn get_active_style_pack(&self, active_id: &str) -> Result<StylePack, BackendError> {
        self.style_packs.get_or_default_active(active_id)
    }

    pub fn activate_style_pack(&self, id: &str) -> Result<StylePack, BackendError> {
        let mut pack = self.style_packs.get(id)?;
        if !pack.enabled {
            pack = self.style_packs.set_enabled(id, true)?;
        }
        let mut preferences = self.preferences.get();
        preferences.active_style_pack_id = id.to_string();
        sync_style_pack_preferences(&mut preferences, &self.style_packs.list()?);
        self.preferences.set(preferences)?;
        self.publish_preferences_changed();
        self.publish_style_packs_changed();
        pack.active = true;
        Ok(pack)
    }

    pub fn create_style_pack(&self, pack: StylePack) -> Result<StylePack, BackendError> {
        let pack = self.style_packs.create(pack)?;
        self.sync_preferences_after_style_pack_change()?;
        self.publish_style_packs_changed();
        Ok(pack)
    }

    pub fn update_style_pack(&self, pack: StylePack) -> Result<StylePack, BackendError> {
        let pack = self.style_packs.update(pack)?;
        self.sync_preferences_after_style_pack_change()?;
        self.publish_style_packs_changed();
        Ok(pack)
    }

    pub fn set_style_pack_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<StylePack, BackendError> {
        let pack = self.style_packs.set_enabled(id, enabled)?;
        self.sync_preferences_after_style_pack_change()?;
        self.publish_style_packs_changed();
        Ok(pack)
    }

    pub fn set_style_pack_origin(
        &self,
        id: &str,
        origin_pack_id: Option<String>,
        origin_author_login: Option<String>,
    ) -> Result<StylePack, BackendError> {
        let pack = self
            .style_packs
            .set_origin(id, origin_pack_id, origin_author_login)?;
        self.publish_style_packs_changed();
        Ok(pack)
    }

    pub fn reset_builtin_style_pack(&self, id: &str) -> Result<StylePack, BackendError> {
        let pack = self.style_packs.reset_builtin(id)?;
        self.sync_preferences_after_style_pack_change()?;
        self.publish_style_packs_changed();
        Ok(pack)
    }

    pub fn remove_style_pack(
        &self,
        id: &str,
    ) -> Result<crate::StylePackRemovalOutcome, BackendError> {
        let _write_guard = self
            .settings_write_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = self.preferences.get();
        self.style_packs.remove_imported(id)?;
        let mut preferences = previous.clone();
        preferences
            .style_pack_hotkeys
            .retain(|entry| entry.pack_id != id);
        if preferences.active_style_pack_id == id {
            preferences.active_style_pack_id = crate::style_packs::default_active_style_pack_id();
        }
        sync_style_pack_preferences(&mut preferences, &self.style_packs.list()?);
        let effects = crate::SettingsEffectPlan::between(&previous, &preferences);
        self.preferences.set(preferences)?;
        self.publish_preferences_changed();
        self.sync_preferences_after_style_pack_change()?;
        self.publish_style_packs_changed();
        Ok(crate::StylePackRemovalOutcome { effects })
    }

    pub fn import_style_pack_bytes(&self, bytes: &[u8]) -> Result<StylePack, BackendError> {
        let pack = self.style_packs.import_from_zip_bytes(bytes)?;
        self.sync_preferences_after_style_pack_change()?;
        self.publish_style_packs_changed();
        Ok(pack)
    }

    pub fn import_style_pack_path(
        &self,
        path: &std::path::Path,
    ) -> Result<StylePack, BackendError> {
        let pack = self.style_packs.import_from_zip(path)?;
        self.sync_preferences_after_style_pack_change()?;
        self.publish_style_packs_changed();
        Ok(pack)
    }

    pub fn export_style_pack_bytes(&self, id: &str) -> Result<Vec<u8>, BackendError> {
        self.style_packs.export_zip_bytes(id)
    }

    pub fn export_style_pack_path(
        &self,
        id: &str,
        path: &std::path::Path,
    ) -> Result<(), BackendError> {
        self.style_packs.export_to_zip(id, path)
    }

    fn sync_preferences_after_style_pack_change(&self) -> Result<(), BackendError> {
        let mut preferences = self.preferences.get();
        if sync_style_pack_preferences(&mut preferences, &self.style_packs.list()?) {
            self.preferences.set(preferences)?;
            self.publish_preferences_changed();
        }
        Ok(())
    }

    fn publish_style_packs_changed(&self) {
        let revision = self.style_pack_revision.fetch_add(1, Ordering::AcqRel) + 1;
        self.events.publish(
            None,
            BackendEventKind::StylePacksChanged(StylePackChange { revision }),
        );
    }

    pub fn list_history(&self) -> Result<Vec<DictationSession>, BackendError> {
        self.history.list()
    }

    pub fn recent_history_within_minutes(
        &self,
        minutes: u32,
    ) -> Result<Vec<DictationSession>, BackendError> {
        self.history.recent_within_minutes(minutes)
    }

    pub fn list_activity(&self) -> Result<Vec<ActivityDay>, BackendError> {
        self.activity.snapshot()
    }

    pub fn record_activity(
        &self,
        date: &str,
        chars: u64,
        duration_ms: u64,
    ) -> Result<(), BackendError> {
        self.activity.bump(date, chars, duration_ms)?;
        self.publish_history_changed();
        Ok(())
    }

    pub fn append_history(
        &self,
        session: DictationSession,
        retention_days: u32,
        max_entries: Option<u32>,
    ) -> Result<(), BackendError> {
        self.history
            .append_with_retention(session, retention_days, max_entries)?;
        self.publish_history_changed();
        Ok(())
    }

    pub fn delete_history(&self, id: &str) -> Result<(), BackendError> {
        self.history.delete(id)?;
        self.publish_history_changed();
        Ok(())
    }

    pub fn update_history_entry(&self, session: DictationSession) -> Result<bool, BackendError> {
        let updated = self.history.update_entry(session)?;
        if updated {
            self.publish_history_changed();
        }
        Ok(updated)
    }

    pub fn clear_history(&self) -> Result<(), BackendError> {
        self.history.clear()?;
        self.publish_history_changed();
        Ok(())
    }

    fn publish_history_changed(&self) {
        let revision = self.history_revision.fetch_add(1, Ordering::AcqRel) + 1;
        self.events.publish(
            None,
            BackendEventKind::HistoryChanged(HistoryChange { revision }),
        );
    }

    pub fn list_vocabulary(&self) -> Result<Vec<DictionaryEntry>, BackendError> {
        self.vocabulary.list()
    }

    /// Return the instance-local correction suggestions awaiting a user
    /// decision. The returned value is owned and safe to render on any host.
    pub fn pending_corrections(&self) -> Vec<PendingCorrection> {
        self.pending_corrections
            .lock()
            .expect("pending correction lock poisoned")
            .clone()
    }

    /// Queue one observed manual correction. Duplicate pairs are ignored and
    /// the oldest item is dropped when the bounded card capacity is reached.
    pub fn queue_pending_correction(
        &self,
        pattern: String,
        replacement: String,
    ) -> Result<Option<PendingCorrection>, BackendError> {
        if pattern.trim().is_empty() || replacement.trim().is_empty() {
            return Err(BackendError::new(
                BackendErrorCode::InvalidArgument,
                "correction pattern and replacement are required",
            ));
        }
        let (suggestion, snapshot) = {
            let mut pending = self
                .pending_corrections
                .lock()
                .expect("pending correction lock poisoned");
            if pending
                .iter()
                .any(|item| item.pattern == pattern && item.replacement == replacement)
            {
                return Ok(None);
            }
            if pending.len() >= MAX_PENDING_CORRECTIONS {
                pending.remove(0);
            }
            let suggestion = PendingCorrection {
                id: uuid::Uuid::new_v4().to_string(),
                pattern,
                replacement,
            };
            pending.push(suggestion.clone());
            (suggestion, pending.clone())
        };
        self.events.publish(
            None,
            BackendEventKind::VocabularySuggestionsChanged(snapshot),
        );
        Ok(Some(suggestion))
    }

    /// Accept one suggestion and atomically remove it only after the shared
    /// vocabulary mutation succeeds. Repeated or stale ids are idempotent.
    pub fn accept_pending_correction(
        &self,
        id: &str,
    ) -> Result<Option<PendingCorrection>, BackendError> {
        let (suggestion, added, snapshot) = {
            let mut pending = self
                .pending_corrections
                .lock()
                .expect("pending correction lock poisoned");
            let Some(index) = pending.iter().position(|item| item.id == id) else {
                return Ok(None);
            };
            let suggestion = pending[index].clone();
            let added = self.vocabulary.add_if_absent(
                suggestion.replacement.clone(),
                Some(LEARNED_VOCAB_NOTE.to_string()),
            )?;
            pending.remove(index);
            (suggestion, added.is_some(), pending.clone())
        };
        if added {
            self.publish_vocabulary_changed();
        }
        self.events.publish(
            None,
            BackendEventKind::VocabularySuggestionsChanged(snapshot),
        );
        Ok(Some(suggestion))
    }

    /// Reject one suggestion without creating a hidden deny-list.
    pub fn reject_pending_correction(&self, id: &str) -> bool {
        let snapshot = {
            let mut pending = self
                .pending_corrections
                .lock()
                .expect("pending correction lock poisoned");
            let Some(index) = pending.iter().position(|item| item.id == id) else {
                return false;
            };
            pending.remove(index);
            pending.clone()
        };
        self.events.publish(
            None,
            BackendEventKind::VocabularySuggestionsChanged(snapshot),
        );
        true
    }

    /// Dismiss the complete card. Empty dismissals are idempotent and do not
    /// publish redundant events.
    pub fn dismiss_pending_corrections(&self) {
        let changed = {
            let mut pending = self
                .pending_corrections
                .lock()
                .expect("pending correction lock poisoned");
            if pending.is_empty() {
                false
            } else {
                pending.clear();
                true
            }
        };
        if changed {
            self.events.publish(
                None,
                BackendEventKind::VocabularySuggestionsChanged(Vec::new()),
            );
        }
    }

    pub fn add_vocabulary(
        &self,
        phrase: String,
        note: Option<String>,
    ) -> Result<DictionaryEntry, BackendError> {
        let entry = self.vocabulary.add(phrase, note)?;
        self.publish_vocabulary_changed();
        Ok(entry)
    }

    pub fn add_vocabulary_if_absent(
        &self,
        phrase: String,
        note: Option<String>,
    ) -> Result<Option<DictionaryEntry>, BackendError> {
        let entry = self.vocabulary.add_if_absent(phrase, note)?;
        if entry.is_some() {
            self.publish_vocabulary_changed();
        }
        Ok(entry)
    }

    pub fn record_vocabulary_hits(&self, text: &str) -> Result<u64, BackendError> {
        let hits = self.vocabulary.record_hits(text)?;
        if hits > 0 {
            self.publish_vocabulary_changed();
        }
        Ok(hits)
    }

    pub fn remove_vocabulary(&self, id: &str) -> Result<(), BackendError> {
        self.vocabulary.remove(id)?;
        self.publish_vocabulary_changed();
        Ok(())
    }

    pub fn set_vocabulary_enabled(&self, id: &str, enabled: bool) -> Result<(), BackendError> {
        self.vocabulary.set_enabled(id, enabled)?;
        self.publish_vocabulary_changed();
        Ok(())
    }

    pub fn list_correction_rules(&self) -> Result<Vec<CorrectionRule>, BackendError> {
        self.correction_rules.list()
    }

    pub fn add_correction_rule(
        &self,
        pattern: String,
        replacement: String,
    ) -> Result<CorrectionRule, BackendError> {
        let rule = self.correction_rules.add(pattern, replacement)?;
        self.publish_vocabulary_changed();
        Ok(rule)
    }

    pub fn remove_correction_rule(&self, id: &str) -> Result<(), BackendError> {
        self.correction_rules.remove(id)?;
        self.publish_vocabulary_changed();
        Ok(())
    }

    pub fn set_correction_rule_enabled(&self, id: &str, enabled: bool) -> Result<(), BackendError> {
        self.correction_rules.set_enabled(id, enabled)?;
        self.publish_vocabulary_changed();
        Ok(())
    }

    pub fn list_vocabulary_presets(&self) -> Result<VocabPresetStore, BackendError> {
        crate::vocabulary::list_vocab_presets(&self.config.data_dir)
    }

    pub fn save_vocabulary_presets(&self, store: &VocabPresetStore) -> Result<(), BackendError> {
        crate::vocabulary::save_vocab_presets(&self.config.data_dir, store)?;
        self.publish_vocabulary_changed();
        Ok(())
    }

    fn publish_vocabulary_changed(&self) {
        let revision = self.vocabulary_revision.fetch_add(1, Ordering::AcqRel) + 1;
        self.events.publish(
            None,
            BackendEventKind::VocabularyChanged(VocabularyChange { revision }),
        );
    }

    pub async fn start_dictation(&self) -> Result<SessionId, BackendError> {
        self.start_dictation_with_options(DictationStartOptions::default())
            .await
    }

    pub async fn start_external_dictation(&self) -> Result<SessionId, BackendError> {
        self.start_external_dictation_with_options(DictationStartOptions::default())
            .await
    }

    pub async fn start_external_dictation_with_options(
        &self,
        mut options: DictationStartOptions,
    ) -> Result<SessionId, BackendError> {
        options.audio_source = DictationAudioSource::External;
        self.start_dictation_with_options(options).await
    }

    pub fn feed_external_pcm(&self, session_id: SessionId, pcm: &[u8]) -> Result<(), BackendError> {
        {
            let state = self.state.read().expect("backend state lock poisoned");
            ensure_running(&state)?;
            if state.dictation.session_id != Some(session_id) {
                return Err(BackendError::new(
                    BackendErrorCode::InvalidState,
                    "external PCM targets an inactive dictation session",
                ));
            }
            if !matches!(
                state.dictation.phase,
                DictationPhase::Starting | DictationPhase::Recording
            ) {
                return Err(BackendError::new(
                    BackendErrorCode::InvalidState,
                    "external PCM is only accepted while recording",
                ));
            }
            if state
                .dictation_context
                .as_ref()
                .is_none_or(|context| context.audio_source != DictationAudioSource::External)
            {
                return Err(BackendError::new(
                    BackendErrorCode::InvalidState,
                    "active dictation session does not use external audio",
                ));
            }
        }
        self.deps.dictation_engine.feed_audio(session_id, pcm)
    }

    pub async fn start_dictation_with_options(
        &self,
        options: DictationStartOptions,
    ) -> Result<SessionId, BackendError> {
        let context = Arc::new(self.capture_dictation_context(&options).await?);
        let session_id = {
            let mut state = self.state.write().expect("backend state lock poisoned");
            ensure_running(&state)?;
            if state.dictation.session_id.is_some() {
                return Err(BackendError::new(
                    BackendErrorCode::Busy,
                    "a dictation session is already active",
                ));
            }
            let session_id = SessionId::new();
            state.dictation = DictationStateSnapshot {
                phase: DictationPhase::Starting,
                session_id: Some(session_id),
                elapsed_ms: 0,
                level: 0.0,
                message: None,
                translation_active: context.polish.translation_active,
            };
            state.dictation_context = Some(Arc::clone(&context));
            self.events.publish(
                Some(session_id),
                BackendEventKind::DictationStateChanged(state.dictation.clone()),
            );
            self.phase_changed.notify_waiters();
            session_id
        };

        if let Err(error) = self
            .deps
            .host_actions
            .request(HostAction::ShowDictationFeedback)
        {
            self.mark_dictation_failed(session_id, &error);
            self.reset_dictation_session(session_id);
            return Err(error);
        }
        if let Err(error) = self
            .deps
            .text_inserter
            .prepare(session_id, Arc::clone(&context))
            .await
        {
            self.mark_dictation_failed(session_id, &error);
            self.persist_failed_dictation(
                &context,
                session_id,
                "insertFailed",
                String::new(),
                String::new(),
                None,
                None,
                None,
                None,
                None,
                false,
            );
            let _ = self.deps.text_inserter.cancel(session_id).await;
            let _ = self
                .deps
                .host_actions
                .request(HostAction::HideDictationFeedback);
            self.reset_dictation_session(session_id);
            return Err(error);
        }
        if let Err(error) = self
            .deps
            .dictation_engine
            .start(
                session_id,
                Arc::clone(&context),
                self.engine_progress_sink(),
            )
            .await
        {
            if error.code != BackendErrorCode::Cancelled {
                self.mark_dictation_failed(session_id, &error);
                self.persist_failed_dictation(
                    &context,
                    session_id,
                    "transcribeFailed",
                    String::new(),
                    String::new(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                );
            }
            let _ = self.cancel_session_adapters(session_id).await;
            let _ = self
                .deps
                .host_actions
                .request(HostAction::HideDictationFeedback);
            self.reset_dictation_session(session_id);
            return Err(error);
        }
        let started = {
            let mut state = self.state.write().expect("backend state lock poisoned");
            if state.dictation.session_id == Some(session_id)
                && state.dictation.phase == DictationPhase::Starting
            {
                state.dictation.phase = DictationPhase::Recording;
                self.events.publish(
                    Some(session_id),
                    BackendEventKind::DictationStateChanged(state.dictation.clone()),
                );
                self.phase_changed.notify_waiters();
                true
            } else {
                false
            }
        };
        if !started {
            let _ = self.cancel_session_adapters(session_id).await;
            return Err(BackendError::new(
                BackendErrorCode::Cancelled,
                "dictation session was cancelled while the engine was starting",
            ));
        }
        Ok(session_id)
    }

    pub async fn stop_dictation(&self) -> Result<DictationResult, BackendError> {
        self.stop_dictation_session_with_options(None, DictationStopOptions::default())
            .await
    }

    pub async fn stop_dictation_with_options(
        &self,
        options: DictationStopOptions,
    ) -> Result<DictationResult, BackendError> {
        self.stop_dictation_session_with_options(None, options)
            .await
    }

    pub async fn stop_dictation_session(
        &self,
        session_id: SessionId,
    ) -> Result<DictationResult, BackendError> {
        self.stop_dictation_session_with_options(Some(session_id), DictationStopOptions::default())
            .await
    }

    async fn stop_dictation_session_with_options(
        &self,
        expected_session_id: Option<SessionId>,
        options: DictationStopOptions,
    ) -> Result<DictationResult, BackendError> {
        let (session_id, context, context_changed) = loop {
            // Register before inspecting the phase so a Starting -> Recording
            // transition cannot be lost between the state read and await.
            let changed = self.phase_changed.notified();
            let ready = {
                let mut state = self.state.write().expect("backend state lock poisoned");
                ensure_running(&state)?;
                let session_id = state.dictation.session_id.ok_or_else(|| {
                    BackendError::new(
                        BackendErrorCode::InvalidState,
                        "no active dictation session",
                    )
                })?;
                if expected_session_id.is_some_and(|expected| expected != session_id) {
                    return Err(BackendError::new(
                        BackendErrorCode::InvalidState,
                        "dictation stop targets a different session",
                    ));
                }
                match state.dictation.phase {
                    DictationPhase::Starting => None,
                    DictationPhase::Recording => {
                        let captured = state.dictation_context.clone().ok_or_else(|| {
                            BackendError::new(
                                BackendErrorCode::Internal,
                                "active dictation session has no captured context",
                            )
                        })?;
                        let context = match options.translation_requested {
                            Some(requested) => {
                                Arc::new(captured.with_translation_requested(requested))
                            }
                            None => captured,
                        };
                        let context_changed =
                            state.dictation_context.as_ref().is_some_and(|previous| {
                                previous.polish.translation_active
                                    != context.polish.translation_active
                            });
                        state.dictation_context = Some(Arc::clone(&context));
                        state.dictation.translation_active = context.polish.translation_active;
                        state.dictation.phase = DictationPhase::Transcribing;
                        self.events.publish(
                            Some(session_id),
                            BackendEventKind::DictationStateChanged(state.dictation.clone()),
                        );
                        self.phase_changed.notify_waiters();
                        Some((session_id, context, context_changed))
                    }
                    _ => {
                        return Err(BackendError::new(
                            BackendErrorCode::Busy,
                            "dictation session is already being finalized",
                        ));
                    }
                }
            };
            if let Some(session) = ready {
                break session;
            }
            changed.await;
        };

        if context_changed {
            if let Err(error) = self
                .deps
                .dictation_engine
                .update_context(session_id, Arc::clone(&context))
                .await
            {
                self.mark_dictation_failed(session_id, &error);
                self.persist_failed_dictation(
                    &context,
                    session_id,
                    "polishFailed",
                    String::new(),
                    String::new(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                );
                let _ = self.cancel_session_adapters(session_id).await;
                let _ = self
                    .deps
                    .host_actions
                    .request(HostAction::HideDictationFeedback);
                self.reset_dictation_session(session_id);
                return Err(error);
            }
        }

        let progress = self.engine_progress_sink();
        let mut engine_result = match self
            .deps
            .dictation_engine
            .finish(session_id, progress)
            .await
        {
            Ok(result) => result,
            Err(failure) => {
                let error = failure.error;
                if error.code != BackendErrorCode::Cancelled {
                    self.mark_dictation_failed(session_id, &error);
                    let raw_text = failure.raw_text.unwrap_or_default();
                    let (error_code, final_text, llm_used) = match failure.stage {
                        EngineFailureStage::Transcribing => {
                            ("transcribeFailed", String::new(), false)
                        }
                        EngineFailureStage::Polishing => ("polishFailed", raw_text.clone(), true),
                    };
                    self.persist_failed_dictation(
                        &context,
                        session_id,
                        error_code,
                        raw_text,
                        final_text,
                        None,
                        failure.duration_ms,
                        failure.asr_ms,
                        failure.polish_ms,
                        failure.has_audio_recording,
                        llm_used,
                    );
                }
                let _ = self.deps.text_inserter.cancel(session_id).await;
                let _ = self
                    .deps
                    .host_actions
                    .request(HostAction::HideDictationFeedback);
                self.reset_dictation_session(session_id);
                return Err(error);
            }
        };

        if engine_result.raw_text.trim().is_empty() {
            let error = BackendError::new(
                BackendErrorCode::Provider,
                "transcription provider returned an empty transcript",
            );
            self.mark_dictation_failed(session_id, &error);
            self.persist_failed_dictation(
                &context,
                session_id,
                "emptyTranscript",
                engine_result.raw_text.clone(),
                String::new(),
                None,
                Some(engine_result.duration_ms),
                engine_result.asr_ms,
                None,
                engine_result.has_audio_recording,
                false,
            );
            let _ = self.deps.text_inserter.cancel(session_id).await;
            let _ = self
                .deps
                .host_actions
                .request(HostAction::HideDictationFeedback);
            self.reset_dictation_session(session_id);
            return Err(error);
        }

        let correction_rules = match self.correction_rules.list() {
            Ok(rules) => rules,
            Err(error) => {
                log::warn!(
                    "failed to load correction rules for completed dictation: {error}; continuing without correction"
                );
                Vec::new()
            }
        };
        if !correction_rules.is_empty() {
            engine_result.polished_text =
                apply_correction_rules(&engine_result.polished_text, &correction_rules);
        }

        // Cancellation may happen while ASR/LLM work is in flight. Never
        // insert a result after the session has been cancelled or replaced.
        {
            let state = self.state.read().expect("backend state lock poisoned");
            if state.dictation.session_id != Some(session_id)
                || !matches!(
                    state.dictation.phase,
                    DictationPhase::Transcribing | DictationPhase::Polishing
                )
            {
                return Err(BackendError::new(
                    BackendErrorCode::Cancelled,
                    "dictation session was cancelled before insertion",
                ));
            }
        }

        {
            let mut state = self.state.write().expect("backend state lock poisoned");
            ensure_active_session(&state, session_id)?;
            state.dictation.phase = DictationPhase::Inserting;
            self.events.publish(
                Some(session_id),
                BackendEventKind::DictationStateChanged(state.dictation.clone()),
            );
            self.phase_changed.notify_waiters();
        }

        let insert_outcome = match self
            .deps
            .text_inserter
            .insert(
                session_id,
                Arc::clone(&context),
                engine_result.polished_text.clone(),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                self.mark_dictation_failed(session_id, &error);
                self.persist_failed_dictation(
                    &context,
                    session_id,
                    "insertFailed",
                    engine_result.raw_text.clone(),
                    engine_result.polished_text.clone(),
                    engine_result.polish_source.clone(),
                    Some(engine_result.duration_ms),
                    engine_result.asr_ms,
                    engine_result.polish_ms,
                    engine_result.has_audio_recording,
                    context.uses_llm_polisher(),
                );
                let _ = self.deps.text_inserter.cancel(session_id).await;
                let _ = self
                    .deps
                    .host_actions
                    .request(HostAction::HideDictationFeedback);
                self.reset_dictation_session(session_id);
                return Err(error);
            }
        };
        let result = DictationResult {
            session_id,
            raw_text: engine_result.raw_text.clone(),
            polished_text: engine_result.polished_text.clone(),
            polish_source: engine_result.polish_source.clone(),
            duration_ms: engine_result.duration_ms,
            inserted: insert_outcome.into_status(),
        };

        let mut state = self.state.write().expect("backend state lock poisoned");
        if state.dictation.session_id != Some(session_id)
            || state.dictation.phase != DictationPhase::Inserting
        {
            return Err(BackendError::new(
                BackendErrorCode::Cancelled,
                "dictation session was replaced before completion",
            ));
        }
        state.dictation.phase = DictationPhase::Completed;
        state.dictation.message = Some(match insert_outcome {
            InsertOutcome::Inserted => "inserted".to_string(),
            InsertOutcome::CopiedFallback => "copied_fallback".to_string(),
            InsertOutcome::Unknown => "outcome_unknown".to_string(),
        });
        if !matches!(insert_outcome, InsertOutcome::Inserted) {
            self.events.publish(
                Some(session_id),
                BackendEventKind::InsertFallback(crate::types::InsertFallbackPayload {
                    reason: match insert_outcome {
                        InsertOutcome::CopiedFallback => "clipboard_fallback".to_string(),
                        InsertOutcome::Unknown => "outcome_unknown".to_string(),
                        InsertOutcome::Inserted => unreachable!("guarded above"),
                    },
                    copied_text: Some(engine_result.polished_text.clone()),
                }),
            );
        }
        self.events.publish(
            Some(session_id),
            BackendEventKind::DictationCompleted(result.clone()),
        );
        self.events.publish(
            Some(session_id),
            BackendEventKind::DictationStateChanged(state.dictation.clone()),
        );
        self.phase_changed.notify_waiters();
        drop(state);
        let host_result = self
            .deps
            .host_actions
            .request(HostAction::HideDictationFeedback);
        let mut state = self.state.write().expect("backend state lock poisoned");
        state.dictation = DictationStateSnapshot::default();
        state.dictation_context = None;
        self.phase_changed.notify_waiters();
        drop(state);
        self.persist_completed_dictation(&context, &result, insert_outcome, &engine_result);
        host_result?;
        Ok(result)
    }

    fn persist_completed_dictation(
        &self,
        context: &DictationContext,
        result: &DictationResult,
        insert_outcome: InsertOutcome,
        engine_result: &crate::ports::EngineResult,
    ) {
        let preferences = self.get_preferences();
        let dictionary_entry_count = match self.record_vocabulary_hits(&result.polished_text) {
            Ok(hits) => Some(hits.min(u32::MAX as u64) as u32),
            Err(error) => {
                log::warn!("failed to record vocabulary hits for completed dictation: {error}");
                None
            }
        };
        let front_app =
            crate::shared_types::split_front_app_opt(context.polish.front_app.as_deref());
        let insert_status = match insert_outcome {
            InsertOutcome::Inserted => HistoryInsertStatus::Inserted,
            InsertOutcome::CopiedFallback => HistoryInsertStatus::CopiedFallback,
            InsertOutcome::Unknown => HistoryInsertStatus::PasteSent,
        };
        let pipeline_mode = match context.pipeline_mode {
            crate::shared_types::PipelineMode::Traditional => "traditional",
            crate::shared_types::PipelineMode::Multimodal => "multimodal",
        };
        let llm_used = context.uses_llm_polisher();
        let attribution = HistoryProviderAttribution::from_context(
            context,
            llm_used,
            engine_result.asr_ms,
            engine_result.polish_ms,
        );
        let session = DictationSession {
            id: result.session_id.to_string(),
            created_at: self.clock.now_utc().to_rfc3339(),
            source: HistorySource::Voice,
            raw_transcript: result.raw_text.clone(),
            asr_transcript: Some(result.raw_text.clone()),
            final_text: result.polished_text.clone(),
            mode: context.polish.mode,
            style_pack_id: Some(context.polish.style_pack_id.clone()),
            translation_active: context.polish.translation_active,
            polish_source: result.polish_source.clone(),
            app_bundle_id: front_app.bundle_id,
            app_name: front_app.name,
            insert_status,
            error_code: engine_result
                .polish_failed
                .then(|| "polishFailed".to_string()),
            duration_ms: Some(result.duration_ms),
            dictionary_entry_count,
            has_audio_recording: engine_result.has_audio_recording,
            asr_provider: attribution.asr_provider,
            asr_model: attribution.asr_model,
            llm_provider: attribution.llm_provider,
            llm_model: attribution.llm_model,
            pipeline_mode: Some(pipeline_mode.to_string()),
            asr_ms: attribution.asr_ms,
            polish_ms: attribution.polish_ms,
        };
        if let Err(error) = self.append_history(
            session,
            preferences.history_retention_days,
            preferences.history_max_entries,
        ) {
            log::warn!("failed to persist completed dictation history: {error}");
        }
        if let Err(error) = self.record_activity(
            &self.clock.today_local().format("%Y-%m-%d").to_string(),
            result.polished_text.chars().count() as u64,
            result.duration_ms,
        ) {
            log::warn!("failed to persist completed dictation activity: {error}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_failed_dictation(
        &self,
        context: &DictationContext,
        session_id: SessionId,
        error_code: &str,
        raw_text: String,
        final_text: String,
        polish_source: Option<String>,
        duration_ms: Option<u64>,
        asr_ms: Option<u64>,
        polish_ms: Option<u64>,
        has_audio_recording: Option<bool>,
        llm_used: bool,
    ) {
        let preferences = self.get_preferences();
        let front_app =
            crate::shared_types::split_front_app_opt(context.polish.front_app.as_deref());
        let pipeline_mode = match context.pipeline_mode {
            crate::shared_types::PipelineMode::Traditional => "traditional",
            crate::shared_types::PipelineMode::Multimodal => "multimodal",
        };
        let attribution =
            HistoryProviderAttribution::from_context(context, llm_used, asr_ms, polish_ms);
        let session = DictationSession {
            id: session_id.to_string(),
            created_at: self.clock.now_utc().to_rfc3339(),
            source: HistorySource::Voice,
            raw_transcript: raw_text.clone(),
            asr_transcript: Some(raw_text),
            final_text,
            mode: context.polish.mode,
            style_pack_id: Some(context.polish.style_pack_id.clone()),
            translation_active: context.polish.translation_active,
            polish_source,
            app_bundle_id: front_app.bundle_id,
            app_name: front_app.name,
            insert_status: HistoryInsertStatus::Failed,
            error_code: Some(error_code.to_string()),
            duration_ms,
            dictionary_entry_count: None,
            has_audio_recording,
            asr_provider: attribution.asr_provider,
            asr_model: attribution.asr_model,
            llm_provider: attribution.llm_provider,
            llm_model: attribution.llm_model,
            pipeline_mode: Some(pipeline_mode.to_string()),
            asr_ms: attribution.asr_ms,
            polish_ms: attribution.polish_ms,
        };
        if let Err(error) = self.append_history(
            session,
            preferences.history_retention_days,
            preferences.history_max_entries,
        ) {
            log::warn!("failed to persist dictation failure history: {error}");
        }
    }

    fn reset_dictation_session(&self, session_id: SessionId) {
        let mut state = self.state.write().expect("backend state lock poisoned");
        if state.dictation.session_id == Some(session_id) {
            state.dictation = DictationStateSnapshot::default();
            state.dictation_context = None;
            self.phase_changed.notify_waiters();
        }
    }

    async fn cancel_session_adapters(&self, session_id: SessionId) -> Result<(), BackendError> {
        let engine_result = self.deps.dictation_engine.cancel(session_id).await;
        let inserter_result = self.deps.text_inserter.cancel(session_id).await;
        engine_result?;
        inserter_result
    }

    pub async fn cancel_dictation(
        &self,
        session_id: Option<SessionId>,
    ) -> Result<(), BackendError> {
        let active = {
            let mut state = self.state.write().expect("backend state lock poisoned");
            ensure_running(&state)?;
            let active = state.dictation.session_id.ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::InvalidState,
                    "no active dictation session",
                )
            })?;
            if session_id.is_some() && session_id != Some(active) {
                return Err(BackendError::new(
                    BackendErrorCode::InvalidArgument,
                    "session id does not match the active session",
                ));
            }
            state.dictation.phase = DictationPhase::Cancelled;
            self.events.publish(
                Some(active),
                BackendEventKind::DictationStateChanged(state.dictation.clone()),
            );
            state.dictation = DictationStateSnapshot::default();
            state.dictation_context = None;
            self.phase_changed.notify_waiters();
            active
        };
        let cancel_result = self.cancel_session_adapters(active).await;
        let host_result = self
            .deps
            .host_actions
            .request(HostAction::HideDictationFeedback);
        cancel_result?;
        host_result?;
        Ok(())
    }

    async fn capture_dictation_context(
        &self,
        options: &DictationStartOptions,
    ) -> Result<DictationContext, BackendError> {
        let preferences = self.get_preferences();
        let style_pack_id = options
            .style_pack_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&preferences.active_style_pack_id);
        let style_pack = self.style_packs.get_or_default_active(style_pack_id)?;
        let hotwords = self
            .vocabulary
            .list()?
            .into_iter()
            .filter(|entry| entry.enabled)
            .map(|entry| entry.phrase)
            .collect();
        let active_asr_provider = self
            .resolve_session_provider(ProviderSlot::Asr, &preferences.active_asr_provider)
            .await?;
        let active_llm_provider = self
            .resolve_session_provider(ProviderSlot::Llm, &preferences.active_llm_provider)
            .await?;
        let active_omni_provider = self
            .resolve_session_provider(ProviderSlot::Omni, &preferences.active_omni_provider)
            .await?;
        let recent_history = if preferences.polish_context_window_minutes == 0 {
            Vec::new()
        } else {
            match self
                .history
                .recent_within_minutes(preferences.polish_context_window_minutes)
            {
                Ok(sessions) => sessions,
                Err(error) => {
                    log::warn!(
                        "failed to capture polish history context; using a single turn: {error}"
                    );
                    Vec::new()
                }
            }
        };
        Ok(DictationContext::capture(
            &preferences,
            &style_pack,
            DictationProviderInvocations::new(
                active_asr_provider,
                active_llm_provider,
                active_omni_provider,
            ),
            hotwords,
            recent_history,
            options,
        ))
    }

    async fn resolve_session_provider(
        &self,
        slot: ProviderSlot,
        preference_fallback: &str,
    ) -> Result<crate::dictation_context::ProviderInvocation, BackendError> {
        crate::provider_resolution::resolve_session_provider(
            &self.deps.credential_store,
            slot,
            preference_fallback,
        )
        .await
    }

    fn mark_dictation_failed(&self, session_id: SessionId, error: &BackendError) {
        let mut state = self.state.write().expect("backend state lock poisoned");
        if state.dictation.session_id != Some(session_id) {
            return;
        }
        state.dictation.phase = DictationPhase::Failed;
        state.dictation.message = Some(format!("{:?}", error.code));
        let snapshot = state.dictation.clone();
        self.events.publish(
            Some(session_id),
            BackendEventKind::DictationStateChanged(snapshot),
        );
        self.phase_changed.notify_waiters();
    }
}

fn ensure_running(state: &MutableState) -> Result<(), BackendError> {
    if state.running {
        Ok(())
    } else {
        Err(BackendError::new(
            BackendErrorCode::InvalidState,
            "backend is not started",
        ))
    }
}

fn ensure_active_session(state: &MutableState, session_id: SessionId) -> Result<(), BackendError> {
    ensure_running(state)?;
    if state.dictation.session_id == Some(session_id)
        && !matches!(
            state.dictation.phase,
            DictationPhase::Idle
                | DictationPhase::Completed
                | DictationPhase::Cancelled
                | DictationPhase::Failed
        )
    {
        Ok(())
    } else {
        Err(BackendError::new(
            BackendErrorCode::Cancelled,
            "dictation progress belongs to an inactive session",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::future::BoxFuture;

    use super::*;
    use crate::config::{BackendConfig, TokioTaskSpawner};
    use crate::errors::BackendError;
    use crate::events::CodingAgentStreamEvent;
    use crate::ports::{
        boxed, DictationEngine, EngineFailure, EngineProgressSink, EngineResult, HostAction,
        HostActions, InsertOutcome, TextInserter,
    };

    fn assert_send_sync<T: Send + Sync>() {}

    struct TestDataDir {
        path: std::path::PathBuf,
    }

    impl TestDataDir {
        fn new(label: &str) -> Self {
            Self {
                path: std::env::temp_dir().join(format!(
                    "openless-core-{label}-{}",
                    uuid::Uuid::new_v4().simple()
                )),
            }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TestDataDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    struct TestBackend {
        backend: OpenLessBackend,
        _data_dir: TestDataDir,
    }

    impl std::ops::Deref for TestBackend {
        type Target = OpenLessBackend;

        fn deref(&self) -> &Self::Target {
            &self.backend
        }
    }

    #[test]
    fn backend_is_safe_to_share_between_host_and_ui_tasks() {
        assert_send_sync::<OpenLessBackend>();
    }

    #[derive(Default)]
    struct FakeHost(Mutex<Vec<HostAction>>);

    impl HostActions for FakeHost {
        fn request(&self, action: HostAction) -> Result<(), BackendError> {
            self.0.lock().unwrap().push(action);
            Ok(())
        }
    }

    struct FakeEngine;

    impl DictationEngine for FakeEngine {
        fn start(
            &self,
            _session_id: SessionId,
            _context: Arc<DictationContext>,
            _progress: Arc<dyn EngineProgressSink>,
        ) -> BoxFuture<'static, Result<(), BackendError>> {
            boxed(async { Ok(()) })
        }

        fn finish(
            &self,
            _session_id: SessionId,
            _progress: Arc<dyn EngineProgressSink>,
        ) -> BoxFuture<'static, Result<EngineResult, EngineFailure>> {
            boxed(async {
                Ok(EngineResult {
                    raw_text: "raw".to_string(),
                    polished_text: "polished".to_string(),
                    polish_source: None,
                    duration_ms: 1000,
                    polish_failed: false,
                    asr_ms: None,
                    polish_ms: None,
                    has_audio_recording: None,
                })
            })
        }

        fn cancel(&self, _session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>> {
            boxed(async { Ok(()) })
        }
    }

    struct FakeInserter;

    impl TextInserter for FakeInserter {
        fn insert(
            &self,
            _session_id: SessionId,
            _context: Arc<DictationContext>,
            _text: String,
        ) -> BoxFuture<'static, Result<InsertOutcome, BackendError>> {
            boxed(async { Ok(InsertOutcome::Inserted) })
        }
    }

    struct FailingEngine;

    impl DictationEngine for FailingEngine {
        fn start(
            &self,
            _session_id: SessionId,
            _context: Arc<DictationContext>,
            _progress: Arc<dyn EngineProgressSink>,
        ) -> BoxFuture<'static, Result<(), BackendError>> {
            boxed(async { Ok(()) })
        }

        fn finish(
            &self,
            _session_id: SessionId,
            _progress: Arc<dyn EngineProgressSink>,
        ) -> BoxFuture<'static, Result<EngineResult, EngineFailure>> {
            boxed(async {
                Err(EngineFailure::from(BackendError::new(
                    BackendErrorCode::Provider,
                    "fixture provider failure",
                )))
            })
        }

        fn cancel(&self, _session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>> {
            boxed(async { Ok(()) })
        }
    }

    struct PolishMetadataFailingEngine;

    impl DictationEngine for PolishMetadataFailingEngine {
        fn start(
            &self,
            _session_id: SessionId,
            _context: Arc<DictationContext>,
            _progress: Arc<dyn EngineProgressSink>,
        ) -> BoxFuture<'static, Result<(), BackendError>> {
            boxed(async { Ok(()) })
        }

        fn finish(
            &self,
            _session_id: SessionId,
            _progress: Arc<dyn EngineProgressSink>,
        ) -> BoxFuture<'static, Result<EngineResult, EngineFailure>> {
            boxed(async {
                Err(EngineFailure {
                    error: BackendError::new(BackendErrorCode::Provider, "fixture omni failure"),
                    stage: EngineFailureStage::Polishing,
                    raw_text: Some("omni raw".to_string()),
                    duration_ms: Some(900),
                    asr_ms: Some(300),
                    polish_ms: Some(600),
                    has_audio_recording: Some(true),
                })
            })
        }

        fn cancel(&self, _session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>> {
            boxed(async { Ok(()) })
        }
    }

    struct StartFailingEngine;

    impl DictationEngine for StartFailingEngine {
        fn start(
            &self,
            _session_id: SessionId,
            _context: Arc<DictationContext>,
            _progress: Arc<dyn EngineProgressSink>,
        ) -> BoxFuture<'static, Result<(), BackendError>> {
            boxed(async {
                Err(BackendError::new(
                    BackendErrorCode::Platform,
                    "fixture recorder start failure",
                ))
            })
        }

        fn finish(
            &self,
            _session_id: SessionId,
            _progress: Arc<dyn EngineProgressSink>,
        ) -> BoxFuture<'static, Result<EngineResult, EngineFailure>> {
            boxed(async {
                Err(EngineFailure::from(BackendError::new(
                    BackendErrorCode::InvalidState,
                    "fixture start never completed",
                )))
            })
        }

        fn cancel(&self, _session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>> {
            boxed(async { Ok(()) })
        }
    }

    struct BlockingStartEngine {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl DictationEngine for BlockingStartEngine {
        fn start(
            &self,
            _session_id: SessionId,
            _context: Arc<DictationContext>,
            _progress: Arc<dyn EngineProgressSink>,
        ) -> BoxFuture<'static, Result<(), BackendError>> {
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            boxed(async move {
                entered.notify_one();
                release.notified().await;
                Ok(())
            })
        }

        fn finish(
            &self,
            _session_id: SessionId,
            _progress: Arc<dyn EngineProgressSink>,
        ) -> BoxFuture<'static, Result<EngineResult, EngineFailure>> {
            boxed(async {
                Ok(EngineResult {
                    raw_text: "raw".to_string(),
                    polished_text: "polished".to_string(),
                    polish_source: None,
                    duration_ms: 1000,
                    polish_failed: false,
                    asr_ms: None,
                    polish_ms: None,
                    has_audio_recording: None,
                })
            })
        }

        fn cancel(&self, _session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>> {
            boxed(async { Ok(()) })
        }
    }

    fn backend() -> (TestBackend, Arc<FakeHost>) {
        let host = Arc::new(FakeHost::default());
        let data_dir = TestDataDir::new("facade");
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.path().to_path_buf(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: host.clone(),
                text_inserter: Arc::new(FakeInserter),
                dictation_engine: Arc::new(FakeEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        (
            TestBackend {
                backend,
                _data_dir: data_dir,
            },
            host,
        )
    }

    #[derive(Default)]
    struct LessComputerCaptureRuntime {
        request: Mutex<Option<crate::coding_agent::CodingAgentRequest>>,
    }

    impl crate::domains::LessComputerRuntimeAdapter for LessComputerCaptureRuntime {
        fn run(
            &self,
            request: crate::coding_agent::CodingAgentRequest,
            events: tokio::sync::mpsc::UnboundedSender<CodingAgentStreamEvent>,
            _cancel: Arc<std::sync::atomic::AtomicBool>,
        ) -> BoxFuture<'static, Result<(), BackendError>> {
            *self.request.lock().unwrap() = Some(request.clone());
            boxed(async move {
                let _ = events.send(CodingAgentStreamEvent::Completed {
                    session_id: request.session_id,
                    text: "完成".into(),
                    cost_usd: None,
                    duration_ms: None,
                });
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn less_computer_facade_resolves_provider_model_permission_and_workdir() {
        let data_dir = TestDataDir::new("less-computer-facade");
        let runtime = Arc::new(LessComputerCaptureRuntime::default());
        let dependencies = BackendDependencies::unsupported();
        dependencies
            .services
            .less_computer
            .bind_runtime(runtime.clone());
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.path().to_path_buf(),
                home_dir: Some(std::env::temp_dir()),
                ..BackendConfig::default()
            },
            dependencies,
        )
        .unwrap();
        let mut preferences = backend.get_preferences();
        preferences.coding_agent_enabled = true;
        preferences.coding_agent_provider = "dsh-cli".into();
        preferences.coding_agent_permission_mode = "bypassPermissions".into();
        let workdir = std::env::temp_dir().join("openless-less-computer-workdir");
        preferences.coding_agent_workdir = Some(format!("  {}  ", workdir.display()));
        backend.set_preferences(preferences).unwrap();

        let session_id = SessionId::new();
        let result = backend
            .submit_less_computer_with_session(session_id, "  做一次检查  ".into())
            .await
            .unwrap();
        assert!(matches!(
            result.outcome,
            crate::domains::LessComputerRunOutcome::Completed { .. }
        ));

        let request = runtime.request.lock().unwrap().clone().unwrap();
        assert_eq!(
            request.provider,
            crate::coding_agent::CodingAgentProvider::DshCli
        );
        assert_eq!(request.executable.as_deref(), Some("dsh"));
        assert_eq!(request.model, None);
        assert_eq!(
            request.permission_mode,
            crate::coding_agent::CodingAgentPermissionMode::Plan
        );
        assert_eq!(request.cwd, Some(workdir));
        assert!(request.prompt.contains("做一次检查"));
    }

    #[tokio::test]
    async fn disabled_less_computer_is_rejected_before_runtime_access() {
        let data_dir = TestDataDir::new("less-computer-disabled");
        let runtime = Arc::new(LessComputerCaptureRuntime::default());
        let dependencies = BackendDependencies::unsupported();
        dependencies
            .services
            .less_computer
            .bind_runtime(runtime.clone());
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.path().to_path_buf(),
                ..BackendConfig::default()
            },
            dependencies,
        )
        .unwrap();

        let error = backend
            .submit_less_computer("不应启动".into())
            .await
            .unwrap_err();
        assert_eq!(error.code, BackendErrorCode::PermissionDenied);
        assert!(runtime.request.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn less_computer_capture_facade_is_session_scoped_and_releasable() {
        let (backend, _) = backend();
        let mut preferences = backend.get_preferences();
        preferences.coding_agent_enabled = true;
        backend.set_preferences(preferences).unwrap();

        let session_id = SessionId::new();
        let other_session = SessionId::new();
        backend.begin_less_computer_capture(session_id).unwrap();
        assert_eq!(backend.less_computer_active_session(), Some(session_id));

        let busy = backend
            .begin_less_computer_capture(other_session)
            .unwrap_err();
        assert_eq!(busy.code, BackendErrorCode::Busy);

        backend
            .cancel_less_computer(Some(session_id))
            .await
            .unwrap();
        assert!(backend.less_computer_capture_cancelled(session_id));
        assert_eq!(backend.less_computer_active_session(), Some(session_id));

        backend.abort_less_computer_capture(session_id).unwrap();
        assert_eq!(backend.less_computer_active_session(), None);
    }

    fn backend_with_dictation_engine(
        data_dir: std::path::PathBuf,
        dictation_engine: Arc<dyn DictationEngine>,
    ) -> OpenLessBackend {
        OpenLessBackend::new(
            BackendConfig {
                data_dir,
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                    InsertOutcome::Inserted,
                )),
                dictation_engine,
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap()
    }

    fn history_session(id: &str) -> DictationSession {
        DictationSession {
            id: id.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            source: crate::types::HistorySource::Voice,
            raw_transcript: "raw".to_string(),
            asr_transcript: None,
            final_text: "final".to_string(),
            mode: crate::types::PolishMode::Light,
            style_pack_id: None,
            translation_active: false,
            polish_source: None,
            app_bundle_id: None,
            app_name: None,
            insert_status: crate::types::HistoryInsertStatus::Inserted,
            error_code: None,
            duration_ms: Some(1000),
            dictionary_entry_count: None,
            has_audio_recording: None,
            asr_provider: None,
            asr_model: None,
            llm_provider: None,
            llm_model: None,
            pipeline_mode: None,
            asr_ms: None,
            polish_ms: None,
        }
    }

    #[test]
    fn vocabulary_facade_persists_shared_types_and_publishes_revisions() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-backend-vocab-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(FakeInserter),
                dictation_engine: Arc::new(FakeEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        let mut events = backend.subscribe();

        let entry = backend.add_vocabulary("OpenLess".into(), None).unwrap();
        let rule = backend
            .add_correction_rule("几粒".into(), "几例".into())
            .unwrap();
        backend.set_vocabulary_enabled(&entry.id, false).unwrap();
        backend.remove_correction_rule(&rule.id).unwrap();

        assert!(!backend.list_vocabulary().unwrap()[0].enabled);
        assert!(backend.list_correction_rules().unwrap().is_empty());
        assert_eq!(backend.snapshot().vocabulary_revision, 4);
        for expected_revision in 1..=4 {
            let event = events.try_recv().unwrap();
            assert_eq!(
                event.kind,
                BackendEventKind::VocabularyChanged(VocabularyChange {
                    revision: expected_revision,
                })
            );
        }
        assert_eq!(events.try_recv(), Err(crate::events::EventRecvError::Empty));

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn correction_suggestions_are_bounded_idempotent_and_committed_by_core() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-correction-suggestions-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies::unsupported(),
        )
        .unwrap();
        let mut events = backend.subscribe();

        let first = backend
            .queue_pending_correction("扣的爱思".into(), "Codex".into())
            .unwrap()
            .unwrap();
        assert!(backend
            .queue_pending_correction("扣的爱思".into(), "Codex".into())
            .unwrap()
            .is_none());
        for index in 0..MAX_PENDING_CORRECTIONS {
            backend
                .queue_pending_correction(format!("old-{index}"), format!("new-{index}"))
                .unwrap();
        }
        let pending = backend.pending_corrections();
        assert_eq!(pending.len(), MAX_PENDING_CORRECTIONS);
        assert!(pending.iter().all(|item| item.id != first.id));

        let accepted = pending[0].clone();
        assert_eq!(
            backend
                .accept_pending_correction(&accepted.id)
                .unwrap()
                .unwrap(),
            accepted
        );
        assert!(backend
            .accept_pending_correction(&accepted.id)
            .unwrap()
            .is_none());
        let learned = backend
            .list_vocabulary()
            .unwrap()
            .into_iter()
            .find(|entry| entry.phrase == accepted.replacement)
            .unwrap();
        assert_eq!(learned.note.as_deref(), Some("从手改中自动收集"));

        let rejected = backend.pending_corrections()[0].id.clone();
        assert!(backend.reject_pending_correction(&rejected));
        assert!(!backend.reject_pending_correction(&rejected));
        backend.dismiss_pending_corrections();
        backend.dismiss_pending_corrections();
        assert!(backend.pending_corrections().is_empty());

        let mut suggestion_events = 0;
        let mut vocabulary_events = 0;
        while let Ok(event) = events.try_recv() {
            match event.kind {
                BackendEventKind::VocabularySuggestionsChanged(_) => suggestion_events += 1,
                BackendEventKind::VocabularyChanged(_) => vocabulary_events += 1,
                _ => {}
            }
        }
        assert_eq!(suggestion_events, MAX_PENDING_CORRECTIONS + 4);
        assert_eq!(vocabulary_events, 1);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn history_facade_persists_shared_types_and_publishes_revisions() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-backend-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(FakeInserter),
                dictation_engine: Arc::new(FakeEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        let mut events = backend.subscribe();

        let mut entry = history_session("one");
        backend.append_history(entry.clone(), 30, Some(20)).unwrap();
        entry.final_text = "updated".to_string();
        assert!(backend.update_history_entry(entry.clone()).unwrap());
        assert!(!backend
            .update_history_entry(history_session("missing"))
            .unwrap());
        backend.delete_history(&entry.id).unwrap();
        backend.clear_history().unwrap();
        backend.record_activity("2026-08-27", 42, 1000).unwrap();

        assert!(backend.list_history().unwrap().is_empty());
        assert_eq!(backend.list_activity().unwrap()[0].chars, 42);
        assert_eq!(backend.snapshot().history_revision, 5);
        for expected_revision in 1..=5 {
            assert_eq!(
                events.try_recv().unwrap().kind,
                BackendEventKind::HistoryChanged(HistoryChange {
                    revision: expected_revision,
                })
            );
        }
        assert_eq!(events.try_recv(), Err(crate::events::EventRecvError::Empty));

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn focused_microphone_selection_persists_and_publishes_once() {
        let (backend, _) = backend();
        let mut events = backend.subscribe();

        backend
            .select_microphone_device("Studio microphone".to_string())
            .unwrap();

        assert_eq!(
            backend.get_preferences().microphone_device_name,
            "Studio microphone"
        );
        assert_eq!(backend.snapshot().preferences_revision, 1);
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::PreferencesChanged(PreferencesChange { revision: 1 })
        ));
        assert_eq!(events.try_recv(), Err(crate::events::EventRecvError::Empty));
    }

    #[test]
    fn previous_style_use_case_owns_cycle_order_and_preferences_event() {
        let (backend, _) = backend();
        let before = backend.get_preferences();
        let mut events = backend.subscribe();

        let selected = backend
            .activate_previous_style_pack()
            .unwrap()
            .expect("default store has multiple enabled packs");

        assert_ne!(selected.id, before.active_style_pack_id);
        assert!(selected.active);
        assert_eq!(backend.get_preferences().active_style_pack_id, selected.id);
        assert_eq!(backend.snapshot().preferences_revision, 1);
        assert_eq!(backend.snapshot().style_pack_revision, 0);
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::PreferencesChanged(PreferencesChange { revision: 1 })
        ));
        assert_eq!(events.try_recv(), Err(crate::events::EventRecvError::Empty));
    }

    #[test]
    fn style_pack_facade_owns_mutations_and_publishes_revisions() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-backend-style-packs-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(FakeInserter),
                dictation_engine: Arc::new(FakeEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        let mut events = backend.subscribe();
        let pack = backend
            .create_style_pack(StylePack {
                name: "Linux contract".to_string(),
                prompt: "prompt".to_string(),
                ..StylePack::default()
            })
            .unwrap();
        backend.set_style_pack_enabled(&pack.id, false).unwrap();
        backend.remove_style_pack(&pack.id).unwrap();

        assert_eq!(backend.snapshot().style_pack_revision, 3);
        let mut style_revisions = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let BackendEventKind::StylePacksChanged(change) = event.kind {
                style_revisions.push(change.revision);
            }
        }
        assert_eq!(style_revisions, vec![1, 2, 3]);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn activating_style_pack_publishes_preferences_and_style_revisions() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-activate-style-pack-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies::unsupported(),
        )
        .unwrap();
        let pack = backend
            .create_style_pack(StylePack {
                name: "Activate me".to_string(),
                prompt: "prompt".to_string(),
                ..StylePack::default()
            })
            .unwrap();
        let mut events = backend.subscribe();
        let before = backend.snapshot();

        let active = backend.activate_style_pack(&pack.id).unwrap();

        assert!(active.active);
        assert_eq!(backend.get_preferences().active_style_pack_id, pack.id);
        let after = backend.snapshot();
        assert_eq!(after.preferences_revision, before.preferences_revision + 1);
        assert_eq!(after.style_pack_revision, before.style_pack_revision + 1);
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::PreferencesChanged(_)
        ));
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::StylePacksChanged(_)
        ));
        assert_eq!(events.try_recv(), Err(crate::events::EventRecvError::Empty));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn removing_style_pack_cleans_active_id_and_orphan_hotkey() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-remove-style-pack-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies::unsupported(),
        )
        .unwrap();
        let pack = backend
            .create_style_pack(StylePack {
                name: "Temporary pack".to_string(),
                prompt: "prompt".to_string(),
                ..StylePack::default()
            })
            .unwrap();
        let mut preferences = backend.get_preferences();
        preferences.active_style_pack_id = pack.id.clone();
        preferences
            .style_pack_hotkeys
            .push(crate::shared_types::StylePackHotkey {
                pack_id: pack.id.clone(),
                binding: crate::shared_types::ShortcutBinding {
                    primary: "K".to_string(),
                    modifiers: vec!["ctrl".to_string()],
                },
            });
        backend.set_preferences(preferences).unwrap();

        let outcome = backend.remove_style_pack(&pack.id).unwrap();

        let preferences = backend.get_preferences();
        assert_ne!(preferences.active_style_pack_id, pack.id);
        assert!(preferences
            .style_pack_hotkeys
            .iter()
            .all(|entry| entry.pack_id != pack.id));
        let hotkey_change = outcome
            .effects
            .hotkeys
            .expect("removal must expose the host hotkey effect");
        assert!(hotkey_change
            .previous
            .style_packs
            .iter()
            .any(|entry| entry.pack_id == pack.id));
        assert!(hotkey_change
            .next
            .style_packs
            .iter()
            .all(|entry| entry.pack_id != pack.id));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn preferences_facade_persists_shared_contract_and_publishes_revisions() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-backend-preferences-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(FakeInserter),
                dictation_engine: Arc::new(FakeEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        let mut events = backend.subscribe();

        let mut preferences = backend.get_preferences();
        preferences.microphone_device_name = "Shared microphone".to_string();
        backend.set_preferences(preferences).unwrap();

        assert_eq!(
            backend.get_preferences().microphone_device_name,
            "Shared microphone"
        );
        assert_eq!(backend.snapshot().preferences_revision, 1);
        assert_eq!(
            events.try_recv().unwrap().kind,
            BackendEventKind::PreferencesChanged(PreferencesChange { revision: 1 })
        );
        assert!(data_dir.join("preferences.json").is_file());

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn validated_preferences_write_syncs_legacy_fields_and_publishes_once() {
        let (backend, _) = backend();
        let mut events = backend.subscribe();
        let mut preferences = backend.get_preferences();
        preferences.dictation_hotkey = crate::shared_types::ShortcutBinding {
            primary: "RightControl".to_string(),
            modifiers: Vec::new(),
        };

        backend.set_preferences_validated(preferences).unwrap();

        let saved = backend.get_preferences();
        assert_eq!(
            saved.hotkey.trigger,
            crate::shared_types::HotkeyTrigger::RightControl
        );
        assert_eq!(saved.custom_combo_hotkey, None);
        assert_eq!(backend.snapshot().preferences_revision, 1);
        assert_eq!(
            events.try_recv().unwrap().kind,
            BackendEventKind::PreferencesChanged(PreferencesChange { revision: 1 })
        );
        assert_eq!(events.try_recv(), Err(crate::events::EventRecvError::Empty));
    }

    #[test]
    fn validated_preferences_write_rejects_conflicts_without_mutation_or_event() {
        let (backend, _) = backend();
        let before = backend.get_preferences();
        let before_json = serde_json::to_value(&before).unwrap();
        let mut events = backend.subscribe();
        let mut conflicting = before.clone();
        conflicting.translation_hotkey = conflicting.dictation_hotkey.clone();

        let error = backend
            .set_preferences_validated(conflicting)
            .expect_err("conflicting shortcut must be rejected");

        assert_eq!(error.code, BackendErrorCode::InvalidArgument);
        assert_eq!(
            serde_json::to_value(backend.get_preferences()).unwrap(),
            before_json
        );
        assert_eq!(backend.snapshot().preferences_revision, 0);
        assert_eq!(events.try_recv(), Err(crate::events::EventRecvError::Empty));
    }

    #[test]
    fn validated_preferences_write_can_preserve_current_style_fields() {
        let (backend, _) = backend();
        let before = backend.get_preferences();
        let mut events = backend.subscribe();
        let mut incoming = before.clone();
        incoming.microphone_device_name = "Updated microphone".to_string();
        incoming.default_mode = crate::types::PolishMode::Raw;
        incoming.enabled_modes = vec![crate::types::PolishMode::Raw];
        incoming.active_style_pack_id = "incoming.style".to_string();
        incoming.custom_style_prompts.raw = "incoming prompt".to_string();

        backend
            .set_preferences_preserving_style_validated(incoming)
            .unwrap();

        let saved = backend.get_preferences();
        assert_eq!(saved.microphone_device_name, "Updated microphone");
        assert_eq!(saved.default_mode, before.default_mode);
        assert_eq!(saved.enabled_modes, before.enabled_modes);
        assert_eq!(saved.active_style_pack_id, before.active_style_pack_id);
        assert_eq!(saved.style_system_prompts, before.style_system_prompts);
        assert_eq!(saved.custom_style_prompts, before.custom_style_prompts);
        assert_eq!(backend.snapshot().preferences_revision, 1);
        assert_eq!(
            events.try_recv().unwrap().kind,
            BackendEventKind::PreferencesChanged(PreferencesChange { revision: 1 })
        );
        assert_eq!(events.try_recv(), Err(crate::events::EventRecvError::Empty));
    }

    #[derive(Default)]
    struct FailingSettingsRuntime {
        restore_error: Option<BackendError>,
    }

    impl crate::SettingsRuntime for FailingSettingsRuntime {
        fn prepare(
            &self,
            _plan: &crate::SettingsEffectPlan,
        ) -> Result<crate::SettingsEffectReceipt, crate::SettingsEffectFailure> {
            Err(crate::SettingsEffectFailure::after_side_effect(
                BackendError::new(BackendErrorCode::Platform, "runtime apply failed"),
                crate::SettingsEffectReceipt {
                    applied: vec![crate::SettingsEffectKind::ActiveAsrProvider],
                },
            ))
        }

        fn restore(
            &self,
            _plan: &crate::SettingsEffectPlan,
            _receipt: &crate::SettingsEffectReceipt,
        ) -> Result<(), BackendError> {
            self.restore_error.clone().map_or(Ok(()), Err)
        }
    }

    #[derive(Default)]
    struct RecordingSettingsRuntime {
        actions: Mutex<Vec<&'static str>>,
        fail_commit: bool,
    }

    impl crate::SettingsRuntime for RecordingSettingsRuntime {
        fn prepare(
            &self,
            plan: &crate::SettingsEffectPlan,
        ) -> Result<crate::SettingsEffectReceipt, crate::SettingsEffectFailure> {
            self.actions.lock().unwrap().push("prepare");
            Ok(crate::SettingsEffectReceipt {
                applied: plan
                    .active_asr_provider
                    .as_ref()
                    .map(|_| vec![crate::SettingsEffectKind::ActiveAsrProvider])
                    .unwrap_or_default(),
            })
        }

        fn commit(
            &self,
            _plan: &crate::SettingsEffectPlan,
            receipt: &mut crate::SettingsEffectReceipt,
        ) -> Result<(), crate::SettingsEffectFailure> {
            self.actions.lock().unwrap().push("commit");
            receipt.applied.push(crate::SettingsEffectKind::Hotkeys);
            if self.fail_commit {
                Err(crate::SettingsEffectFailure::after_side_effect(
                    BackendError::new(BackendErrorCode::Platform, "listener registration failed"),
                    receipt.clone(),
                ))
            } else {
                Ok(())
            }
        }

        fn restore(
            &self,
            _plan: &crate::SettingsEffectPlan,
            _receipt: &crate::SettingsEffectReceipt,
        ) -> Result<(), BackendError> {
            self.actions.lock().unwrap().push("restore");
            Ok(())
        }
    }

    #[test]
    fn settings_transaction_success_persists_and_publishes_once() {
        let (backend, _) = backend();
        let mut events = backend.subscribe();
        let mut next = backend.get_preferences();
        next.dictation_hotkey = crate::shared_types::ShortcutBinding {
            primary: "RightControl".into(),
            modifiers: vec![],
        };

        let outcome = backend
            .update_settings(
                next,
                crate::SettingsUpdateOptions::STRICT,
                &crate::NoopSettingsRuntime,
            )
            .unwrap();

        assert_eq!(
            outcome.preferences.hotkey.trigger,
            crate::shared_types::HotkeyTrigger::RightControl
        );
        assert_eq!(backend.snapshot().preferences_revision, 1);
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::PreferencesChanged(PreferencesChange { revision: 1 })
        ));
        assert!(matches!(
            events.try_recv(),
            Err(crate::EventRecvError::Empty)
        ));
    }

    #[test]
    fn settings_runtime_failure_preserves_preferences_revision_and_events() {
        let (backend, _) = backend();
        let previous = backend.get_preferences();
        let mut events = backend.subscribe();
        let mut next = previous.clone();
        next.active_asr_provider = "fixture-asr".into();

        let error = backend
            .update_settings(
                next,
                crate::SettingsUpdateOptions::STRICT,
                &FailingSettingsRuntime::default(),
            )
            .unwrap_err();

        assert_eq!(error.code, BackendErrorCode::Platform);
        assert_eq!(
            serde_json::to_value(backend.get_preferences()).unwrap(),
            serde_json::to_value(previous).unwrap()
        );
        assert_eq!(backend.snapshot().preferences_revision, 0);
        assert!(matches!(
            events.try_recv(),
            Err(crate::EventRecvError::Empty)
        ));
    }

    #[test]
    fn settings_transaction_reports_primary_and_compensation_errors() {
        let (backend, _) = backend();
        let mut next = backend.get_preferences();
        next.active_asr_provider = "fixture-asr".into();
        let runtime = FailingSettingsRuntime {
            restore_error: Some(BackendError::new(
                BackendErrorCode::Platform,
                "runtime restore failed",
            )),
        };

        let error = backend
            .update_settings(next, crate::SettingsUpdateOptions::STRICT, &runtime)
            .unwrap_err();

        assert_eq!(error.message, "runtime apply failed");
        let details = error.details.expect("structured transaction details");
        assert_eq!(details["primaryError"]["message"], "runtime apply failed");
        assert_eq!(
            details["compensationErrors"][0]["message"],
            "runtime restore failed"
        );
    }

    #[test]
    fn settings_commit_failure_never_persists_or_publishes() {
        let (backend, _) = backend();
        let previous = backend.get_preferences();
        let mut next = previous.clone();
        next.dictation_hotkey = crate::shared_types::ShortcutBinding {
            primary: "F9".into(),
            modifiers: vec!["ctrl".into()],
        };
        let mut events = backend.subscribe();
        let runtime = RecordingSettingsRuntime {
            fail_commit: true,
            ..RecordingSettingsRuntime::default()
        };

        let error = backend
            .update_settings(next, crate::SettingsUpdateOptions::STRICT, &runtime)
            .unwrap_err();

        assert_eq!(error.code, BackendErrorCode::Platform);
        assert_eq!(
            serde_json::to_value(backend.get_preferences()).unwrap(),
            serde_json::to_value(&previous).unwrap()
        );
        assert_eq!(backend.snapshot().preferences_revision, 0);
        assert!(matches!(
            events.try_recv(),
            Err(crate::EventRecvError::Empty)
        ));
        assert_eq!(
            runtime.actions.lock().unwrap().as_slice(),
            ["prepare", "commit", "restore"]
        );
    }

    #[test]
    fn settings_persistence_failure_restores_prepared_effects() {
        let host = Arc::new(FakeHost::default());
        let data_dir = TestDataDir::new("settings-persistence-failure");
        let mut repositories = BackendRepositories::open(data_dir.path()).unwrap();
        repositories.preferences = Arc::new(crate::PreferencesStore::in_memory());
        let backend = OpenLessBackend::new_with_repositories(
            BackendConfig {
                data_dir: data_dir.path().to_path_buf(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: host,
                text_inserter: Arc::new(FakeInserter),
                dictation_engine: Arc::new(FakeEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
            repositories,
        )
        .unwrap();
        let previous = backend.get_preferences();
        let mut next = previous.clone();
        next.active_asr_provider = "fixture-asr".into();
        let runtime = RecordingSettingsRuntime::default();
        let mut events = backend.subscribe();

        let error = backend
            .update_settings(next, crate::SettingsUpdateOptions::STRICT, &runtime)
            .unwrap_err();

        assert_eq!(error.code, BackendErrorCode::Persistence);
        assert_eq!(
            serde_json::to_value(backend.get_preferences()).unwrap(),
            serde_json::to_value(previous).unwrap()
        );
        assert_eq!(backend.snapshot().preferences_revision, 0);
        assert!(matches!(
            events.try_recv(),
            Err(crate::EventRecvError::Empty)
        ));
        assert_eq!(
            runtime.actions.lock().unwrap().as_slice(),
            ["prepare", "commit", "restore"]
        );
    }

    #[test]
    fn settings_document_reconciles_conflicts_and_preserves_current_style() {
        let (backend, _) = backend();
        let previous = backend.get_preferences();
        let mut next = previous.clone();
        next.microphone_device_name = "updated microphone".into();
        next.default_mode = crate::types::PolishMode::Raw;
        next.enabled_modes = vec![crate::types::PolishMode::Raw];
        next.active_style_pack_id = "stale-style".into();
        next.translation_hotkey = next.dictation_hotkey.clone();
        let mut events = backend.subscribe();

        let outcome = backend
            .update_settings(
                next,
                crate::SettingsUpdateOptions::SETTINGS_DOCUMENT,
                &crate::NoopSettingsRuntime,
            )
            .unwrap();

        assert!(outcome.reconciled_hotkey_count > 0);
        assert_eq!(
            outcome.preferences.microphone_device_name,
            "updated microphone"
        );
        assert_eq!(outcome.preferences.default_mode, previous.default_mode);
        assert_eq!(outcome.preferences.enabled_modes, previous.enabled_modes);
        assert_eq!(
            outcome.preferences.active_style_pack_id,
            previous.active_style_pack_id
        );
        crate::reject_hotkey_collisions(&outcome.preferences).unwrap();
        assert_eq!(backend.snapshot().preferences_revision, 1);
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::PreferencesChanged(PreferencesChange { revision: 1 })
        ));
        assert!(matches!(
            events.try_recv(),
            Err(crate::EventRecvError::Empty)
        ));
    }

    #[test]
    fn settings_revision_guard_rejects_one_of_two_concurrent_stale_documents() {
        let (backend, _) = backend();
        let backend = Arc::new(backend);
        let expected_revision = backend.snapshot().preferences_revision;
        let mut microphone_update = backend.get_preferences();
        microphone_update.microphone_device_name = "concurrent microphone".into();
        let mut theme_update = backend.get_preferences();
        theme_update.theme_mode = crate::shared_types::ThemeMode::Light;
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let spawn = |preferences| {
            let backend = Arc::clone(&backend);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                backend.update_settings(
                    preferences,
                    crate::SettingsUpdateOptions::STRICT.at_revision(expected_revision),
                    &crate::NoopSettingsRuntime,
                )
            })
        };
        let first = spawn(microphone_update);
        let second = spawn(theme_update);
        let mut events = backend.subscribe();
        barrier.wait();

        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let stale = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("one stale settings document");
        assert_eq!(stale.code, BackendErrorCode::Busy);
        assert!(stale.retryable);
        assert_eq!(backend.snapshot().preferences_revision, 1);
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::PreferencesChanged(PreferencesChange { revision: 1 })
        ));
        assert!(matches!(
            events.try_recv(),
            Err(crate::EventRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn credentials_facade_keeps_secrets_out_of_snapshots_and_publishes_status() {
        let credential_store = Arc::new(crate::credentials::InMemoryCredentialStore::default());
        credential_store.set_status(CredentialsStatus {
            active_asr_provider: "fixture-asr".to_string(),
            active_llm_provider: "fixture-llm".to_string(),
            asr_configured: true,
            ..CredentialsStatus::default()
        });
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: std::env::temp_dir().join(format!(
                    "openless-core-credentials-{}",
                    uuid::Uuid::new_v4().simple()
                )),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(FakeInserter),
                dictation_engine: Arc::new(FakeEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: credential_store.clone(),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        let mut events = backend.subscribe();
        let startup = backend.start().await.unwrap();
        assert_eq!(
            startup.backend.credentials.active_asr_provider,
            "fixture-asr"
        );
        assert!(startup.backend.credentials.asr_configured);
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::BackendStarted
        ));

        let key = crate::credentials::CredentialKey::new(
            crate::credentials::CredentialNamespace::Asr,
            Some("fixture-asr".to_string()),
            "api_key",
        )
        .unwrap();
        backend
            .set_credential(
                key.clone(),
                crate::credentials::SecretValue::new("not-in-the-snapshot"),
            )
            .await
            .unwrap();
        assert_eq!(
            backend
                .read_credential(key)
                .await
                .unwrap()
                .unwrap()
                .expose_secret(),
            "not-in-the-snapshot"
        );
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::CredentialsChanged(_)
        ));
        let snapshot_json = serde_json::to_string(&backend.snapshot()).unwrap();
        assert!(!snapshot_json.contains("not-in-the-snapshot"));
        assert!(!snapshot_json.contains("api_key"));
    }

    #[tokio::test]
    async fn provider_channel_facade_owns_mutations_and_active_selection() {
        let credential_store = Arc::new(crate::credentials::InMemoryCredentialStore::default());
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: std::env::temp_dir().join(format!(
                    "openless-core-provider-channels-{}",
                    uuid::Uuid::new_v4().simple()
                )),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(FakeInserter),
                dictation_engine: Arc::new(FakeEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store,
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        let mut events = backend.subscribe();

        let id = backend
            .create_channel(
                crate::credentials::ChannelKind::Asr,
                "openai-compatible".to_string(),
                "Primary".to_string(),
            )
            .await
            .unwrap();
        backend
            .rename_channel(
                crate::credentials::ChannelKind::Asr,
                id.clone(),
                "Renamed".to_string(),
            )
            .await
            .unwrap();
        backend
            .set_active_provider(crate::credentials::ProviderSlot::Asr, id.clone())
            .await
            .unwrap();

        let channels = backend
            .list_channels(crate::credentials::ChannelKind::Asr)
            .await
            .unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].name, "Renamed");
        assert_eq!(
            backend
                .active_provider(crate::credentials::ProviderSlot::Asr)
                .await
                .unwrap(),
            id
        );
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::CredentialsChanged(_)
        ));
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::CredentialsChanged(_)
        ));
        assert!(matches!(
            events.try_recv().unwrap().kind,
            BackendEventKind::CredentialsChanged(_)
        ));
    }

    #[tokio::test]
    async fn lifecycle_is_idempotent_and_emits_started_once_per_transition() {
        let (backend, _) = backend();
        let mut events = backend.subscribe();
        backend.start().await.unwrap();
        backend.start().await.unwrap();
        assert_eq!(
            events.recv().await.unwrap().kind,
            BackendEventKind::BackendStarted
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), events.recv())
                .await
                .is_err()
        );
        backend.shutdown().await.unwrap();
        backend.shutdown().await.unwrap();
        assert_eq!(
            events.recv().await.unwrap().kind,
            BackendEventKind::BackendStopping
        );
    }

    #[tokio::test]
    async fn dictation_captures_preferences_style_and_vocabulary_once_per_session() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-context-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::testing::FixtureDictationEngine::successful("raw", "polished");
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                    InsertOutcome::Inserted,
                )),
                dictation_engine: Arc::new(engine.clone()),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend.start().await.unwrap();
        let mut preferences = backend.get_preferences();
        preferences.microphone_device_name = "Session microphone".to_string();
        preferences.active_asr_provider = "local-qwen3".to_string();
        preferences.active_llm_provider = "openai-compatible".to_string();
        preferences.local_asr_active_model = "qwen3-asr-1.7b".to_string();
        preferences.translation_target_language = "English".to_string();
        preferences.working_languages = vec!["简体中文".to_string()];
        backend.set_preferences(preferences).unwrap();
        backend
            .add_vocabulary("OpenLess".to_string(), None)
            .unwrap();

        backend
            .start_dictation_with_options(DictationStartOptions {
                translation_requested: true,
                style_pack_id: None,
                ..DictationStartOptions::default()
            })
            .await
            .unwrap();
        assert!(backend.snapshot().dictation.translation_active);
        let mut changed = backend.get_preferences();
        changed.microphone_device_name = "Changed microphone".to_string();
        changed.active_asr_provider = "changed-provider".to_string();
        changed.translation_target_language = "日本語".to_string();
        backend.set_preferences(changed).unwrap();
        backend.stop_dictation().await.unwrap();

        let contexts = engine.contexts();
        assert_eq!(contexts.len(), 1);
        let context = &contexts[0];
        assert_eq!(
            context.microphone_device_name.as_deref(),
            Some("Session microphone")
        );
        assert_eq!(context.asr.provider_id, "local-qwen3");
        assert_eq!(context.asr.model.as_deref(), Some("qwen3-asr-1.7b"));
        assert_eq!(context.asr.prompt.as_deref(), Some("OpenLess."));
        assert_eq!(context.polish.translation_target_language, "English");
        assert!(context.polish.translation_active);
        backend.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn stop_time_translation_updates_only_the_frozen_polish_choice() {
        use crate::testing::FixtureEngineAction;

        let data_dir = std::env::temp_dir().join(format!(
            "openless-stop-time-translation-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::testing::FixtureDictationEngine::successful("raw", "translated");
        let backend = backend_with_dictation_engine(data_dir.clone(), Arc::new(engine.clone()));
        backend.start().await.unwrap();
        let mut preferences = backend.get_preferences();
        preferences.microphone_device_name = "Frozen microphone".to_string();
        preferences.active_asr_provider = "local-qwen3".to_string();
        preferences.active_llm_provider = "openai-compatible".to_string();
        preferences.local_asr_active_model = "frozen-asr-model".to_string();
        preferences.translation_target_language = "English".to_string();
        preferences.working_languages = vec!["简体中文".to_string()];
        backend.set_preferences(preferences).unwrap();
        let mut events = backend.subscribe();

        let session_id = backend.start_dictation().await.unwrap();
        assert!(!backend.snapshot().dictation.translation_active);
        let mut changed = backend.get_preferences();
        changed.microphone_device_name = "Changed microphone".to_string();
        changed.active_asr_provider = "changed-provider".to_string();
        changed.active_llm_provider = "changed-provider".to_string();
        changed.local_asr_active_model = "changed-asr-model".to_string();
        changed.translation_target_language = "日本語".to_string();
        changed.working_languages = vec!["English".to_string()];
        backend.set_preferences(changed).unwrap();

        backend
            .stop_dictation_with_options(DictationStopOptions {
                translation_requested: Some(true),
            })
            .await
            .unwrap();

        assert_eq!(
            engine.actions(),
            vec![
                FixtureEngineAction::Start(session_id),
                FixtureEngineAction::UpdateContext(session_id),
                FixtureEngineAction::Finish(session_id),
            ]
        );
        let contexts = engine.contexts();
        assert_eq!(contexts.len(), 2);
        assert!(!contexts[0].polish.translation_active);
        let mut expected = (*contexts[0]).clone();
        expected.polish.translation_active = true;
        assert_eq!(*contexts[1], expected);
        assert_eq!(
            contexts[1].microphone_device_name.as_deref(),
            Some("Frozen microphone")
        );
        assert_eq!(contexts[1].asr.provider_id, "local-qwen3");
        assert_eq!(contexts[1].asr.model.as_deref(), Some("frozen-asr-model"));
        assert_eq!(contexts[1].llm.provider_id, "openai-compatible");
        assert_eq!(contexts[1].polish.translation_target_language, "English");
        assert_eq!(
            contexts[1].polish.working_languages,
            vec!["简体中文".to_string()]
        );
        assert!(backend.list_history().unwrap()[0].translation_active);
        let mut saw_translation_finalization = false;
        while let Ok(event) = events.try_recv() {
            if matches!(
                event.kind,
                BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                    phase: DictationPhase::Transcribing,
                    translation_active: true,
                    ..
                })
            ) {
                saw_translation_finalization = true;
            }
        }
        assert!(saw_translation_finalization);

        backend.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn stop_time_translation_can_disable_the_start_time_choice() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-stop-time-translation-off-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::testing::FixtureDictationEngine::successful("raw", "polished");
        let backend = backend_with_dictation_engine(data_dir.clone(), Arc::new(engine.clone()));
        backend.start().await.unwrap();
        let mut preferences = backend.get_preferences();
        preferences.translation_target_language = "English".to_string();
        preferences.working_languages = vec!["简体中文".to_string()];
        backend.set_preferences(preferences).unwrap();

        backend
            .start_dictation_with_options(DictationStartOptions {
                translation_requested: true,
                ..DictationStartOptions::default()
            })
            .await
            .unwrap();
        assert!(backend.snapshot().dictation.translation_active);
        backend
            .stop_dictation_with_options(DictationStopOptions {
                translation_requested: Some(false),
            })
            .await
            .unwrap();

        let contexts = engine.contexts();
        assert_eq!(contexts.len(), 2);
        assert!(contexts[0].polish.translation_active);
        assert!(!contexts[1].polish.translation_active);
        assert!(!backend.list_history().unwrap()[0].translation_active);

        backend.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn active_translation_update_changes_only_the_session_polish_choice() {
        use crate::testing::FixtureEngineAction;

        let data_dir = std::env::temp_dir().join(format!(
            "openless-active-translation-update-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::testing::FixtureDictationEngine::successful("raw", "translated");
        let backend = backend_with_dictation_engine(data_dir.clone(), Arc::new(engine.clone()));
        backend.start().await.unwrap();
        let mut preferences = backend.get_preferences();
        preferences.microphone_device_name = "Frozen microphone".to_string();
        preferences.translation_target_language = "English".to_string();
        preferences.working_languages = vec!["简体中文".to_string()];
        backend.set_preferences(preferences).unwrap();

        let session_id = backend.start_dictation().await.unwrap();
        backend
            .update_dictation_translation_requested(true)
            .await
            .unwrap();
        assert!(backend.snapshot().dictation.translation_active);
        backend.stop_dictation().await.unwrap();

        assert_eq!(
            engine.actions(),
            vec![
                FixtureEngineAction::Start(session_id),
                FixtureEngineAction::UpdateContext(session_id),
                FixtureEngineAction::Finish(session_id),
            ]
        );
        let contexts = engine.contexts();
        assert_eq!(contexts.len(), 2);
        assert_eq!(
            contexts[1].microphone_device_name.as_deref(),
            Some("Frozen microphone")
        );
        assert!(!contexts[0].polish.translation_active);
        assert!(contexts[1].polish.translation_active);
        assert!(backend.list_history().unwrap()[0].translation_active);

        backend.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn stop_time_context_update_failure_cancels_and_resets_the_session() {
        use crate::testing::FixtureEngineAction;

        let data_dir = std::env::temp_dir().join(format!(
            "openless-stop-time-translation-failure-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::testing::FixtureDictationEngine::failing_context_update(
            "raw",
            "polished",
            BackendError::new(BackendErrorCode::Platform, "fixture context update failure"),
        );
        let backend = backend_with_dictation_engine(data_dir.clone(), Arc::new(engine.clone()));
        backend.start().await.unwrap();
        let mut preferences = backend.get_preferences();
        preferences.translation_target_language = "English".to_string();
        preferences.working_languages = vec!["简体中文".to_string()];
        backend.set_preferences(preferences).unwrap();

        let failed_session = backend.start_dictation().await.unwrap();
        let error = backend
            .stop_dictation_with_options(DictationStopOptions {
                translation_requested: Some(true),
            })
            .await
            .expect_err("context update failure must abort finalization");
        assert_eq!(error.code, BackendErrorCode::Platform);
        assert_eq!(backend.snapshot().dictation.phase, DictationPhase::Idle);
        assert_eq!(backend.snapshot().dictation.session_id, None);
        assert_eq!(
            engine.actions(),
            vec![
                FixtureEngineAction::Start(failed_session),
                FixtureEngineAction::UpdateContext(failed_session),
                FixtureEngineAction::Cancel(failed_session),
            ]
        );
        let history = backend.list_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].error_code.as_deref(), Some("polishFailed"));
        assert!(history[0].translation_active);

        backend.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn external_audio_uses_the_same_pipeline_and_is_strictly_session_scoped() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-external-audio-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let transcription = crate::testing::FixtureTranscriptionEngine::successful("raw", 125);
        let recorder = crate::AudioRecorderRouter::new(
            Arc::new(crate::testing::FixtureAudioRecorder::new(
                Vec::new(),
                Vec::new(),
            )),
            crate::ExternalAudioRecorder::default(),
        );
        let engine = crate::PipelineDictationEngine::new(
            Arc::new(recorder),
            Arc::new(transcription.clone()),
            Arc::new(crate::testing::FixtureTextPolisher::successful("polished")),
        );
        let backend = backend_with_dictation_engine(data_dir.clone(), Arc::new(engine));
        backend.start().await.unwrap();

        let session_id = backend.start_external_dictation().await.unwrap();
        assert_eq!(
            backend
                .feed_external_pcm(SessionId::new(), &[1, 0])
                .unwrap_err()
                .code,
            BackendErrorCode::InvalidState
        );
        backend
            .feed_external_pcm(session_id, &[1, 0, 2, 0])
            .unwrap();
        assert_eq!(backend.snapshot().dictation.elapsed_ms, 0);
        let result = backend.stop_dictation_session(session_id).await.unwrap();
        assert_eq!(result.polished_text, "polished");
        assert_eq!(transcription.pcm(), vec![1, 0, 2, 0]);
        assert_eq!(
            backend
                .feed_external_pcm(session_id, &[3, 0])
                .unwrap_err()
                .code,
            BackendErrorCode::InvalidState
        );

        let cancelled_session = backend.start_external_dictation().await.unwrap();
        backend
            .feed_external_pcm(cancelled_session, &[4, 0])
            .unwrap();
        backend
            .cancel_dictation(Some(cancelled_session))
            .await
            .unwrap();
        assert_eq!(
            backend
                .feed_external_pcm(cancelled_session, &[5, 0])
                .unwrap_err()
                .code,
            BackendErrorCode::InvalidState
        );

        backend.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn dictation_freezes_channel_identity_protocol_and_model_for_the_session() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-channel-snapshot-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::testing::FixtureDictationEngine::successful("raw", "polished");
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                    InsertOutcome::Inserted,
                )),
                dictation_engine: Arc::new(engine.clone()),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        let first_asr = backend
            .create_channel(
                ChannelKind::Asr,
                "openai-compatible".to_string(),
                "ASR first".to_string(),
            )
            .await
            .unwrap();
        let second_asr = backend
            .create_channel(
                ChannelKind::Asr,
                "openai-compatible".to_string(),
                "ASR second".to_string(),
            )
            .await
            .unwrap();
        let first_llm = backend
            .create_channel(
                ChannelKind::Llm,
                "deepseek".to_string(),
                "LLM first".to_string(),
            )
            .await
            .unwrap();
        let second_llm = backend
            .create_channel(
                ChannelKind::Llm,
                "deepseek".to_string(),
                "LLM second".to_string(),
            )
            .await
            .unwrap();
        for (namespace, provider_id, account, model) in [
            (
                crate::credentials::CredentialNamespace::Asr,
                first_asr.clone(),
                "asr.model",
                "asr-model-first",
            ),
            (
                crate::credentials::CredentialNamespace::Asr,
                second_asr.clone(),
                "asr.model",
                "asr-model-second",
            ),
            (
                crate::credentials::CredentialNamespace::Llm,
                first_llm.clone(),
                "ark.model_id",
                "llm-model-first",
            ),
            (
                crate::credentials::CredentialNamespace::Llm,
                second_llm.clone(),
                "ark.model_id",
                "llm-model-second",
            ),
        ] {
            backend
                .set_credential(
                    CredentialKey::new(namespace, Some(provider_id), account).unwrap(),
                    SecretValue::new(model),
                )
                .await
                .unwrap();
        }
        backend
            .set_active_provider(ProviderSlot::Asr, first_asr.clone())
            .await
            .unwrap();
        backend
            .set_active_provider(ProviderSlot::Llm, first_llm.clone())
            .await
            .unwrap();
        backend.start().await.unwrap();

        backend.start_dictation().await.unwrap();
        backend
            .set_active_provider(ProviderSlot::Asr, second_asr)
            .await
            .unwrap();
        backend
            .set_active_provider(ProviderSlot::Llm, second_llm)
            .await
            .unwrap();
        backend.stop_dictation().await.unwrap();

        let contexts = engine.contexts();
        let context = &contexts[0];
        assert_eq!(context.asr.provider_id, first_asr);
        assert_eq!(context.asr.provider_type, "openai-compatible");
        assert_eq!(context.asr.model.as_deref(), Some("asr-model-first"));
        assert_eq!(context.llm.provider_id, first_llm);
        assert_eq!(context.llm.provider_type, "deepseek");
        assert_eq!(context.llm.model.as_deref(), Some("llm-model-first"));

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn completed_dictation_persists_history_and_activity_from_the_session_snapshot() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-completed-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::testing::FixtureDictationEngine::successful_with_metadata(
            "raw voice",
            "translated output",
            Some("polished source".to_string()),
            1250,
        );
        let fixed_clock = Arc::new(crate::testing::FixedClock::new(
            chrono::DateTime::parse_from_rfc3339("2026-08-28T12:34:56Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 28).unwrap(),
        ));
        let backend = OpenLessBackend::new_with_clock(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                    InsertOutcome::Inserted,
                )),
                dictation_engine: Arc::new(engine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
            fixed_clock,
        )
        .unwrap();
        let mut preferences = backend.get_preferences();
        preferences.translation_target_language = "English".to_string();
        preferences.working_languages = vec!["简体中文".to_string()];
        preferences.history_retention_days = 30;
        preferences.history_max_entries = Some(20);
        backend.set_preferences(preferences).unwrap();
        backend.start().await.unwrap();

        let session_id = backend
            .start_dictation_with_options(DictationStartOptions {
                translation_requested: true,
                front_app: Some("Visual Studio Code".to_string()),
                ..DictationStartOptions::default()
            })
            .await
            .unwrap();
        let result = backend.stop_dictation().await.unwrap();

        assert_eq!(result.session_id, session_id);
        assert_eq!(result.polish_source.as_deref(), Some("polished source"));
        assert_eq!(result.duration_ms, 1250);
        let history = backend.list_history().unwrap();
        assert_eq!(history.len(), 1);
        let entry = &history[0];
        assert_eq!(entry.id, session_id.to_string());
        assert_eq!(entry.created_at, "2026-08-28T12:34:56+00:00");
        assert_eq!(entry.raw_transcript, "raw voice");
        assert_eq!(entry.final_text, "translated output");
        assert_eq!(entry.polish_source.as_deref(), Some("polished source"));
        assert!(entry.translation_active);
        assert_eq!(entry.duration_ms, Some(1250));
        assert_eq!(
            entry.insert_status,
            crate::types::HistoryInsertStatus::Inserted
        );
        let activity = backend.list_activity().unwrap();
        assert_eq!(activity.len(), 1);
        assert_eq!(activity[0].date, "2026-08-28");
        assert_eq!(
            activity[0].chars,
            "translated output".chars().count() as u64
        );
        assert_eq!(activity[0].duration_ms, 1250);

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn completed_dictation_applies_enabled_correction_rules_before_insert_and_history() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-completed-correction-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let inserter = crate::testing::FixtureTextInserter::with_outcome(InsertOutcome::Inserted);
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(inserter.clone()),
                dictation_engine: Arc::new(crate::testing::FixtureDictationEngine::successful(
                    "10粒样品和禁用词",
                    "10粒样品和禁用词",
                )),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend
            .add_correction_rule("{num}粒".to_string(), "{num}例".to_string())
            .unwrap();
        let disabled = backend
            .add_correction_rule("禁用词".to_string(), "不应出现".to_string())
            .unwrap();
        backend
            .set_correction_rule_enabled(&disabled.id, false)
            .unwrap();
        backend.start().await.unwrap();

        backend.start_dictation().await.unwrap();
        let result = backend.stop_dictation().await.unwrap();

        assert_eq!(result.polished_text, "10例样品和禁用词");
        assert!(inserter.actions().iter().any(|action| matches!(
            action,
            crate::testing::FixtureInsertionAction::Insert { text, .. }
                if text == "10例样品和禁用词"
        )));
        assert_eq!(
            backend.list_history().unwrap()[0].final_text,
            result.polished_text
        );

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn multimodal_history_attributes_success_to_the_frozen_omni_provider() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-omni-success-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::dictation_engine::PipelineDictationEngine::new(
            Arc::new(crate::testing::FixtureAudioRecorder::new(
                vec![vec![1, 0, 2, 0]],
                vec![(40, 0.25)],
            )),
            Arc::new(crate::testing::FixtureTranscriptionEngine::successful(
                "omni raw", 40,
            )),
            Arc::new(crate::testing::FixtureTextPolisher::successful(
                "omni final",
            )),
        );
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                    InsertOutcome::Inserted,
                )),
                dictation_engine: Arc::new(engine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        let mut preferences = backend.get_preferences();
        preferences.pipeline_mode = crate::shared_types::PipelineMode::Multimodal;
        backend.set_preferences(preferences).unwrap();
        backend
            .set_active_provider(ProviderSlot::Omni, "omni-channel".to_string())
            .await
            .unwrap();
        backend
            .set_credential(
                CredentialKey::new(
                    crate::credentials::CredentialNamespace::Omni,
                    None,
                    "omni.model",
                )
                .unwrap(),
                SecretValue::new("omni-model"),
            )
            .await
            .unwrap();
        backend.start().await.unwrap();

        backend.start_dictation().await.unwrap();
        backend.stop_dictation().await.unwrap();

        let history = backend.list_history().unwrap();
        let entry = &history[0];
        assert_eq!(entry.pipeline_mode.as_deref(), Some("multimodal"));
        assert_eq!(entry.asr_provider, None);
        assert_eq!(entry.asr_model, None);
        assert_eq!(entry.asr_ms, None);
        assert_eq!(entry.llm_provider.as_deref(), Some("omni-channel"));
        assert_eq!(entry.llm_model.as_deref(), Some("omni-model"));
        assert!(entry.polish_ms.is_some());

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn multimodal_history_attributes_failure_to_the_frozen_omni_provider() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-omni-failure-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                    InsertOutcome::Inserted,
                )),
                dictation_engine: Arc::new(PolishMetadataFailingEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        let mut preferences = backend.get_preferences();
        preferences.pipeline_mode = crate::shared_types::PipelineMode::Multimodal;
        backend.set_preferences(preferences).unwrap();
        backend
            .set_active_provider(ProviderSlot::Omni, "omni-failure-channel".to_string())
            .await
            .unwrap();
        backend.start().await.unwrap();

        backend.start_dictation().await.unwrap();
        backend.stop_dictation().await.unwrap_err();

        let history = backend.list_history().unwrap();
        let entry = &history[0];
        assert_eq!(entry.pipeline_mode.as_deref(), Some("multimodal"));
        assert_eq!(entry.asr_provider, None);
        assert_eq!(entry.asr_model, None);
        assert_eq!(entry.asr_ms, None);
        assert_eq!(entry.llm_provider.as_deref(), Some("omni-failure-channel"));
        assert_eq!(entry.polish_ms, Some(600));

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn polish_fallback_persists_the_polish_failed_history_code() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-polish-fallback-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::dictation_engine::PipelineDictationEngine::new(
            Arc::new(
                crate::testing::FixtureAudioRecorder::new(vec![vec![1, 0, 2, 0]], vec![(25, 0.5)])
                    .with_archived_recording(true),
            ),
            Arc::new(crate::testing::FixtureTranscriptionEngine::successful(
                "raw fallback",
                25,
            )),
            Arc::new(crate::testing::FixtureTextPolisher::failing(
                BackendError::new(BackendErrorCode::Provider, "fixture polish failure"),
            )),
        );
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                    InsertOutcome::Inserted,
                )),
                dictation_engine: Arc::new(engine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend.start().await.unwrap();

        backend.start_dictation().await.unwrap();
        let result = backend.stop_dictation().await.unwrap();

        assert_eq!(result.polished_text, "raw fallback");
        let history = backend.list_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].error_code.as_deref(), Some("polishFailed"));
        assert!(history[0].asr_ms.is_some());
        assert!(history[0].polish_ms.is_some());
        assert_eq!(history[0].has_audio_recording, Some(false));

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn empty_transcript_persists_a_failed_history_entry_without_activity() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-empty-transcript-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::dictation_engine::PipelineDictationEngine::new(
            Arc::new(crate::testing::FixtureAudioRecorder::new(
                vec![vec![1, 0, 2, 0]],
                vec![(40, 0.25)],
            )),
            Arc::new(crate::testing::FixtureTranscriptionEngine::successful(
                "   ", 40,
            )),
            Arc::new(crate::testing::FixtureTextPolisher::successful(
                "must not be inserted",
            )),
        );
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                    InsertOutcome::Inserted,
                )),
                dictation_engine: Arc::new(engine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend.start().await.unwrap();

        let session_id = backend.start_dictation().await.unwrap();
        let error = backend
            .stop_dictation()
            .await
            .expect_err("empty transcript must fail");

        assert_eq!(error.code, BackendErrorCode::Provider);
        let history = backend.list_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, session_id.to_string());
        assert_eq!(history[0].error_code.as_deref(), Some("emptyTranscript"));
        assert_eq!(
            history[0].insert_status,
            crate::types::HistoryInsertStatus::Failed
        );
        assert!(backend.list_activity().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn asr_finish_failure_persists_history_and_releases_the_session() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-asr-failure-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                    InsertOutcome::Inserted,
                )),
                dictation_engine: Arc::new(crate::testing::FixtureDictationEngine::failing(
                    BackendError::new(BackendErrorCode::Provider, "fixture ASR failure"),
                )),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend.start().await.unwrap();

        let session_id = backend.start_dictation().await.unwrap();
        let error = backend.stop_dictation().await.expect_err("ASR must fail");

        assert_eq!(error.code, BackendErrorCode::Provider);
        let history = backend.list_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, session_id.to_string());
        assert_eq!(history[0].error_code.as_deref(), Some("transcribeFailed"));
        assert_eq!(backend.snapshot().dictation.phase, DictationPhase::Idle);
        assert!(backend.start_dictation().await.is_ok());

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn pipeline_asr_failure_preserves_archive_and_timing_diagnostics() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-asr-diagnostics-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::dictation_engine::PipelineDictationEngine::new(
            Arc::new(
                crate::testing::FixtureAudioRecorder::new(vec![vec![1, 0, 2, 0]], vec![(80, 0.5)])
                    .with_archived_recording(true),
            ),
            Arc::new(crate::testing::FixtureTranscriptionEngine::failing(
                BackendError::new(BackendErrorCode::Provider, "fixture ASR failure"),
            )),
            Arc::new(crate::testing::FixtureTextPolisher::successful("unused")),
        );
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                    InsertOutcome::Inserted,
                )),
                dictation_engine: Arc::new(engine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend.start().await.unwrap();

        backend.start_dictation().await.unwrap();
        backend.stop_dictation().await.expect_err("ASR must fail");

        let history = backend.list_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].error_code.as_deref(), Some("transcribeFailed"));
        assert_eq!(history[0].has_audio_recording, Some(true));
        assert!(history[0].asr_ms.is_some());

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn engine_start_failure_persists_history_and_releases_the_session() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-start-failure-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let inserter = crate::testing::FixtureTextInserter::with_outcome(InsertOutcome::Inserted);
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(inserter.clone()),
                dictation_engine: Arc::new(StartFailingEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend.start().await.unwrap();

        let error = backend
            .start_dictation()
            .await
            .expect_err("engine start must fail");

        assert_eq!(error.code, BackendErrorCode::Platform);
        let history = backend.list_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].error_code.as_deref(), Some("transcribeFailed"));
        assert_eq!(backend.snapshot().dictation.phase, DictationPhase::Idle);
        let actions = inserter.actions();
        assert_eq!(actions.len(), 2);
        let prepared_session = match &actions[0] {
            crate::testing::FixtureInsertionAction::Prepare(session_id) => *session_id,
            action => panic!("unexpected first insertion action: {action:?}"),
        };
        assert_eq!(
            actions[1],
            crate::testing::FixtureInsertionAction::Cancel(prepared_session)
        );
        assert!(backend.start_dictation().await.is_err());
        assert_eq!(backend.list_history().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn insertion_failure_persists_generated_text_and_releases_the_session() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-core-insert-failure-history-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(crate::testing::FixtureTextInserter::failing(
                    BackendError::new(BackendErrorCode::Platform, "fixture insertion failure"),
                )),
                dictation_engine: Arc::new(
                    crate::testing::FixtureDictationEngine::successful_with_metadata(
                        "raw voice",
                        "generated text",
                        Some("polished source".to_string()),
                        600,
                    ),
                ),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend.start().await.unwrap();

        let session_id = backend.start_dictation().await.unwrap();
        let error = backend
            .stop_dictation()
            .await
            .expect_err("insertion must fail");

        assert_eq!(error.code, BackendErrorCode::Platform);
        let history = backend.list_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, session_id.to_string());
        assert_eq!(history[0].raw_transcript, "raw voice");
        assert_eq!(history[0].final_text, "generated text");
        assert_eq!(history[0].polish_source.as_deref(), Some("polished source"));
        assert_eq!(history[0].error_code.as_deref(), Some("insertFailed"));
        assert_eq!(
            history[0].insert_status,
            crate::types::HistoryInsertStatus::Failed
        );
        assert_eq!(backend.snapshot().dictation.phase, DictationPhase::Idle);
        assert!(backend.list_activity().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn dictation_runs_through_engine_inserter_and_host_actions() {
        let (backend, host) = backend();
        let mut events = backend.subscribe();
        backend.start().await.unwrap();
        let session = backend.start_dictation().await.unwrap();
        assert_eq!(
            backend.stop_dictation().await.unwrap().polished_text,
            "polished"
        );
        let mut emitted = Vec::new();
        while let Ok(event) = events.try_recv() {
            emitted.push(event);
        }
        assert!(matches!(emitted[0].kind, BackendEventKind::BackendStarted));
        assert_eq!(
            emitted
                .iter()
                .filter(|event| event.session_id == Some(session))
                .filter_map(|event| match &event.kind {
                    BackendEventKind::DictationStateChanged(snapshot) => Some(snapshot.phase),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![
                DictationPhase::Starting,
                DictationPhase::Recording,
                DictationPhase::Transcribing,
                DictationPhase::Inserting,
                DictationPhase::Completed,
            ]
        );
        assert!(emitted.iter().any(|event| {
            event.session_id == Some(session)
                && matches!(event.kind, BackendEventKind::DictationCompleted(_))
        }));
        let actions = host.0.lock().unwrap();
        assert_eq!(
            actions.as_slice(),
            &[
                HostAction::ShowDictationFeedback,
                HostAction::HideDictationFeedback
            ]
        );
    }

    #[tokio::test]
    async fn cli_dictation_intents_use_the_same_core_state_machine() {
        let (backend, _) = backend();
        backend.start().await.unwrap();

        let started = backend
            .dispatch_cli_intent(crate::cli::CliIntent::ToggleDictation)
            .await
            .unwrap();
        let session_id = match started {
            CliDispatchOutcome::DictationStarted(session_id) => session_id,
            other => panic!("unexpected start outcome: {other:?}"),
        };
        assert_eq!(
            backend.snapshot().dictation.phase,
            DictationPhase::Recording
        );

        let completed = backend
            .dispatch_cli_intent(crate::cli::CliIntent::ToggleDictation)
            .await
            .unwrap();
        assert!(matches!(
            completed,
            CliDispatchOutcome::DictationCompleted(DictationResult {
                session_id: completed_session,
                ..
            }) if completed_session == session_id
        ));
        assert_eq!(backend.snapshot().dictation.phase, DictationPhase::Idle);

        assert_eq!(
            backend
                .dispatch_cli_intent(crate::cli::CliIntent::CancelDictation)
                .await
                .unwrap(),
            CliDispatchOutcome::Noop
        );
    }

    #[tokio::test]
    async fn cancelling_wrong_session_is_rejected_without_mutating_state() {
        let (backend, _) = backend();
        backend.start().await.unwrap();
        let active = backend.start_dictation().await.unwrap();
        let wrong = SessionId::new();
        let error = backend.cancel_dictation(Some(wrong)).await.unwrap_err();
        assert_eq!(error.code, BackendErrorCode::InvalidArgument);
        assert_eq!(backend.snapshot().dictation.session_id, Some(active));
    }

    #[tokio::test]
    async fn engine_failure_publishes_failed_state_and_preserves_session_identity() {
        let host = Arc::new(FakeHost::default());
        let data_dir = TestDataDir::new("engine-failure");
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.path().to_path_buf(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: host,
                text_inserter: Arc::new(FakeInserter),
                dictation_engine: Arc::new(FailingEngine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();

        let mut events = backend.subscribe();
        backend.start().await.unwrap();
        let session = backend.start_dictation().await.unwrap();
        let error = backend.stop_dictation().await.unwrap_err();

        assert_eq!(error.code, BackendErrorCode::Provider);
        let snapshot = backend.snapshot();
        assert_eq!(snapshot.dictation.session_id, None);
        assert_eq!(snapshot.dictation.phase, DictationPhase::Idle);
        let mut failed_session = None;
        while let Ok(event) = events.try_recv() {
            if let BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                session_id,
                phase: DictationPhase::Failed,
                ..
            }) = event.kind
            {
                failed_session = session_id;
            }
        }
        assert_eq!(failed_session, Some(session));
    }

    #[tokio::test]
    async fn stop_is_rejected_after_the_session_has_reached_idle() {
        let (backend, _) = backend();
        backend.start().await.unwrap();
        backend.start_dictation().await.unwrap();
        backend.stop_dictation().await.unwrap();

        let error = backend.stop_dictation().await.unwrap_err();
        assert_eq!(error.code, BackendErrorCode::InvalidState);
    }

    #[tokio::test]
    async fn fallback_and_unknown_outcomes_are_explicit_events() {
        for outcome in [InsertOutcome::CopiedFallback, InsertOutcome::Unknown] {
            let host = Arc::new(FakeHost::default());
            let data_dir = TestDataDir::new("insert-outcome");
            let backend = OpenLessBackend::new(
                BackendConfig {
                    data_dir: data_dir.path().to_path_buf(),
                    ..BackendConfig::default()
                },
                BackendDependencies {
                    host_actions: host,
                    text_inserter: Arc::new(crate::testing::FixtureTextInserter::with_outcome(
                        outcome,
                    )),
                    dictation_engine: Arc::new(FakeEngine),
                    task_spawner: Arc::new(TokioTaskSpawner),
                    credential_store: Arc::new(
                        crate::credentials::InMemoryCredentialStore::default(),
                    ),
                    services: crate::domains::BackendServices::unsupported(),
                    local_asr_runtime: None,
                    marketplace_config: None,
                    selection_runtime: None,
                    selection_polisher: None,
                    qa_runtime: None,
                },
            )
            .unwrap();
            let mut events = backend.subscribe();
            backend.start().await.unwrap();
            backend.start_dictation().await.unwrap();
            let result = backend.stop_dictation().await.unwrap();
            assert_eq!(result.inserted, outcome.into_status());

            let mut fallback_payload = None;
            loop {
                match events.try_recv() {
                    Ok(event) => {
                        if let BackendEventKind::InsertFallback(payload) = event.kind {
                            fallback_payload = Some(payload);
                        }
                    }
                    Err(crate::events::EventRecvError::Empty) => break,
                    Err(error) => panic!("unexpected event error: {error}"),
                }
            }
            assert_eq!(
                fallback_payload
                    .expect("fallback outcome must be visible to both hosts")
                    .copied_text
                    .as_deref(),
                Some("polished")
            );
        }
    }

    #[tokio::test]
    async fn cancellation_emits_cancelled_state_and_clears_the_session() {
        let (backend, host) = backend();
        let mut events = backend.subscribe();
        backend.start().await.unwrap();
        let session = backend.start_dictation().await.unwrap();
        backend.cancel_dictation(Some(session)).await.unwrap();

        assert_eq!(
            backend.snapshot().dictation,
            DictationStateSnapshot::default()
        );
        assert_eq!(
            *host.0.lock().unwrap(),
            vec![
                HostAction::ShowDictationFeedback,
                HostAction::HideDictationFeedback
            ]
        );
        let mut saw_cancelled = false;
        while let Ok(event) = events.try_recv() {
            if matches!(
                event.kind,
                BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                    phase: DictationPhase::Cancelled,
                    ..
                })
            ) {
                saw_cancelled = true;
            }
        }
        assert!(saw_cancelled);
        assert_eq!(
            backend.stop_dictation().await.unwrap_err().code,
            BackendErrorCode::InvalidState
        );
    }

    #[tokio::test]
    async fn engine_receives_start_finish_and_cancel_lifecycle_calls() {
        use crate::testing::{
            FixtureDictationEngine, FixtureEngineAction, FixtureInsertionAction,
            FixtureTextInserter,
        };

        let completed_engine = FixtureDictationEngine::successful("raw", "polished");
        let completed_inserter = FixtureTextInserter::with_outcome(InsertOutcome::Inserted);
        let completed_data_dir = TestDataDir::new("engine-lifecycle-completed");
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: completed_data_dir.path().to_path_buf(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(completed_inserter.clone()),
                dictation_engine: Arc::new(completed_engine.clone()),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend.start().await.unwrap();
        let completed = backend.start_dictation().await.unwrap();
        backend.stop_dictation().await.unwrap();
        assert_eq!(
            completed_engine.actions(),
            vec![
                FixtureEngineAction::Start(completed),
                FixtureEngineAction::Finish(completed),
            ]
        );
        assert_eq!(
            completed_inserter.actions(),
            vec![
                FixtureInsertionAction::Prepare(completed),
                FixtureInsertionAction::Insert {
                    session_id: completed,
                    text: "polished".to_string(),
                },
            ]
        );

        let cancelled_engine = FixtureDictationEngine::successful("raw", "polished");
        let cancelled_inserter = FixtureTextInserter::with_outcome(InsertOutcome::Inserted);
        let cancelled_data_dir = TestDataDir::new("engine-lifecycle-cancelled");
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: cancelled_data_dir.path().to_path_buf(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(cancelled_inserter.clone()),
                dictation_engine: Arc::new(cancelled_engine.clone()),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend.start().await.unwrap();
        let cancelled = backend.start_dictation().await.unwrap();
        backend.cancel_dictation(Some(cancelled)).await.unwrap();
        assert_eq!(
            cancelled_engine.actions(),
            vec![
                FixtureEngineAction::Start(cancelled),
                FixtureEngineAction::Cancel(cancelled),
            ]
        );
        assert_eq!(
            cancelled_inserter.actions(),
            vec![
                FixtureInsertionAction::Prepare(cancelled),
                FixtureInsertionAction::Cancel(cancelled),
            ]
        );

        let shutdown_engine = FixtureDictationEngine::successful("raw", "polished");
        let shutdown_inserter = FixtureTextInserter::with_outcome(InsertOutcome::Inserted);
        let shutdown_data_dir = TestDataDir::new("engine-lifecycle-shutdown");
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: shutdown_data_dir.path().to_path_buf(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(shutdown_inserter.clone()),
                dictation_engine: Arc::new(shutdown_engine.clone()),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        backend.start().await.unwrap();
        let interrupted = backend.start_dictation().await.unwrap();
        backend.shutdown().await.unwrap();
        assert_eq!(
            shutdown_engine.actions(),
            vec![
                FixtureEngineAction::Start(interrupted),
                FixtureEngineAction::Cancel(interrupted),
            ]
        );
        assert_eq!(
            shutdown_inserter.actions(),
            vec![
                FixtureInsertionAction::Prepare(interrupted),
                FixtureInsertionAction::Cancel(interrupted),
            ]
        );
    }

    #[tokio::test]
    async fn engine_progress_is_session_scoped_and_orders_stage_delta_and_terminal_events() {
        let engine = crate::testing::FixtureDictationEngine::successful("raw", "polished");
        let data_dir = TestDataDir::new("engine-progress");
        let backend = OpenLessBackend::new(
            BackendConfig {
                data_dir: data_dir.path().to_path_buf(),
                ..BackendConfig::default()
            },
            BackendDependencies {
                host_actions: Arc::new(FakeHost::default()),
                text_inserter: Arc::new(FakeInserter),
                dictation_engine: Arc::new(engine),
                task_spawner: Arc::new(TokioTaskSpawner),
                credential_store: Arc::new(crate::credentials::InMemoryCredentialStore::default()),
                services: crate::domains::BackendServices::unsupported(),
                local_asr_runtime: None,
                marketplace_config: None,
                selection_runtime: None,
                selection_polisher: None,
                qa_runtime: None,
            },
        )
        .unwrap();
        let mut events = backend.subscribe();
        backend.start().await.unwrap();
        let session = backend.start_dictation().await.unwrap();
        backend.stop_dictation().await.unwrap();

        let mut session_events = Vec::new();
        while let Ok(event) = events.try_recv() {
            if event.session_id == Some(session) {
                session_events.push(event);
            }
        }
        assert!(session_events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
        assert!(matches!(
            session_events[0].kind,
            BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                phase: DictationPhase::Starting,
                ..
            })
        ));
        assert!(session_events.iter().any(|event| matches!(
            event.kind,
            BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                phase: DictationPhase::Recording,
                ..
            })
        )));
        assert!(session_events.iter().any(|event| matches!(
            event.kind,
            BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                phase: DictationPhase::Transcribing,
                ..
            })
        )));
        assert!(session_events
            .iter()
            .any(|event| matches!(event.kind, BackendEventKind::TranscriptDelta(_))));
        assert!(session_events.iter().any(|event| matches!(
            event.kind,
            BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                phase: DictationPhase::Polishing,
                ..
            })
        )));
        assert!(session_events
            .iter()
            .any(|event| matches!(event.kind, BackendEventKind::PolishDelta(_))));
        assert!(session_events.iter().any(|event| matches!(
            event.kind,
            BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                phase: DictationPhase::Inserting,
                ..
            })
        )));
        assert!(matches!(
            session_events[session_events.len() - 2].kind,
            BackendEventKind::DictationCompleted(_)
        ));
        assert!(matches!(
            session_events.last().unwrap().kind,
            BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                phase: DictationPhase::Completed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn late_engine_progress_is_rejected_after_session_cancellation() {
        let (backend, _) = backend();
        backend.start().await.unwrap();
        let session = backend.start_dictation().await.unwrap();
        let progress = BackendEngineProgress {
            events: Arc::clone(&backend.events),
            state: Arc::clone(&backend.state),
            phase_changed: Arc::clone(&backend.phase_changed),
        };
        backend.cancel_dictation(Some(session)).await.unwrap();

        let error = progress
            .publish(
                session,
                EngineProgress::TranscriptDelta(crate::types::TranscriptDelta {
                    text: "late".to_string(),
                    offset: 0,
                    is_final: false,
                }),
            )
            .unwrap_err();
        assert_eq!(error.code, BackendErrorCode::Cancelled);
    }

    #[tokio::test]
    async fn stop_requested_while_engine_starts_waits_and_finishes_same_session() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let data_dir = TestDataDir::new("stop-during-start");
        let backend = Arc::new(
            OpenLessBackend::new(
                BackendConfig {
                    data_dir: data_dir.path().to_path_buf(),
                    ..BackendConfig::default()
                },
                BackendDependencies {
                    host_actions: Arc::new(FakeHost::default()),
                    text_inserter: Arc::new(FakeInserter),
                    dictation_engine: Arc::new(BlockingStartEngine {
                        entered: Arc::clone(&entered),
                        release: Arc::clone(&release),
                    }),
                    task_spawner: Arc::new(TokioTaskSpawner),
                    credential_store: Arc::new(
                        crate::credentials::InMemoryCredentialStore::default(),
                    ),
                    services: crate::domains::BackendServices::unsupported(),
                    local_asr_runtime: None,
                    marketplace_config: None,
                    selection_runtime: None,
                    selection_polisher: None,
                    qa_runtime: None,
                },
            )
            .unwrap(),
        );
        backend.start().await.unwrap();
        let starting_backend = Arc::clone(&backend);
        let start_task = tokio::spawn(async move { starting_backend.start_dictation().await });
        entered.notified().await;
        let expected_session = backend.snapshot().dictation.session_id.unwrap();

        let stopping_backend = Arc::clone(&backend);
        let stop_task = tokio::spawn(async move { stopping_backend.stop_dictation().await });
        tokio::task::yield_now().await;
        assert!(!stop_task.is_finished());

        release.notify_one();
        assert_eq!(start_task.await.unwrap().unwrap(), expected_session);
        let result = stop_task.await.unwrap().unwrap();
        assert_eq!(result.session_id, expected_session);
        assert_eq!(result.polished_text, "polished");
        assert_eq!(backend.snapshot().dictation.phase, DictationPhase::Idle);
    }

    #[tokio::test]
    async fn cancellation_while_engine_starts_never_reenters_recording() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let data_dir = TestDataDir::new("cancel-during-start");
        let backend = Arc::new(
            OpenLessBackend::new(
                BackendConfig {
                    data_dir: data_dir.path().to_path_buf(),
                    ..BackendConfig::default()
                },
                BackendDependencies {
                    host_actions: Arc::new(FakeHost::default()),
                    text_inserter: Arc::new(FakeInserter),
                    dictation_engine: Arc::new(BlockingStartEngine {
                        entered: Arc::clone(&entered),
                        release: Arc::clone(&release),
                    }),
                    task_spawner: Arc::new(TokioTaskSpawner),
                    credential_store: Arc::new(
                        crate::credentials::InMemoryCredentialStore::default(),
                    ),
                    services: crate::domains::BackendServices::unsupported(),
                    local_asr_runtime: None,
                    marketplace_config: None,
                    selection_runtime: None,
                    selection_polisher: None,
                    qa_runtime: None,
                },
            )
            .unwrap(),
        );
        backend.start().await.unwrap();
        let mut events = backend.subscribe();
        let starting_backend = Arc::clone(&backend);
        let start_task = tokio::spawn(async move { starting_backend.start_dictation().await });
        entered.notified().await;

        let session = backend.snapshot().dictation.session_id.unwrap();
        assert_eq!(backend.snapshot().dictation.phase, DictationPhase::Starting);
        backend.cancel_dictation(Some(session)).await.unwrap();
        release.notify_one();
        assert_eq!(
            start_task.await.unwrap().unwrap_err().code,
            BackendErrorCode::Cancelled
        );
        assert_eq!(backend.snapshot().dictation.phase, DictationPhase::Idle);

        let mut saw_recording_after_cancel = false;
        let mut cancelled_sequence = None;
        while let Ok(event) = events.try_recv() {
            if matches!(
                event.kind,
                BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                    phase: DictationPhase::Cancelled,
                    ..
                })
            ) {
                cancelled_sequence = Some(event.sequence);
            }
            if cancelled_sequence.is_some_and(|sequence| event.sequence > sequence)
                && matches!(
                    event.kind,
                    BackendEventKind::DictationStateChanged(DictationStateSnapshot {
                        phase: DictationPhase::Recording,
                        ..
                    })
                )
            {
                saw_recording_after_cancel = true;
            }
        }
        assert!(!saw_recording_after_cancel);
    }

    #[tokio::test]
    async fn shared_hotkey_edges_own_hold_auto_and_combo_abort_semantics() {
        let (backend, _) = backend();
        backend.start().await.unwrap();

        let mut preferences = backend.get_preferences();
        preferences.hotkey.mode = crate::shared_types::HotkeyMode::Hold;
        backend.set_preferences(preferences).unwrap();
        let pressed_at = std::time::Instant::now();
        assert!(matches!(
            backend
                .dispatch_dictation_hotkey_edge(DictationHotkeyEdge::Pressed { at: pressed_at })
                .await
                .unwrap(),
            CliDispatchOutcome::DictationStarted(_)
        ));
        assert!(matches!(
            backend
                .dispatch_dictation_hotkey_edge(DictationHotkeyEdge::Released {
                    at: pressed_at + std::time::Duration::from_millis(50),
                })
                .await
                .unwrap(),
            CliDispatchOutcome::DictationCompleted(_)
        ));

        let mut preferences = backend.get_preferences();
        preferences.hotkey.mode = crate::shared_types::HotkeyMode::Auto;
        backend.set_preferences(preferences).unwrap();
        let short_press = std::time::Instant::now();
        backend
            .dispatch_dictation_hotkey_edge(DictationHotkeyEdge::Pressed { at: short_press })
            .await
            .unwrap();
        assert_eq!(
            backend
                .dispatch_dictation_hotkey_edge(DictationHotkeyEdge::Released {
                    at: short_press + std::time::Duration::from_millis(100),
                })
                .await
                .unwrap(),
            CliDispatchOutcome::Noop
        );
        assert_eq!(
            backend.snapshot().dictation.phase,
            DictationPhase::Recording
        );
        assert!(matches!(
            backend
                .dispatch_dictation_hotkey_edge(DictationHotkeyEdge::Pressed {
                    at: std::time::Instant::now(),
                })
                .await
                .unwrap(),
            CliDispatchOutcome::DictationCompleted(_)
        ));

        let long_press = std::time::Instant::now();
        backend
            .dispatch_dictation_hotkey_edge(DictationHotkeyEdge::Pressed { at: long_press })
            .await
            .unwrap();
        assert!(matches!(
            backend
                .dispatch_dictation_hotkey_edge(DictationHotkeyEdge::Released {
                    at: long_press + std::time::Duration::from_millis(500),
                })
                .await
                .unwrap(),
            CliDispatchOutcome::DictationCompleted(_)
        ));

        backend
            .dispatch_dictation_hotkey_edge(DictationHotkeyEdge::Pressed {
                at: std::time::Instant::now(),
            })
            .await
            .unwrap();
        assert_eq!(
            backend
                .dispatch_dictation_hotkey_edge(DictationHotkeyEdge::Combined)
                .await
                .unwrap(),
            CliDispatchOutcome::DictationCancelled
        );
        assert_eq!(backend.snapshot().dictation.phase, DictationPhase::Idle);
    }

    #[tokio::test]
    async fn shared_toggle_hotkey_applies_stop_time_translation_options() {
        use crate::testing::FixtureEngineAction;

        let data_dir = std::env::temp_dir().join(format!(
            "openless-hotkey-stop-translation-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let engine = crate::testing::FixtureDictationEngine::successful("raw", "translated");
        let backend = backend_with_dictation_engine(data_dir.clone(), Arc::new(engine.clone()));
        backend.start().await.unwrap();
        let mut preferences = backend.get_preferences();
        preferences.hotkey.mode = crate::shared_types::HotkeyMode::Toggle;
        preferences.translation_target_language = "English".to_string();
        preferences.working_languages = vec!["简体中文".to_string()];
        backend.set_preferences(preferences).unwrap();

        let pressed_at = std::time::Instant::now();
        let session_id = match backend
            .dispatch_dictation_hotkey_edge(DictationHotkeyEdge::Pressed { at: pressed_at })
            .await
            .unwrap()
        {
            CliDispatchOutcome::DictationStarted(session_id) => session_id,
            other => panic!("unexpected start outcome: {other:?}"),
        };
        let outcome = backend
            .dispatch_dictation_hotkey_edge_with_session_options(
                DictationHotkeyEdge::Pressed {
                    at: pressed_at + std::time::Duration::from_secs(1),
                },
                DictationHotkeyDispatchOptions {
                    start: DictationStartOptions::default(),
                    stop: DictationStopOptions {
                        translation_requested: Some(true),
                    },
                },
            )
            .await
            .unwrap();
        assert!(matches!(outcome, CliDispatchOutcome::DictationCompleted(_)));
        assert_eq!(
            engine.actions(),
            vec![
                FixtureEngineAction::Start(session_id),
                FixtureEngineAction::UpdateContext(session_id),
                FixtureEngineAction::Finish(session_id),
            ]
        );
        assert!(backend.list_history().unwrap()[0].translation_active);

        backend.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(data_dir);
    }
}
