use openless_core::{BackendError, BackendErrorCode, CliDispatchOutcome};

use crate::{
    Fcitx5HotkeyListener, LinuxBackendRuntime, LinuxHost, LinuxHostActions, SingleInstanceBroker,
};

#[derive(Debug, Default)]
pub struct LinuxRuntimePumpResult {
    pub launch_intents: usize,
    pub hotkey_events: usize,
    pub outcomes: Vec<CliDispatchOutcome>,
    pub errors: Vec<BackendError>,
}

/// Owns every non-UI Linux background resource that must stop before the
/// shared backend shuts down.
///
/// The egui team may keep this beside its app state and schedule `pump()` on
/// the host Tokio runtime. No egui/eframe type crosses this interface.
pub struct LinuxNativeRuntime {
    host: LinuxHost,
    host_actions: std::sync::Arc<LinuxHostActions>,
    broker: Option<SingleInstanceBroker>,
    hotkeys: Option<Fcitx5HotkeyListener>,
}

impl LinuxNativeRuntime {
    pub async fn start(
        backend: LinuxBackendRuntime,
        broker: Option<SingleInstanceBroker>,
        hotkeys: Option<Fcitx5HotkeyListener>,
    ) -> Result<Self, BackendError> {
        backend.backend.start().await?;
        Ok(Self {
            host: LinuxHost::with_settings_runtime(backend.backend, backend.settings_runtime),
            host_actions: backend.host_actions,
            broker,
            hotkeys,
        })
    }

    pub fn host(&self) -> &LinuxHost {
        &self.host
    }

    pub fn host_actions(&self) -> &std::sync::Arc<LinuxHostActions> {
        &self.host_actions
    }

    /// Drain currently queued native events without blocking on DBus or Unix
    /// sockets, then execute their shared core use-cases asynchronously.
    pub async fn pump(&self) -> LinuxRuntimePumpResult {
        let mut result = LinuxRuntimePumpResult::default();
        let mut launch_intents = Vec::new();
        if let Some(broker) = &self.broker {
            result.launch_intents = broker.drain(|intent| launch_intents.push(intent));
            if let Some(error) = broker.take_error() {
                result
                    .errors
                    .push(BackendError::new(BackendErrorCode::Platform, error));
            }
        }
        let mut hotkey_events = Vec::new();
        if let Some(hotkeys) = &self.hotkeys {
            result.hotkey_events = hotkeys.drain(|event| hotkey_events.push(event));
            if let Some(error) = hotkeys.take_error() {
                result.errors.push(error);
            }
        }

        for intent in launch_intents {
            match self.host.dispatch_launch_intent(intent).await {
                Ok(Some(outcome)) => result.outcomes.push(outcome),
                Ok(None) => {}
                Err(error) => result.errors.push(error),
            }
        }
        for event in hotkey_events {
            match self.host.dispatch_hotkey_event(event).await {
                Ok(Some(outcome)) => result.outcomes.push(outcome),
                Ok(None) => {}
                Err(error) => result.errors.push(error),
            }
        }
        result
    }

    /// Stop/join native listeners before asking the shared backend to cancel
    /// sessions and flush its lifecycle.
    pub async fn shutdown(mut self) -> Result<(), BackendError> {
        self.hotkeys.take();
        self.broker.take();
        self.host.backend().shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use openless_core::testing::{
        FixtureDictationEngine, FixtureTextInserter, RecordingHostActions,
    };
    use openless_core::{
        BackendConfig, BackendDependencies, InMemoryCredentialStore, InsertOutcome,
        OpenLessBackend, TokioTaskSpawner,
    };

    use super::*;

    #[tokio::test]
    async fn native_runtime_starts_pumps_and_shuts_down_without_ui() {
        let data_dir = std::env::temp_dir().join(format!(
            "openless-linux-native-runtime-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let host_actions = Arc::new(LinuxHostActions::default());
        let backend = Arc::new(
            OpenLessBackend::new(
                BackendConfig {
                    data_dir: data_dir.clone(),
                    ..BackendConfig::default()
                },
                BackendDependencies {
                    host_actions: Arc::new(RecordingHostActions::default()),
                    text_inserter: Arc::new(FixtureTextInserter::with_outcome(
                        InsertOutcome::Inserted,
                    )),
                    dictation_engine: Arc::new(FixtureDictationEngine::successful(
                        "raw", "polished",
                    )),
                    task_spawner: Arc::new(TokioTaskSpawner),
                    credential_store: Arc::new(InMemoryCredentialStore::default()),
                    services: openless_core::BackendServices::unsupported(),
                    local_asr_runtime: None,
                    marketplace_config: None,
                    selection_runtime: None,
                    selection_polisher: None,
                    qa_runtime: None,
                },
            )
            .unwrap(),
        );
        let runtime = LinuxNativeRuntime::start(
            LinuxBackendRuntime {
                backend: Arc::clone(&backend),
                host_actions,
                settings_runtime: Arc::new(openless_core::NoopSettingsRuntime),
            },
            None,
            None,
        )
        .await
        .unwrap();

        assert!(backend.snapshot().running);
        let pump = runtime.pump().await;
        assert_eq!(pump.launch_intents, 0);
        assert_eq!(pump.hotkey_events, 0);
        assert!(pump.outcomes.is_empty());
        assert!(pump.errors.is_empty());

        runtime.shutdown().await.unwrap();
        assert!(!backend.snapshot().running);
        let _ = std::fs::remove_dir_all(data_dir);
    }
}
