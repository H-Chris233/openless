//! Shared Local ASR orchestration.
//!
//! Model/runtime policy and preference transactions live here. Native engines,
//! download transports and model-file operations are supplied by a host Adapter.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;

use crate::domains::{
    LocalAsrApi, LocalAsrModel, LocalAsrModelCard, LocalAsrRemoteInfo, LocalAsrRuntimeStatus,
    LocalAsrSettings, LocalAsrStorageSettings, LocalAsrTestResult,
};
use crate::errors::{BackendError, BackendErrorCode};
use crate::events::{BackendEventKind, BackendEventPublisher};
use crate::local_asr_catalog::{
    normalize_foundry_language_hint, normalize_sherpa_language_hint, FoundryRuntimeSource,
    LocalAsrMirror, LocalAsrRuntime, LocalAsrTarget,
};
use crate::types::PreferencesChange;
use crate::{PreferencesStore, UserPreferences};

fn unsupported<T>(operation: &'static str) -> BoxFuture<'static, Result<T, BackendError>> {
    Box::pin(async move {
        Err(BackendError::new(
            BackendErrorCode::Unsupported,
            format!("local ASR runtime does not support {operation}"),
        ))
    })
}

/// Host seam for native Local ASR engines, downloads and model-file operations.
///
/// Defaults fail explicitly so a platform can implement only capabilities it
/// genuinely supports without reporting fake success.
pub trait LocalAsrRuntimeAdapter: Send + Sync {
    fn engine_available(&self, _runtime: LocalAsrRuntime) -> bool {
        false
    }

    fn storage_settings(
        &self,
        _base_dir: Option<PathBuf>,
    ) -> BoxFuture<'static, Result<LocalAsrStorageSettings, BackendError>> {
        unsupported("storage settings")
    }

    fn relocate_storage(
        &self,
        _current: Option<PathBuf>,
        _next: Option<PathBuf>,
    ) -> BoxFuture<'static, Result<LocalAsrStorageSettings, BackendError>> {
        unsupported("storage relocation")
    }

    fn list_models(
        &self,
        _runtime: LocalAsrRuntime,
    ) -> BoxFuture<'static, Result<Vec<LocalAsrModel>, BackendError>> {
        unsupported("model catalog")
    }

    fn runtime_status(
        &self,
        _settings: LocalAsrSettings,
    ) -> BoxFuture<'static, Result<LocalAsrRuntimeStatus, BackendError>> {
        unsupported("runtime status")
    }

    fn remote_info(
        &self,
        _target: LocalAsrTarget,
        _mirror: LocalAsrMirror,
    ) -> BoxFuture<'static, Result<LocalAsrRemoteInfo, BackendError>> {
        unsupported("remote model info")
    }

    fn model_card(
        &self,
        _target: LocalAsrTarget,
        _mirror: LocalAsrMirror,
    ) -> BoxFuture<'static, Result<LocalAsrModelCard, BackendError>> {
        unsupported("model card")
    }

    fn start_download(
        &self,
        _target: LocalAsrTarget,
        _mirror: LocalAsrMirror,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        unsupported("model download")
    }

    fn cancel_download(
        &self,
        _target: LocalAsrTarget,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        unsupported("download cancellation")
    }

    fn prepare(
        &self,
        _target: LocalAsrTarget,
        _runtime_source: FoundryRuntimeSource,
    ) -> BoxFuture<'static, Result<String, BackendError>> {
        unsupported("runtime preparation")
    }

    fn cancel_prepare(
        &self,
        _runtime: LocalAsrRuntime,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        unsupported("prepare cancellation")
    }

    fn release(&self, _runtime: LocalAsrRuntime) -> BoxFuture<'static, Result<(), BackendError>> {
        unsupported("runtime release")
    }

    fn preload(&self, _runtime: LocalAsrRuntime) -> BoxFuture<'static, Result<(), BackendError>> {
        unsupported("runtime preload")
    }

    fn delete_model(
        &self,
        _target: LocalAsrTarget,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        unsupported("model deletion")
    }

    fn model_dir(
        &self,
        _target: LocalAsrTarget,
    ) -> BoxFuture<'static, Result<PathBuf, BackendError>> {
        unsupported("model directory lookup")
    }

    fn test_model(
        &self,
        _target: LocalAsrTarget,
    ) -> BoxFuture<'static, Result<LocalAsrTestResult, BackendError>> {
        unsupported("model test")
    }

    fn invalidate_route(&self, _runtime: LocalAsrRuntime) {}
}

