use std::sync::Arc;

use futures_util::future::BoxFuture;

use openless_core::{
    normalize_coding_agent_executable, AudioConsumer, AudioRecorder, BackendConfig,
    BackendDependencies, BackendError, BackendErrorCode, BackendRepositories, BackendServices,
    CodingAgentApi, CodingAgentAvailability, CodingAgentDetectRequest, CodingAgentModelsRequest,
    CodingAgentProvider, CodingAgentRequest, CodingAgentTestRequest, CodingAgentTestStatus,
    CommandRisk, CommandRiskAssessment, CredentialStore, DictationEngine, DictationEngineRouter,
    LessComputerRuntimeAdapter, MarketplaceConfig, OpenLessBackend, PipelineDictationEngine,
    PolishFailurePolicy, ProviderService, SettingsRuntime, SharedAuxiliaryTextPolisher,
    SharedCloudTextPolisher, SharedCloudTranscriptionEngine, SharedOmniDictationEngine,
    TextInserter, TextPolisher, TextPolisherRouter, TextStreamSink, TokioTaskSpawner,
    TranscriptOutput, TranscriptionEngine, TranscriptionRouter, TranscriptionSession,
    SHARED_CLOUD_ASR_PROVIDER_TYPES, SHARED_CLOUD_LLM_PROVIDER_TYPES, SHARED_OMNI_PROVIDER_TYPES,
};

use crate::{
    Fcitx5TextInserter, LinuxCpalRecorder, LinuxCredentialStore, LinuxHostActions,
    LinuxPlatformApi, LinuxSelectionRuntime, LinuxSettingsRuntime,
};

pub struct LinuxBackendRuntime {
    pub backend: Arc<OpenLessBackend>,
    pub host_actions: Arc<LinuxHostActions>,
    pub settings_runtime: Arc<dyn SettingsRuntime>,
}

/// Linux process adapter for the shared Core Less Computer runner.
/// Provider-specific policy is already normalized in `CodingAgentRequest`;
/// this adapter only owns process I/O and typed stream forwarding.
#[derive(Default)]
struct LinuxCodingAgentRuntime;

impl LessComputerRuntimeAdapter for LinuxCodingAgentRuntime {
    fn run(
        &self,
        request: CodingAgentRequest,
        events: tokio::sync::mpsc::UnboundedSender<openless_core::CodingAgentStreamEvent>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        Box::pin(async move {
            let session_id = request.session_id.clone();
            let _ = events.send(openless_core::CodingAgentStreamEvent::Started {
                session_id: session_id.clone(),
            });
            let result = tokio::task::spawn_blocking(move || {
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(BackendError::new(
                        BackendErrorCode::Cancelled,
                        "Less Computer cancelled",
                    ));
                }
                let executable = normalize_coding_agent_executable(
                    request.provider,
                    request.executable.clone(),
                )?;
                let mut command = std::process::Command::new(&executable);
                command.current_dir(
                    request
                        .cwd
                        .clone()
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
                );
                let args = match request.provider {
                    CodingAgentProvider::ClaudeCodeCli => {
                        openless_core::build_claude_args(&request)
                    }
                    _ => vec!["-p".to_string()],
                };
                command
                    .args(args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let mut child = command.spawn().map_err(|error| {
                    BackendError::new(
                        BackendErrorCode::Unsupported,
                        format!("failed to start {executable}: {error}"),
                    )
                })?;
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write;
                    stdin
                        .write_all(request.prompt.as_bytes())
                        .map_err(|error| {
                            BackendError::new(BackendErrorCode::Provider, error.to_string())
                        })?;
                }
                let output = child.wait_with_output().map_err(|error| {
                    BackendError::new(BackendErrorCode::Provider, error.to_string())
                })?;
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(BackendError::new(
                        BackendErrorCode::Cancelled,
                        "Less Computer cancelled",
                    ));
                }
                let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !text.is_empty() {
                    let _ = events.send(openless_core::CodingAgentStreamEvent::Delta {
                        session_id: session_id.clone(),
                        text: text.clone(),
                    });
                }
                if !output.status.success() {
                    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    let _ = events.send(openless_core::CodingAgentStreamEvent::Error {
                        session_id,
                        message: if message.is_empty() {
                            "coding agent exited with an error".into()
                        } else {
                            message
                        },
                    });
                    return Ok(());
                }
                let _ = events.send(openless_core::CodingAgentStreamEvent::Completed {
                    session_id,
                    text,
                    cost_usd: None,
                    duration_ms: None,
                });
                Ok(())
            })
            .await
            .map_err(|error| BackendError::new(BackendErrorCode::Internal, error.to_string()))?;
            result
        })
    }
}

