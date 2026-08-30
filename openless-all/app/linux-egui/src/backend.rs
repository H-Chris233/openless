use std::sync::Arc;

use openless_core::{
    AudioRecorder, BackendConfig, BackendDependencies, BackendError, BackendRepositories,
    BackendServices, CredentialStore, DictationEngine, DictationEngineRouter, MarketplaceConfig,
    OpenLessBackend, PipelineDictationEngine, PolishFailurePolicy, ProviderService,
    SettingsRuntime, SharedAuxiliaryTextPolisher, SharedCloudTextPolisher,
    SharedCloudTranscriptionEngine, SharedOmniDictationEngine, TextInserter, TextPolisher,
    TextPolisherRouter, TokioTaskSpawner, TranscriptionEngine, TranscriptionRouter,
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
                local_asr_runtime: None,
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