pub(crate) struct LocalAsrService {
    preferences: Arc<PreferencesStore>,
    runtime: Arc<dyn LocalAsrRuntimeAdapter>,
    events: BackendEventPublisher,
    preferences_revision: Arc<AtomicU64>,
}

impl LocalAsrService {
    pub(crate) fn new(
        preferences: Arc<PreferencesStore>,
        runtime: Arc<dyn LocalAsrRuntimeAdapter>,
        events: BackendEventPublisher,
        preferences_revision: Arc<AtomicU64>,
    ) -> Self {
        Self {
            preferences,
            runtime,
            events,
            preferences_revision,
        }
    }

    fn publish_preferences(&self, preferences: UserPreferences) -> Result<(), BackendError> {
        self.preferences.set(preferences)?;
        let revision = self.preferences_revision.fetch_add(1, Ordering::SeqCst) + 1;
        self.events.publish(
            None,
            BackendEventKind::PreferencesChanged(PreferencesChange { revision }),
        );
        Ok(())
    }

    fn active_model(preferences: &UserPreferences, runtime: LocalAsrRuntime) -> String {
        match runtime {
            LocalAsrRuntime::Generic => {
                if preferences.active_asr_provider == "local-whisper" {
                    preferences.local_whisper_active_model.clone()
                } else {
                    preferences.local_asr_active_model.clone()
                }
            }
            LocalAsrRuntime::Foundry => {
                LocalAsrTarget::parse(runtime, preferences.foundry_local_asr_model.clone())
                    .map(|target| target.model_id().to_string())
                    .unwrap_or_else(|_| runtime.default_model().to_string())
            }
            LocalAsrRuntime::SherpaOnnx => {
                LocalAsrTarget::parse(runtime, preferences.sherpa_onnx_model.clone())
                    .map(|target| target.model_id().to_string())
                    .unwrap_or_else(|_| runtime.default_model().to_string())
            }
        }
    }

    fn keep_loaded_secs(preferences: &UserPreferences, runtime: LocalAsrRuntime) -> u32 {
        match runtime {
            LocalAsrRuntime::Generic => preferences.local_asr_keep_loaded_secs,
            LocalAsrRuntime::Foundry => preferences.foundry_local_asr_keep_loaded_secs,
            LocalAsrRuntime::SherpaOnnx => preferences.sherpa_onnx_keep_loaded_secs,
        }
    }

    async fn runtime_status_snapshot(
        preferences: Arc<PreferencesStore>,
        adapter: Arc<dyn LocalAsrRuntimeAdapter>,
        runtime: LocalAsrRuntime,
    ) -> Result<LocalAsrRuntimeStatus, BackendError> {
        let preferences = preferences.get();
        let storage = adapter
            .storage_settings(Self::normalized_base_dir(Some(PathBuf::from(
                preferences.local_asr_models_base_dir.clone(),
            )))?)
            .await?;
        adapter
            .runtime_status(LocalAsrSettings {
                runtime,
                provider_id: runtime.provider_id().to_string(),
                active_model: Self::active_model(&preferences, runtime),
                mirror: LocalAsrMirror::from_legacy(&preferences.local_asr_mirror),
                models_base_dir: storage.models_base_dir,
                models_root_dir: storage.models_root_dir,
                engine_available: adapter.engine_available(runtime),
                language_hint: match runtime {
                    LocalAsrRuntime::Generic => None,
                    LocalAsrRuntime::Foundry => {
                        Some(preferences.foundry_local_asr_language_hint.clone())
                    }
                    LocalAsrRuntime::SherpaOnnx => {
                        Some(preferences.sherpa_onnx_language_hint.clone())
                    }
                },
                runtime_source: (runtime == LocalAsrRuntime::Foundry).then(|| {
                    FoundryRuntimeSource::from_legacy(&preferences.foundry_local_runtime_source)
                }),
                keep_loaded_secs: Self::keep_loaded_secs(&preferences, runtime),
            })
            .await
    }