struct LinuxCodingAgentApi {
    less_computer: std::sync::Arc<dyn openless_core::LessComputerApi>,
}

impl LinuxCodingAgentApi {
    fn new(less_computer: std::sync::Arc<dyn openless_core::LessComputerApi>) -> Self {
        Self { less_computer }
    }
}

impl CodingAgentApi for LinuxCodingAgentApi {
    fn detect(
        &self,
        request: CodingAgentDetectRequest,
    ) -> BoxFuture<'static, Result<CodingAgentAvailability, BackendError>> {
        let executable =
            match normalize_coding_agent_executable(request.provider, request.executable) {
                Ok(executable) => executable,
                Err(error) => return Box::pin(async move { Err(error) }),
            };
        Box::pin(async move {
            let installed = std::process::Command::new(&executable)
                .arg("--version")
                .output()
                .is_ok();
            Ok(CodingAgentAvailability {
                provider: request.provider,
                installed,
                executable,
                version: None,
                mcp_servers: Vec::new(),
                has_computer_use: false,
            })
        })
    }

    fn list_models(
        &self,
        _request: CodingAgentModelsRequest,
    ) -> BoxFuture<'static, Result<Vec<String>, BackendError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn command_risk(
        &self,
        _command: String,
    ) -> BoxFuture<'static, Result<CommandRiskAssessment, BackendError>> {
        Box::pin(async {
            Ok(CommandRiskAssessment {
                risk: CommandRisk::Unknown,
                reason: None,
            })
        })
    }

    fn run_test(
        &self,
        request: CodingAgentTestRequest,
    ) -> BoxFuture<'static, Result<CodingAgentTestStatus, BackendError>> {
        Box::pin(async move {
            let executable =
                normalize_coding_agent_executable(request.provider, request.executable)?;
            let installed = std::process::Command::new(&executable)
                .arg("--version")
                .output()
                .is_ok();
            Ok(CodingAgentTestStatus {
                running: false,
                request_id: None,
                message: Some(if installed {
                    format!("{executable} is available")
                } else {
                    format!("{executable} is not installed")
                }),
            })
        })
    }

    fn cancel_test(&self) -> BoxFuture<'static, Result<(), BackendError>> {
        Box::pin(async { Ok(()) })
    }

    fn approve(
        &self,
        token: String,
        approved: bool,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        self.less_computer.approve(token, approved)
    }
}

#[derive(Default)]
struct LinuxGenericAsrEngine;

#[derive(Default)]
struct LinuxGenericLocalAsrRuntime;

impl openless_core::LocalAsrRuntimeAdapter for LinuxGenericLocalAsrRuntime {
    fn engine_available(&self, runtime: openless_core::LocalAsrRuntime) -> bool {
        if runtime != openless_core::LocalAsrRuntime::Generic {
            return false;
        }
        match std::env::var_os("OPENLESS_QWEN_ASR_BIN") {
            Some(path) => {
                let path = std::path::Path::new(&path);
                if path.is_absolute() {
                    path.is_file()
                } else {
                    std::process::Command::new(path)
                        .arg("--version")
                        .output()
                        .is_ok()
                }
            }
            None => std::process::Command::new("qwen_asr")
                .arg("--version")
                .output()
                .is_ok(),
        }
    }

