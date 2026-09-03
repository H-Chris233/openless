use std::sync::Arc;

use futures_util::future::BoxFuture;

use openless_core::{
    AudioConsumer, AudioRecorder, BackendConfig, BackendDependencies, BackendError,
    BackendErrorCode, BackendRepositories, BackendServices, CredentialStore, DictationEngine,
    DictationEngineRouter, MarketplaceConfig, ModelStore, ModelStoreConfig, OpenLessBackend,
    PipelineDictationEngine, PolishFailurePolicy, ProviderService, SettingsRuntime,
    SharedAuxiliaryTextPolisher, SharedCloudTextPolisher, SharedCloudTranscriptionEngine,
    SharedOmniDictationEngine, TextInserter, TextPolisher, TextPolisherRouter, TextStreamSink,
    TokioTaskSpawner, TranscriptOutput, TranscriptionEngine, TranscriptionRouter,
    TranscriptionSession, SHARED_CLOUD_ASR_PROVIDER_TYPES, SHARED_CLOUD_LLM_PROVIDER_TYPES,
    SHARED_OMNI_PROVIDER_TYPES,
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

fn augment_linux_path(command: &mut std::process::Command) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let mut paths = vec![
        std::path::PathBuf::from(&home).join(".local/bin"),
        std::path::PathBuf::from(&home).join(".npm-global/bin"),
        std::path::PathBuf::from(&home).join(".bun/bin"),
    ];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    if let Ok(path) = std::env::join_paths(paths) {
        command.env("PATH", path);
    }
}

struct LinuxGenericAsrEngine {
    root: std::sync::Arc<std::sync::Mutex<std::path::PathBuf>>,
}