    async fn publish_runtime_status(
        preferences: Arc<PreferencesStore>,
        adapter: Arc<dyn LocalAsrRuntimeAdapter>,
        events: BackendEventPublisher,
        runtime: LocalAsrRuntime,
    ) {
        if let Ok(status) = Self::runtime_status_snapshot(preferences, adapter, runtime).await {
            events.publish(None, BackendEventKind::LocalAsrEngineChanged(status));
        }
    }

    fn normalized_base_dir(path: Option<PathBuf>) -> Result<Option<PathBuf>, BackendError> {
        match path {
            Some(path) if path.as_os_str().is_empty() => Ok(None),
            Some(path) if !path.is_absolute() => Err(BackendError::new(
                BackendErrorCode::InvalidArgument,
                "local ASR model base directory must be absolute",
            )),
            path => Ok(path),
        }
    }
}

impl LocalAsrApi for LocalAsrService {
    fn settings(
        &self,
        runtime: LocalAsrRuntime,
    ) -> BoxFuture<'static, Result<LocalAsrSettings, BackendError>> {
        let preferences = self.preferences.get();
        let adapter = Arc::clone(&self.runtime);
        Box::pin(async move {
            let storage = adapter
                .storage_settings(Self::normalized_base_dir(Some(PathBuf::from(
                    preferences.local_asr_models_base_dir.clone(),
                )))?)
                .await?;
            Ok(LocalAsrSettings {
                runtime,
                provider_id: runtime.provider_id().to_string(),
                active_model: Self::active_model(&preferences, runtime),
                mirror: LocalAsrMirror::from_legacy(&preferences.local_asr_mirror),
                models_base_dir: storage.models_base_dir,
                models_root_dir: storage.models_root_dir,
                engine_available: adapter.engine_available(runtime),
                language_hint: match runtime {
                    LocalAsrRuntime::Generic => None,
                    LocalAsrRuntime::Foundry => {
                        Some(preferences.foundry_local_asr_language_hint.clone())
                    }
                    LocalAsrRuntime::SherpaOnnx => {
                        Some(preferences.sherpa_onnx_language_hint.clone())
                    }
                },
                runtime_source: (runtime == LocalAsrRuntime::Foundry).then(|| {
                    FoundryRuntimeSource::from_legacy(&preferences.foundry_local_runtime_source)
                }),
                keep_loaded_secs: Self::keep_loaded_secs(&preferences, runtime),
            })
        })
    }

    fn storage_settings(
        &self,
    ) -> BoxFuture<'static, Result<LocalAsrStorageSettings, BackendError>> {
        let adapter = Arc::clone(&self.runtime);
        let current = Self::normalized_base_dir(Some(PathBuf::from(
            self.preferences.get().local_asr_models_base_dir,
        )));
        Box::pin(async move { adapter.storage_settings(current?).await })
    }

    fn list_models(
        &self,
        runtime: LocalAsrRuntime,
    ) -> BoxFuture<'static, Result<Vec<LocalAsrModel>, BackendError>> {
        self.runtime.list_models(runtime)
    }

    fn runtime_status(
        &self,
        runtime: LocalAsrRuntime,
    ) -> BoxFuture<'static, Result<LocalAsrRuntimeStatus, BackendError>> {
        let preferences = Arc::clone(&self.preferences);
        let adapter = Arc::clone(&self.runtime);
        Box::pin(Self::runtime_status_snapshot(preferences, adapter, runtime))
    }

    fn remote_info(
        &self,
        target: LocalAsrTarget,
        mirror: Option<LocalAsrMirror>,
    ) -> BoxFuture<'static, Result<LocalAsrRemoteInfo, BackendError>> {
        let mirror = mirror.unwrap_or_else(|| {
            LocalAsrMirror::from_legacy(&self.preferences.get().local_asr_mirror)
        });
        self.runtime.remote_info(target, mirror)
    }

    fn model_card(
        &self,
        target: LocalAsrTarget,
        mirror: Option<LocalAsrMirror>,
    ) -> BoxFuture<'static, Result<LocalAsrModelCard, BackendError>> {
        let mirror = mirror.unwrap_or_else(|| {
            LocalAsrMirror::from_legacy(&self.preferences.get().local_asr_mirror)
        });
        self.runtime.model_card(target, mirror)
    }

    fn set_models_base_dir(
        &self,
        path: Option<PathBuf>,
    ) -> BoxFuture<'static, Result<LocalAsrStorageSettings, BackendError>> {
        let next = match Self::normalized_base_dir(path) {
            Ok(path) => path,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let current_preferences = self.preferences.get();
        let current = match Self::normalized_base_dir(Some(PathBuf::from(
            current_preferences.local_asr_models_base_dir.clone(),
        ))) {
            Ok(path) => path,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let adapter = Arc::clone(&self.runtime);
        let preferences = Arc::clone(&self.preferences);
        let events = self.events.clone();
        let revision = Arc::clone(&self.preferences_revision);
        Box::pin(async move {
            if current == next {
                return adapter.storage_settings(next).await;
            }
            let storage = adapter.relocate_storage(current, next.clone()).await?;
            let mut updated = current_preferences;
            updated.local_asr_models_base_dir = next
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            preferences.set(updated)?;
            let revision = revision.fetch_add(1, Ordering::SeqCst) + 1;
            events.publish(
                None,
                BackendEventKind::PreferencesChanged(PreferencesChange { revision }),
            );
            Ok(storage)
        })
    }

    fn set_active_model(
        &self,
        target: LocalAsrTarget,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let mut preferences = self.preferences.get();
        let runtime = target.runtime;
        match runtime {
            LocalAsrRuntime::Generic => {
                let model = crate::LocalAsrModelId::from_wire_id(target.model_id())
                    .expect("validated target");
                if model.is_whisper() {
                    preferences.local_whisper_active_model = target.model_id().to_string();
                } else {
                    preferences.local_asr_active_model = target.model_id().to_string();
                }
            }
            LocalAsrRuntime::Foundry => {
                preferences.foundry_local_asr_model = target.model_id().to_string();
            }
            LocalAsrRuntime::SherpaOnnx => {
                preferences.sherpa_onnx_model = target.model_id().to_string();
            }
        }
        let result = self.publish_preferences(preferences);
        if result.is_ok() {
            self.runtime.invalidate_route(runtime);
        }
        let preferences = Arc::clone(&self.preferences);
        let adapter = Arc::clone(&self.runtime);
        let events = self.events.clone();
        Box::pin(async move {
            result?;
            Self::publish_runtime_status(preferences, adapter, events, runtime).await;
            Ok(())
        })
    }

    fn set_mirror(&self, mirror: LocalAsrMirror) -> BoxFuture<'static, Result<(), BackendError>> {
        let mut preferences = self.preferences.get();
        preferences.local_asr_mirror = mirror.as_str().to_string();
        let result = self.publish_preferences(preferences);
        Box::pin(async move { result })
    }

    fn set_language_hint(
        &self,
        runtime: LocalAsrRuntime,
        language_hint: String,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let normalized = match runtime {
            LocalAsrRuntime::Foundry => normalize_foundry_language_hint(&language_hint),
            LocalAsrRuntime::SherpaOnnx => normalize_sherpa_language_hint(&language_hint),
            LocalAsrRuntime::Generic => Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "generic local ASR has no runtime language hint",
            )),
        };
        let normalized = match normalized {
            Ok(value) => value,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let mut preferences = self.preferences.get();
        match runtime {
            LocalAsrRuntime::Foundry => preferences.foundry_local_asr_language_hint = normalized,
            LocalAsrRuntime::SherpaOnnx => preferences.sherpa_onnx_language_hint = normalized,
            LocalAsrRuntime::Generic => unreachable!(),
        }
        let result = self.publish_preferences(preferences);
        Box::pin(async move { result })
    }

    fn set_foundry_runtime_source(
        &self,
        source: FoundryRuntimeSource,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let mut preferences = self.preferences.get();
        preferences.foundry_local_runtime_source = source.as_str().to_string();
        let result = self.publish_preferences(preferences);
        if result.is_ok() {
            self.runtime.invalidate_route(LocalAsrRuntime::Foundry);
        }
        let preferences = Arc::clone(&self.preferences);
        let adapter = Arc::clone(&self.runtime);
        let events = self.events.clone();
        Box::pin(async move {
            result?;
            Self::publish_runtime_status(preferences, adapter, events, LocalAsrRuntime::Foundry)
                .await;
            Ok(())
        })
    }

    fn set_keep_loaded_secs(
        &self,
        runtime: LocalAsrRuntime,
        seconds: u32,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let mut preferences = self.preferences.get();
        match runtime {
            LocalAsrRuntime::Generic => preferences.local_asr_keep_loaded_secs = seconds,
            LocalAsrRuntime::Foundry => preferences.foundry_local_asr_keep_loaded_secs = seconds,
            LocalAsrRuntime::SherpaOnnx => preferences.sherpa_onnx_keep_loaded_secs = seconds,
        }
        let result = self.publish_preferences(preferences);
        let preferences = Arc::clone(&self.preferences);
        let adapter = Arc::clone(&self.runtime);
        let events = self.events.clone();
        Box::pin(async move {
            result?;
            Self::publish_runtime_status(preferences, adapter, events, runtime).await;
            Ok(())
        })
    }

    fn start_download(
        &self,
        target: LocalAsrTarget,
        mirror: Option<LocalAsrMirror>,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let mirror = mirror.unwrap_or_else(|| {
            LocalAsrMirror::from_legacy(&self.preferences.get().local_asr_mirror)
        });
        self.runtime.start_download(target, mirror)
    }

    fn cancel_download(
        &self,
        target: LocalAsrTarget,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        self.runtime.cancel_download(target)
    }

    fn prepare(&self, target: LocalAsrTarget) -> BoxFuture<'static, Result<String, BackendError>> {
        let source =
            FoundryRuntimeSource::from_legacy(&self.preferences.get().foundry_local_runtime_source);
        let runtime = target.runtime;
        let adapter = Arc::clone(&self.runtime);
        let operation = adapter.prepare(target, source);
        let preferences = Arc::clone(&self.preferences);
        let events = self.events.clone();
        Box::pin(async move {
            let result = operation.await?;
            Self::publish_runtime_status(preferences, adapter, events, runtime).await;
            Ok(result)
        })
    }

    fn cancel_prepare(
        &self,
        runtime: LocalAsrRuntime,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        self.runtime.cancel_prepare(runtime)
    }

    fn release(&self, runtime: LocalAsrRuntime) -> BoxFuture<'static, Result<(), BackendError>> {
        let adapter = Arc::clone(&self.runtime);
        let operation = adapter.release(runtime);
        let preferences = Arc::clone(&self.preferences);
        let events = self.events.clone();
        Box::pin(async move {
            operation.await?;
            Self::publish_runtime_status(preferences, adapter, events, runtime).await;
            Ok(())
        })
    }

    fn preload(&self, runtime: LocalAsrRuntime) -> BoxFuture<'static, Result<(), BackendError>> {
        self.runtime.preload(runtime)
    }

    fn delete_model(&self, target: LocalAsrTarget) -> BoxFuture<'static, Result<(), BackendError>> {
        let runtime = target.runtime;
        let adapter = Arc::clone(&self.runtime);
        let operation = adapter.delete_model(target);
        let preferences = Arc::clone(&self.preferences);
        let events = self.events.clone();
        Box::pin(async move {
            operation.await?;
            Self::publish_runtime_status(preferences, adapter, events, runtime).await;
            Ok(())
        })
    }

    fn model_dir(
        &self,
        target: LocalAsrTarget,
    ) -> BoxFuture<'static, Result<PathBuf, BackendError>> {
        self.runtime.model_dir(target)
    }

    fn test_model(
        &self,
        target: LocalAsrTarget,
    ) -> BoxFuture<'static, Result<LocalAsrTestResult, BackendError>> {
        self.runtime.test_model(target)
    }
}