    fn storage_settings(
        &self,
        base_dir: Option<std::path::PathBuf>,
    ) -> BoxFuture<'static, Result<openless_core::LocalAsrStorageSettings, BackendError>> {
        Box::pin(async move {
            let is_default = base_dir.is_none();
            let root = base_dir.unwrap_or_else(|| std::env::temp_dir().join("openless-models"));
            Ok(openless_core::LocalAsrStorageSettings {
                models_base_dir: Some(root.clone()),
                models_root_dir: root,
                is_default,
            })
        })
    }

    fn list_models(
        &self,
        runtime: openless_core::LocalAsrRuntime,
    ) -> BoxFuture<'static, Result<Vec<openless_core::LocalAsrModel>, BackendError>> {
        Box::pin(async move {
            if runtime != openless_core::LocalAsrRuntime::Generic {
                return Ok(Vec::new());
            }
            Ok(["qwen3-asr-0.6b", "qwen3-asr-1.7b"]
                .into_iter()
                .filter_map(|model_id| {
                    openless_core::LocalAsrTarget::parse(runtime, model_id)
                        .ok()
                        .map(|target| openless_core::LocalAsrModel {
                            target,
                            display_name: model_id.to_string(),
                            family: "Qwen3-ASR".to_string(),
                            mode: Some("streaming".to_string()),
                            repository: None,
                            languages: Vec::new(),
                            installed: false,
                            downloaded_bytes: 0,
                            size_bytes: None,
                        })
                })
                .collect())
        })
    }

    fn runtime_status(
        &self,
        settings: openless_core::LocalAsrSettings,
    ) -> BoxFuture<'static, Result<openless_core::LocalAsrRuntimeStatus, BackendError>> {
        let available = self.engine_available(settings.runtime);
        Box::pin(async move {
            Ok(openless_core::LocalAsrRuntimeStatus {
                runtime: settings.runtime,
                provider_id: settings.provider_id,
                available,
                loaded: false,
                active_model: settings.active_model.clone(),
                model_id: Some(settings.active_model),
                keep_loaded_secs: settings.keep_loaded_secs,
                runtime_source: settings.runtime_source,
                endpoint: None,
                operation: None,
                error: (!available).then(|| "qwen_asr executable is not available".into()),
                last_error: None,
                last_prepare_ms: None,
                last_transcribe_ms: None,
                last_audio_ms: None,
            })
        })
    }

    fn model_dir(
        &self,
        target: openless_core::LocalAsrTarget,
    ) -> BoxFuture<'static, Result<std::path::PathBuf, BackendError>> {
        Box::pin(async move {
            Ok(std::env::temp_dir()
                .join("openless-models")
                .join(target.model_id()))
        })
    }
}

struct LinuxGenericAsrSession {
    model: Option<String>,
    pcm: std::sync::Mutex<Vec<u8>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TranscriptionEngine for LinuxGenericAsrEngine {
    fn start(
        &self,
        _session_id: openless_core::SessionId,
        context: std::sync::Arc<openless_core::DictationContext>,
        _partials: std::sync::Arc<dyn TextStreamSink>,
    ) -> BoxFuture<'static, Result<std::sync::Arc<dyn TranscriptionSession>, BackendError>> {
        let session: std::sync::Arc<dyn TranscriptionSession> =
            std::sync::Arc::new(LinuxGenericAsrSession {
                model: context.asr.model.clone(),
                pcm: std::sync::Mutex::new(Vec::new()),
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            });
        Box::pin(async move { Ok(session) })
    }
}

impl AudioConsumer for LinuxGenericAsrSession {
    fn consume_pcm_chunk(&self, pcm: &[u8]) {
        if !self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            self.pcm
                .lock()
                .expect("Linux generic ASR PCM lock poisoned")
                .extend_from_slice(pcm);
        }
    }
}