struct LinuxGenericLocalAsrRuntime {
    root: std::sync::Arc<std::sync::Mutex<std::path::PathBuf>>,
    loaded_model: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    // ponytail: one shared cancellation flag serializes model operations; per-model tokens if parallel downloads are needed.
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for LinuxGenericLocalAsrRuntime {
    fn default() -> Self {
        Self {
            root: std::sync::Arc::new(std::sync::Mutex::new(Self::default_root())),
            loaded_model: std::sync::Arc::new(std::sync::Mutex::new(None)),
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl LinuxGenericLocalAsrRuntime {
    const READY_SENTINEL: &'static str = openless_core::MODEL_READY_SENTINEL;

    fn default_root() -> std::path::PathBuf {
        if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
            return std::path::PathBuf::from(data)
                .join("OpenLess")
                .join("models");
        }
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(".local")
            .join("share")
            .join("OpenLess")
            .join("models")
    }

    fn from_models_root(root: std::path::PathBuf) -> Self {
        Self {
            root: std::sync::Arc::new(std::sync::Mutex::new(root)),
            loaded_model: std::sync::Arc::new(std::sync::Mutex::new(None)),
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn executable() -> String {
        std::env::var("OPENLESS_QWEN_ASR_BIN").unwrap_or_else(|_| "qwen_asr".into())
    }

    fn is_ready_dir(dir: &std::path::Path) -> bool {
        dir.join(Self::READY_SENTINEL).is_file()
    }

    fn ensure_qwen_target(target: &openless_core::LocalAsrTarget) -> Result<(), BackendError> {
        let supported = target.runtime == openless_core::LocalAsrRuntime::Generic
            && openless_core::LocalAsrModelId::from_wire_id(target.model_id())
                .is_some_and(openless_core::LocalAsrModelId::is_qwen);
        if supported {
            Ok(())
        } else {
            Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "Linux local ASR currently supports Qwen models only",
            ))
        }
    }
}

pub(crate) fn qwen_engine_available() -> bool {
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

impl openless_core::ModelRuntimeAdapter for LinuxGenericLocalAsrRuntime {
    fn engine_available(&self, runtime: openless_core::LocalAsrRuntime) -> bool {
        runtime == openless_core::LocalAsrRuntime::Generic && qwen_engine_available()
    }

    fn supports_model(&self, target: &openless_core::LocalAsrTarget) -> bool {
        Self::ensure_qwen_target(target).is_ok()
    }

    fn rebind_storage(
        &self,
        models_root: std::path::PathBuf,
    ) -> BoxFuture<'static, Result<openless_core::StorageRebind, BackendError>> {
        *self.root.lock().expect("Linux ASR root lock poisoned") = models_root;
        Box::pin(async { Ok(openless_core::StorageRebind::Applied) })
    }

    fn runtime_status(
        &self,
        settings: openless_core::LocalAsrSettings,
        _model_dir: std::path::PathBuf,
    ) -> BoxFuture<'static, Result<openless_core::LocalAsrRuntimeStatus, BackendError>> {
        let available = self.engine_available(settings.runtime);
        let loaded_model = std::sync::Arc::clone(&self.loaded_model);
        Box::pin(async move {
            let loaded = loaded_model
                .lock()
                .expect("Linux ASR loaded lock poisoned")
                .clone();
            Ok(openless_core::LocalAsrRuntimeStatus {
                runtime: settings.runtime,
                provider_id: settings.provider_id,
                available,
                loaded: loaded.is_some(),
                active_model: settings.active_model.clone(),
                model_id: loaded,
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

    fn prepare(
        &self,
        target: openless_core::LocalAsrTarget,
        _source: openless_core::FoundryRuntimeSource,
        dir: std::path::PathBuf,
        _progress: openless_core::ModelPrepareProgressSink,
    ) -> BoxFuture<'static, Result<String, BackendError>> {
        if let Err(error) = Self::ensure_qwen_target(&target) {
            return Box::pin(async move { Err(error) });
        }
        if let Some(root) = dir.parent() {
            *self.root.lock().expect("Linux ASR root lock poisoned") = root.to_path_buf();
        }
        let loaded = std::sync::Arc::clone(&self.loaded_model);
        let cancelled = std::sync::Arc::clone(&self.cancelled);
        Box::pin(async move {
            cancelled.store(false, std::sync::atomic::Ordering::Release);
            if !Self::is_ready_dir(&dir) {
                return Err(BackendError::new(
                    BackendErrorCode::InvalidState,
                    format!("local ASR model is not downloaded: {}", target.model_id()),
                ));
            }
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return Err(BackendError::new(
                    BackendErrorCode::Cancelled,
                    "local ASR prepare cancelled",
                ));
            }
            *loaded.lock().expect("Linux ASR loaded lock poisoned") =
                Some(target.model_id().to_string());
            Ok(target.model_id().to_string())
        })
    }

    fn release(
        &self,
        runtime: openless_core::LocalAsrRuntime,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let loaded = std::sync::Arc::clone(&self.loaded_model);
        Box::pin(async move {
            if runtime != openless_core::LocalAsrRuntime::Generic {
                return Ok(());
            }
            *loaded.lock().expect("Linux ASR loaded lock poisoned") = None;
            Ok(())
        })
    }

    fn preload(
        &self,
        target: openless_core::LocalAsrTarget,
        model_dir: std::path::PathBuf,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        if let Err(error) = Self::ensure_qwen_target(&target) {
            return Box::pin(async move { Err(error) });
        }
        if let Some(root) = model_dir.parent() {
            *self.root.lock().expect("Linux ASR root lock poisoned") = root.to_path_buf();
        }
        let loaded = std::sync::Arc::clone(&self.loaded_model);
        Box::pin(async move {
            if loaded
                .lock()
                .expect("Linux ASR loaded lock poisoned")
                .is_none()
            {
                return Err(BackendError::new(
                    BackendErrorCode::InvalidState,
                    "prepare a local ASR model before preloading",
                ));
            }
            Ok(())
        })
    }

    fn cancel_prepare(
        &self,
        runtime: openless_core::LocalAsrRuntime,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        if runtime != openless_core::LocalAsrRuntime::Generic {
            return Box::pin(async {
                Err(BackendError::new(
                    BackendErrorCode::Unsupported,
                    "Linux Generic/Qwen runtime only supports generic models",
                ))
            });
        }
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        Box::pin(async { Ok(()) })
    }

    fn test_model(
        &self,
        target: openless_core::LocalAsrTarget,
        dir: std::path::PathBuf,
    ) -> BoxFuture<'static, Result<openless_core::LocalAsrTestResult, BackendError>> {
        if let Err(error) = Self::ensure_qwen_target(&target) {
            return Box::pin(async move { Err(error) });
        }
        Box::pin(async move {
            if !Self::is_ready_dir(&dir) {
                return Err(BackendError::new(
                    BackendErrorCode::InvalidState,
                    "local ASR model is not downloaded",
                ));
            }
            let audio = std::env::var_os("OPENLESS_QWEN_ASR_TEST_AUDIO")
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_file())
                .ok_or_else(|| {
                    BackendError::new(
                        BackendErrorCode::Unsupported,
                        "set OPENLESS_QWEN_ASR_TEST_AUDIO to an audio fixture for model testing",
                    )
                })?;
            let executable = Self::executable();
            let started = std::time::Instant::now();
            let output = tokio::task::spawn_blocking({
                let executable = executable.clone();
                move || {
                    let mut command = std::process::Command::new(&executable);
                    augment_linux_path(&mut command);
                    command
                        .args(["-d", dir.to_string_lossy().as_ref()])
                        .arg(audio)
                        .output()
                        .map_err(|error| {
                            BackendError::new(BackendErrorCode::Unsupported, error.to_string())
                        })
                }
            })
            .await
            .map_err(|error| BackendError::new(BackendErrorCode::Internal, error.to_string()))??;
            if !output.status.success() {
                return Err(BackendError::new(
                    BackendErrorCode::Provider,
                    String::from_utf8_lossy(&output.stderr).trim().to_string(),
                ));
            }
            Ok(openless_core::LocalAsrTestResult {
                target,
                backend: executable,
                expected_text: std::env::var("OPENLESS_QWEN_ASR_TEST_EXPECTED").unwrap_or_default(),
                transcribed_text: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                audio_ms: 0,
                load_ms: 0,
                transcribe_ms: started.elapsed().as_millis() as u64,
            })
        })
    }
}

struct LinuxGenericAsrSession {
    model: Option<String>,
    root: std::sync::Arc<std::sync::Mutex<std::path::PathBuf>>,
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
                root: std::sync::Arc::clone(&self.root),
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
        let root = std::sync::Arc::clone(&self.root);
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
                augment_linux_path(&mut command);
                command.args(["--stdin", "--silent"]);
                if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
                    let model_dir = root
                        .lock()
                        .expect("Linux ASR root lock poisoned")
                        .join(&model);
                    if !LinuxGenericLocalAsrRuntime::is_ready_dir(&model_dir) {
                        return Err(BackendError::new(
                            BackendErrorCode::InvalidState,
                            format!("Linux local ASR model is not prepared: {model}"),
                        ));
                    }
                    command.args(["-d", model_dir.to_string_lossy().as_ref()]);
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
    local_asr_runtime: Option<Arc<dyn openless_core::ModelRuntimeAdapter>>,
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
        let configured_models_root =
            openless_core::PreferencesStore::open(config.data_dir.join("preferences.json"))
                .ok()
                .map(|preferences| preferences.get().local_asr_models_base_dir)
                .filter(|base| !base.trim().is_empty())
                .map(std::path::PathBuf::from)
                .filter(|base| base.is_absolute())
                .map(|base| base.join("OpenLess").join("models"))
                .unwrap_or_else(LinuxGenericLocalAsrRuntime::default_root);
        let linux_local_runtime = Arc::new(LinuxGenericLocalAsrRuntime::from_models_root(
            configured_models_root,
        ));
        let linux_local_asr: Arc<dyn TranscriptionEngine> = Arc::new(LinuxGenericAsrEngine {
            root: Arc::clone(&linux_local_runtime.root),
        });
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
        if let Ok(model_config) = ModelStoreConfig::new(
            linux_local_runtime
                .root
                .lock()
                .expect("Linux ASR root lock poisoned")
                .clone(),
        ) {
            if let Ok(model_store) = ModelStore::new(model_config) {
                services.configure_model_store(Arc::new(model_store));
            }
        }
        services.provider = Arc::new(ProviderService::new(
            Arc::clone(&credential_store),
            Arc::clone(&task_spawner),
        ));
        services.configure_coding_agent_process(Arc::new(
            crate::coding_agent::LinuxCodingAgentProcessAdapter,
        ));

        Ok(Self::new(config, transcription, polisher)
            .with_task_spawner(task_spawner)
            .with_auxiliary_polisher(auxiliary_polisher)
            .with_credential_store(credential_store)
            .with_services(services)
            .with_local_asr_runtime(linux_local_runtime)
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
        runtime: Arc<dyn openless_core::ModelRuntimeAdapter>,
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
                        .unwrap_or_else(|| Arc::new(LinuxGenericLocalAsrRuntime::default())),
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
        BackendErrorCode, CodingAgentProvider, CodingAgentTestRequest, InMemoryCredentialStore,
        InsertOutcome, ModelRuntimeAdapter, ProviderKind, ProviderRequest,
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

    #[tokio::test]
    async fn generic_local_asr_runtime_tracks_real_model_files_and_lifecycle() {
        let root = std::env::temp_dir().join(format!(
            "openless-linux-local-asr-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let runtime = LinuxGenericLocalAsrRuntime::default();
        let models_root = root.join("OpenLess").join("models");
        let store = openless_core::ModelStore::new(
            openless_core::ModelStoreConfig::new(models_root.clone()).unwrap(),
        )
        .unwrap();
        let target = openless_core::LocalAsrTarget::parse(
            openless_core::LocalAsrRuntime::Generic,
            "qwen3-asr-0.6b",
        )
        .unwrap();
        let model_dir = models_root.join(target.model_id());
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(
            model_dir.join(LinuxGenericLocalAsrRuntime::READY_SENTINEL),
            b"ready",
        )
        .unwrap();
        std::fs::write(model_dir.join("weights.bin"), [1_u8, 2, 3]).unwrap();

        let models = store
            .list_models(openless_core::LocalAsrRuntime::Generic)
            .unwrap();
        let model = models
            .iter()
            .find(|model| model.target.model_id() == target.model_id())
            .unwrap();
        assert!(model.installed);
        assert!(model.downloaded_bytes >= 3);
        assert_eq!(
            runtime
                .prepare(
                    target.clone(),
                    openless_core::FoundryRuntimeSource::Auto,
                    model_dir.clone(),
                    Arc::new(|_| {}),
                )
                .await
                .unwrap(),
            target.model_id()
        );
        assert!(
            runtime
                .runtime_status(
                    openless_core::LocalAsrSettings {
                        runtime: openless_core::LocalAsrRuntime::Generic,
                        provider_id: "local-qwen3".into(),
                        active_model: target.model_id().into(),
                        mirror: openless_core::LocalAsrMirror::Huggingface,
                        models_base_dir: Some(root.clone()),
                        models_root_dir: models_root.clone(),
                        engine_available: false,
                        language_hint: None,
                        runtime_source: None,
                        keep_loaded_secs: 0,
                    },
                    model_dir.clone()
                )
                .await
                .unwrap()
                .loaded
        );
        runtime
            .release(openless_core::LocalAsrRuntime::Generic)
            .await
            .unwrap();
        store.delete_model(&target).unwrap();
        assert!(!model_dir.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shared_builder_restores_custom_model_root_and_filters_unsupported_models() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-linux-custom-model-root-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let custom = data_dir.join("external");
        std::fs::create_dir_all(&data_dir).unwrap();
        let preferences =
            openless_core::PreferencesStore::open(data_dir.join("preferences.json")).unwrap();
        let mut value = preferences.get();
        value.local_asr_models_base_dir = custom.to_string_lossy().into_owned();
        preferences.set(value).unwrap();

        let runtime = LinuxBackendBuilder::from_shared_providers(BackendConfig {
            data_dir: data_dir.clone(),
            ..BackendConfig::default()
        })
        .unwrap()
        .build()
        .unwrap();
        let storage = runtime
            .backend
            .services()
            .local_asr
            .storage_settings()
            .await
            .unwrap();
        assert_eq!(
            storage.models_root_dir,
            custom.join("OpenLess").join("models")
        );
        let models = runtime
            .backend
            .services()
            .local_asr
            .list_models(openless_core::LocalAsrRuntime::Generic)
            .await
            .unwrap();
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|model| model.family == "qwen3"));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn coding_agent_runtime_reports_unavailable_cli_without_fake_success() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-linux-coding-agent-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let runtime = LinuxBackendBuilder::from_shared_providers(BackendConfig {
            data_dir: data_dir.clone(),
            ..BackendConfig::default()
        })
        .unwrap()
        .build()
        .unwrap();
        let error = runtime
            .backend
            .services()
            .coding_agent
            .run_test(CodingAgentTestRequest {
                provider: CodingAgentProvider::ClaudeCodeCli,
                executable: Some("openless-command-that-does-not-exist".into()),
                prompt: "test".into(),
                permission_mode: openless_core::CodingAgentPermissionMode::Plan,
                workdir: None,
                model: None,
                max_budget_usd: Some(0.5),
                timeout_secs: 5,
            })
            .await
            .expect_err("missing coding agent executable must be explicit");
        assert_eq!(error.code, BackendErrorCode::Unsupported);
        let _ = std::fs::remove_dir_all(data_dir);
    }
}