impl TranscriptionSession for LinuxGenericAsrSession {
    fn finish(&self) -> BoxFuture<'static, Result<TranscriptOutput, BackendError>> {
        let pcm = std::mem::take(
            &mut *self
                .pcm
                .lock()
                .expect("Linux generic ASR PCM lock poisoned"),
        );
        let model = self.model.clone();
        let cancelled = std::sync::Arc::clone(&self.cancelled);
        Box::pin(async move {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return Err(BackendError::new(
                    BackendErrorCode::Cancelled,
                    "ASR cancelled",
                ));
            }
            let duration_ms = (pcm.len() as u64).saturating_mul(1000) / 32_000;
            if pcm.is_empty() {
                return Ok(TranscriptOutput {
                    text: String::new(),
                    duration_ms,
                });
            }
            let result = tokio::task::spawn_blocking(move || {
                let executable = std::env::var("OPENLESS_QWEN_ASR_BIN")
                    .unwrap_or_else(|_| "qwen_asr".to_string());
                let mut command = std::process::Command::new(&executable);
                command.args(["--stdin", "--silent"]);
                if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
                    command.args(["-d", model.as_str()]);
                }
                let mut child = command
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|error| {
                        BackendError::new(
                            BackendErrorCode::Unsupported,
                            format!("Linux Generic/Qwen ASR runtime unavailable: {error}"),
                        )
                    })?;
                let wav = openless_core::encode_dictation_wav(&pcm)?;
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .ok_or_else(|| {
                        BackendError::new(BackendErrorCode::Internal, "ASR stdin unavailable")
                    })?
                    .write_all(&wav)
                    .map_err(|error| {
                        BackendError::new(BackendErrorCode::Provider, error.to_string())
                    })?;
                let output = child.wait_with_output().map_err(|error| {
                    BackendError::new(BackendErrorCode::Provider, error.to_string())
                })?;
                if !output.status.success() {
                    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    return Err(BackendError::new(
                        BackendErrorCode::Provider,
                        if message.is_empty() {
                            "Linux Generic/Qwen ASR failed".into()
                        } else {
                            message
                        },
                    ));
                }
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            })
            .await
            .map_err(|error| BackendError::new(BackendErrorCode::Internal, error.to_string()))??;
            Ok(TranscriptOutput {
                text: result,
                duration_ms,
            })
        })
    }

    fn cancel(&self) -> BoxFuture<'static, Result<(), BackendError>> {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.pcm
            .lock()
            .expect("Linux generic ASR PCM lock poisoned")
            .clear();
        Box::pin(async { Ok(()) })
    }
}

/// Assemble the non-UI Linux runtime from shared provider Interfaces.
///
/// The egui team only supplies a repaint callback and consumes the returned
/// backend/actions. Recorder, insertion, credentials and core pipeline
/// ownership remain outside the UI.
pub struct LinuxBackendBuilder {
    config: BackendConfig,
    transcription: Arc<dyn TranscriptionEngine>,
    polisher: Arc<dyn TextPolisher>,
    auxiliary_polisher: Option<Arc<dyn TextPolisher>>,
    recorder: Option<Arc<dyn AudioRecorder>>,
    text_inserter: Option<Arc<dyn TextInserter>>,
    credential_store: Option<Arc<dyn CredentialStore>>,
    marketplace_config: Option<MarketplaceConfig>,
    services: Option<BackendServices>,
    host_actions: Option<Arc<LinuxHostActions>>,
    settings_runtime: Option<Arc<dyn SettingsRuntime>>,
    local_asr_runtime: Option<Arc<dyn openless_core::LocalAsrRuntimeAdapter>>,
    polish_failure_policy: PolishFailurePolicy,
    task_spawner: Arc<dyn openless_core::TaskSpawner>,
}

impl LinuxBackendBuilder {
    /// Assemble the production Linux host with the cloud provider
    /// implementations and credential routing owned by shared core.
    ///
    /// The egui layer supplies only [`BackendConfig`]. It does not select
    /// protocol implementations or read credential accounts.
    pub fn from_shared_providers(config: BackendConfig) -> Result<Self, BackendError> {
        let store = LinuxCredentialStore::open(&config.data_dir)?;
        let credential_store: Arc<dyn CredentialStore> = Arc::new(store.clone());
        let task_spawner: Arc<dyn openless_core::TaskSpawner> = Arc::new(TokioTaskSpawner);

        let transcription = Arc::new(TranscriptionRouter::default());
        let cloud_transcription: Arc<dyn TranscriptionEngine> =
            Arc::new(SharedCloudTranscriptionEngine::with_task_spawner(
                Arc::clone(&credential_store),
                Arc::clone(&task_spawner),
            ));
        for provider_type in SHARED_CLOUD_ASR_PROVIDER_TYPES {
            transcription.register(*provider_type, Arc::clone(&cloud_transcription))?;
        }
        let linux_local_asr: Arc<dyn TranscriptionEngine> = Arc::new(LinuxGenericAsrEngine);
        for provider_id in ["local-qwen3", "local-qwen3-c"] {
            transcription.register(provider_id, Arc::clone(&linux_local_asr))?;
        }

        let polisher = Arc::new(TextPolisherRouter::default());
        let cloud_polisher: Arc<dyn TextPolisher> =
            Arc::new(SharedCloudTextPolisher::new(Arc::clone(&credential_store)));
        for provider_type in SHARED_CLOUD_LLM_PROVIDER_TYPES {
            polisher.register(*provider_type, Arc::clone(&cloud_polisher))?;
        }
        let polisher: Arc<dyn TextPolisher> = polisher;
        let auxiliary_polisher: Arc<dyn TextPolisher> = Arc::new(SharedAuxiliaryTextPolisher::new(
            Arc::clone(&credential_store),
            Arc::clone(&polisher),
        ));

        let mut services = BackendServices::unsupported();
        services.provider = Arc::new(ProviderService::new(
            Arc::clone(&credential_store),
            Arc::clone(&task_spawner),
        ));
        let coding_agent_runtime = Arc::new(LinuxCodingAgentRuntime);
        services
            .less_computer
            .bind_runtime(coding_agent_runtime.clone());
        services.coding_agent = Arc::new(LinuxCodingAgentApi::new(Arc::clone(
            &services.less_computer,
        )));

        Ok(Self::new(config, transcription, polisher)
            .with_task_spawner(task_spawner)
            .with_auxiliary_polisher(auxiliary_polisher)
            .with_credential_store(credential_store)
            .with_services(services)
            .with_marketplace_config(MarketplaceConfig::production())
            .with_settings_runtime(Arc::new(LinuxSettingsRuntime::new(store))))
    }

    /// Assemble a custom/test host with explicitly supplied provider engines.
    /// Production egui code should use [`Self::from_shared_providers`].
    pub fn new(
        config: BackendConfig,
        transcription: Arc<dyn TranscriptionEngine>,
        polisher: Arc<dyn TextPolisher>,
    ) -> Self {
        Self {
            config,
            transcription,
            polisher,
            auxiliary_polisher: None,
            recorder: None,
            text_inserter: None,
            credential_store: None,
            marketplace_config: None,
            services: None,
            host_actions: None,
            settings_runtime: None,
            local_asr_runtime: None,
            polish_failure_policy: PolishFailurePolicy::UseRawText,
            task_spawner: Arc::new(TokioTaskSpawner),
        }
    }

    fn with_task_spawner(mut self, task_spawner: Arc<dyn openless_core::TaskSpawner>) -> Self {
        self.task_spawner = task_spawner;
        self
    }

    pub fn with_recorder(mut self, recorder: Arc<dyn AudioRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    pub fn with_auxiliary_polisher(mut self, polisher: Arc<dyn TextPolisher>) -> Self {
        self.auxiliary_polisher = Some(polisher);
        self
    }

    pub fn with_text_inserter(mut self, inserter: Arc<dyn TextInserter>) -> Self {
        self.text_inserter = Some(inserter);
        self
    }

    pub fn with_credential_store(mut self, store: Arc<dyn CredentialStore>) -> Self {
        self.credential_store = Some(store);
        self
    }

    fn with_marketplace_config(mut self, config: MarketplaceConfig) -> Self {
        self.marketplace_config = Some(config);
        self
    }

    pub fn with_services(mut self, services: BackendServices) -> Self {
        self.services = Some(services);
        self
    }

    pub fn with_host_actions(mut self, actions: Arc<LinuxHostActions>) -> Self {
        self.host_actions = Some(actions);
        self
    }

    pub fn with_settings_runtime(mut self, runtime: Arc<dyn SettingsRuntime>) -> Self {
        self.settings_runtime = Some(runtime);
        self
    }

    pub fn with_local_asr_runtime(
        mut self,
        runtime: Arc<dyn openless_core::LocalAsrRuntimeAdapter>,
    ) -> Self {
        self.local_asr_runtime = Some(runtime);
        self
    }

    pub fn with_polish_failure_policy(mut self, policy: PolishFailurePolicy) -> Self {
        self.polish_failure_policy = policy;
        self
    }

    pub fn build(self) -> Result<LinuxBackendRuntime, BackendError> {
        let repositories = BackendRepositories::open(&self.config.data_dir)?;
        let recorder = self
            .recorder
            .unwrap_or_else(|| Arc::new(LinuxCpalRecorder::new(None)) as Arc<dyn AudioRecorder>);
        let text_inserter = self
            .text_inserter
            .unwrap_or_else(|| Arc::new(Fcitx5TextInserter::new(true)) as Arc<dyn TextInserter>);
        let (credential_store, default_settings_runtime): (
            Arc<dyn CredentialStore>,
            Arc<dyn SettingsRuntime>,
        ) = match self.credential_store {
            Some(store) => (store, Arc::new(LinuxSettingsRuntime::hotkeys_only())),
            None => {
                let store = LinuxCredentialStore::open(&self.config.data_dir)?;
                (
                    Arc::new(store.clone()),
                    Arc::new(LinuxSettingsRuntime::new(store)),
                )
            }
        };
        let settings_runtime = self.settings_runtime.unwrap_or(default_settings_runtime);
        let mut services = self.services.unwrap_or_else(BackendServices::unsupported);
        services.platform = Arc::new(LinuxPlatformApi::new(self.config.platform.clone()));
        let host_actions = self
            .host_actions
            .unwrap_or_else(|| Arc::new(LinuxHostActions::default()));
        let selection_polisher = Arc::clone(&self.polisher);
        let auxiliary_polisher = self
            .auxiliary_polisher
            .unwrap_or_else(|| Arc::clone(&self.polisher));
        services.configure_auxiliary_runtime(auxiliary_polisher, Arc::clone(&self.transcription));
        let traditional: Arc<dyn DictationEngine> = Arc::new(
            PipelineDictationEngine::new(Arc::clone(&recorder), self.transcription, self.polisher)
                .with_polish_failure_policy(self.polish_failure_policy),
        );
        let dictation_engine = Arc::new(DictationEngineRouter::new(traditional));
        let omni: Arc<dyn DictationEngine> = Arc::new(SharedOmniDictationEngine::new(
            Arc::clone(&credential_store),
            recorder,
        ));
        for provider_type in SHARED_OMNI_PROVIDER_TYPES {
            dictation_engine.register_omni(*provider_type, Arc::clone(&omni))?;
        }
        let backend = OpenLessBackend::new_with_repositories(
            self.config,
            BackendDependencies {
                host_actions: host_actions.clone(),
                text_inserter,
                dictation_engine,
                task_spawner: self.task_spawner,
                credential_store,
                services,
                local_asr_runtime: Some(
                    self.local_asr_runtime
                        .unwrap_or_else(|| Arc::new(LinuxGenericLocalAsrRuntime)),
                ),
                marketplace_config: self.marketplace_config,
                selection_runtime: Some(Arc::new(LinuxSelectionRuntime::new())),
                selection_polisher: Some(selection_polisher),
                qa_runtime: None,
            },
            repositories,
        )?;
        Ok(LinuxBackendRuntime {
            backend: Arc::new(backend),
            host_actions,
            settings_runtime,
        })
    }
}

#[cfg(test)]
mod tests {
    use openless_core::testing::{
        FixtureAudioRecorder, FixtureTextInserter, FixtureTextPolisher, FixtureTranscriptionEngine,
    };
    use openless_core::{
        BackendErrorCode, InMemoryCredentialStore, InsertOutcome, ProviderKind, ProviderRequest,
    };

    use super::*;

    #[test]
    fn shared_provider_builder_requires_no_ui_or_provider_factory() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-linux-shared-provider-builder-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let runtime = LinuxBackendBuilder::from_shared_providers(BackendConfig {
            data_dir: data_dir.clone(),
            ..BackendConfig::default()
        })
        .unwrap()
        .with_recorder(Arc::new(FixtureAudioRecorder::new(Vec::new(), Vec::new())))
        .with_text_inserter(Arc::new(FixtureTextInserter::with_outcome(
            InsertOutcome::Inserted,
        )))
        .build()
        .unwrap();

        assert!(!runtime.backend.snapshot().running);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn shared_provider_builder_registers_the_core_marketplace_service() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-linux-marketplace-builder-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let builder = LinuxBackendBuilder::from_shared_providers(BackendConfig {
            data_dir: data_dir.clone(),
            ..BackendConfig::default()
        })
        .unwrap();

        assert!(builder.marketplace_config.is_some());
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn shared_provider_factory_does_not_fall_back_to_unsupported() -> Result<(), BackendError>
    {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-linux-provider-factory-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let runtime = LinuxBackendBuilder::from_shared_providers(BackendConfig {
            data_dir: data_dir.clone(),
            ..BackendConfig::default()
        })?
        .build()?;

        let error = runtime
            .backend
            .services()
            .provider
            .list_models(ProviderRequest {
                kind: ProviderKind::Llm,
                channel_id: None,
            })
            .await
            .expect_err("an unconfigured provider should fail explicitly");
        assert_ne!(error.code, BackendErrorCode::Unsupported);
        let _ = std::fs::remove_dir_all(data_dir);
        Ok(())
    }

    #[tokio::test]
    async fn builder_runs_the_shared_pipeline_without_egui_or_tauri() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-linux-builder-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let recorder = FixtureAudioRecorder::new(vec![vec![1, 0, 2, 0]], vec![(20, 0.5)]);
        let transcription = FixtureTranscriptionEngine::successful("fixture raw", 20);
        let runtime = LinuxBackendBuilder::new(
            BackendConfig {
                data_dir: data_dir.clone(),
                ..BackendConfig::default()
            },
            Arc::new(transcription.clone()),
            Arc::new(FixtureTextPolisher::successful("fixture polished")),
        )
        .with_recorder(Arc::new(recorder.clone()))
        .with_text_inserter(Arc::new(FixtureTextInserter::with_outcome(
            InsertOutcome::Inserted,
        )))
        .with_credential_store(Arc::new(InMemoryCredentialStore::default()))
        .build()
        .unwrap();

        runtime.backend.start().await.unwrap();
        runtime.backend.start_dictation().await.unwrap();
        let result = runtime.backend.stop_dictation().await.unwrap();
        assert_eq!(result.raw_text, "fixture raw");
        assert_eq!(result.polished_text, "fixture polished");
        assert_eq!(transcription.pcm(), vec![1, 0, 2, 0]);
        assert_eq!(recorder.stop_count(), 1);
        runtime.backend.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(data_dir);
    }
}
