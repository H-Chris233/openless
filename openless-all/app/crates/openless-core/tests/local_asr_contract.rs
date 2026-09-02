use futures_util::future::BoxFuture;
use openless_core::{
    normalize_foundry_language_hint, normalize_sherpa_language_hint, BackendConfig,
    BackendDependencies, BackendError, BackendErrorCode, BackendEventKind, FoundryRuntimeSource,
    LocalAsrMirror, LocalAsrRuntime, LocalAsrRuntimeStatus, LocalAsrSettings, LocalAsrTarget,
    ModelRuntimeAdapter, OpenLessBackend,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[test]
fn public_local_asr_catalog_rejects_unknown_models_per_runtime() {
    let qwen = LocalAsrTarget::parse(LocalAsrRuntime::Generic, "qwen3-asr-0.6b").unwrap();
    assert_eq!(qwen.model_id(), "qwen3-asr-0.6b");

    let foundry = LocalAsrTarget::parse(LocalAsrRuntime::Foundry, "whisper-small").unwrap();
    assert_eq!(foundry.model_id(), "whisper-small");

    let sherpa =
        LocalAsrTarget::parse(LocalAsrRuntime::SherpaOnnx, "sense-voice-small-zh").unwrap();
    assert_eq!(sherpa.model_id(), "sense-voice-small-zh");

    let error = LocalAsrTarget::parse(LocalAsrRuntime::SherpaOnnx, "whisper-small")
        .expect_err("a Foundry alias must not leak into the Sherpa catalog");
    assert_eq!(error.code, BackendErrorCode::InvalidArgument);
}

#[test]
fn public_local_asr_preferences_keep_legacy_normalization_semantics() {
    assert_eq!(
        LocalAsrMirror::from_legacy("hf-mirror"),
        LocalAsrMirror::HfMirror
    );
    assert_eq!(
        LocalAsrMirror::from_legacy("unexpected"),
        LocalAsrMirror::Huggingface
    );
    assert_eq!(
        FoundryRuntimeSource::from_legacy("ort-nightly"),
        FoundryRuntimeSource::OrtNightly
    );
    assert_eq!(
        FoundryRuntimeSource::from_legacy("unexpected"),
        FoundryRuntimeSource::Auto
    );

    assert_eq!(normalize_foundry_language_hint(" zh ").unwrap(), "zh");
    assert!(normalize_foundry_language_hint("ZH").is_err());
    assert_eq!(
        normalize_sherpa_language_hint(" ZH-hans ").unwrap(),
        "zh-hans"
    );
    assert!(normalize_sherpa_language_hint("zh_CN").is_err());
}

#[derive(Default)]
struct RecordingLocalAsrRuntime {
    invalidated: Mutex<Vec<LocalAsrRuntime>>,
    fail_release: std::sync::atomic::AtomicBool,
    status: Mutex<Option<LocalAsrRuntimeStatus>>,
}

impl ModelRuntimeAdapter for RecordingLocalAsrRuntime {
    fn engine_available(&self, _: LocalAsrRuntime) -> bool {
        true
    }

    fn runtime_status(
        &self,
        settings: LocalAsrSettings,
        _: PathBuf,
    ) -> BoxFuture<'static, Result<LocalAsrRuntimeStatus, BackendError>> {
        let mut status = self
            .status
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(LocalAsrRuntimeStatus {
                runtime: settings.runtime,
                provider_id: settings.provider_id,
                available: true,
                loaded: false,
                active_model: settings.active_model.clone(),
                model_id: None,
                keep_loaded_secs: settings.keep_loaded_secs,
                runtime_source: settings.runtime_source,
                endpoint: None,
                operation: None,
                error: None,
                last_error: None,
                last_prepare_ms: None,
                last_transcribe_ms: None,
                last_audio_ms: None,
            });
        status.active_model = settings.active_model;
        status.keep_loaded_secs = settings.keep_loaded_secs;
        status.runtime_source = settings.runtime_source;
        Box::pin(async move { Ok(status) })
    }

    fn prepare(
        &self,
        target: LocalAsrTarget,
        _: FoundryRuntimeSource,
        _: PathBuf,
    ) -> BoxFuture<'static, Result<String, BackendError>> {
        let model_id = target.model_id().to_string();
        *self.status.lock().unwrap() = Some(LocalAsrRuntimeStatus {
            runtime: target.runtime,
            provider_id: target.runtime.provider_id().to_string(),
            available: true,
            loaded: true,
            active_model: model_id.clone(),
            model_id: Some(model_id.clone()),
            keep_loaded_secs: 0,
            runtime_source: None,
            endpoint: None,
            operation: None,
            error: None,
            last_error: None,
            last_prepare_ms: Some(17),
            last_transcribe_ms: None,
            last_audio_ms: None,
        });
        Box::pin(async move { Ok(model_id) })
    }

    fn release(&self, runtime: LocalAsrRuntime) -> BoxFuture<'static, Result<(), BackendError>> {
        if self.fail_release.load(std::sync::atomic::Ordering::SeqCst) {
            return Box::pin(async {
                Err(BackendError::new(
                    BackendErrorCode::Platform,
                    "native runtime refused to release",
                ))
            });
        }
        if let Some(status) = self.status.lock().unwrap().as_mut() {
            status.runtime = runtime;
            status.loaded = false;
            status.model_id = None;
        }
        Box::pin(async { Ok(()) })
    }

    fn invalidate_route(&self, runtime: LocalAsrRuntime) {
        self.invalidated.lock().unwrap().push(runtime);
    }
}

fn local_asr_backend() -> (PathBuf, Arc<RecordingLocalAsrRuntime>, OpenLessBackend) {
    let data_dir = std::env::temp_dir().join(format!(
        "openless-core-local-asr-contract-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).unwrap();
    let runtime = Arc::new(RecordingLocalAsrRuntime::default());
    let mut dependencies = BackendDependencies::unsupported();
    dependencies.local_asr_runtime = Some(runtime.clone());
    let backend = OpenLessBackend::new(
        BackendConfig {
            data_dir: data_dir.clone(),
            ..BackendConfig::default()
        },
        dependencies,
    )
    .unwrap();
    (data_dir, runtime, backend)
}

#[tokio::test]
async fn backend_local_asr_service_owns_preferences_and_change_events() {
    let (data_dir, runtime, backend) = local_asr_backend();
    let mut events = backend.subscribe();
    let foundry = LocalAsrTarget::parse(LocalAsrRuntime::Foundry, "whisper-medium").unwrap();

    backend
        .services()
        .local_asr
        .set_active_model(foundry)
        .await
        .unwrap();
    backend
        .services()
        .local_asr
        .set_language_hint(LocalAsrRuntime::Foundry, " zh ".into())
        .await
        .unwrap();
    backend
        .services()
        .local_asr
        .set_language_hint(LocalAsrRuntime::SherpaOnnx, " ZH-hans ".into())
        .await
        .unwrap();
    backend
        .services()
        .local_asr
        .set_foundry_runtime_source(FoundryRuntimeSource::OrtNightly)
        .await
        .unwrap();
    backend
        .services()
        .local_asr
        .set_keep_loaded_secs(LocalAsrRuntime::Foundry, 42)
        .await
        .unwrap();

    let preferences = backend.get_preferences();
    assert_eq!(preferences.foundry_local_asr_model, "whisper-medium");
    assert_eq!(preferences.foundry_local_asr_language_hint, "zh");
    assert_eq!(preferences.sherpa_onnx_language_hint, "zh-hans");
    assert_eq!(preferences.foundry_local_runtime_source, "ort-nightly");
    assert_eq!(preferences.foundry_local_asr_keep_loaded_secs, 42);
    assert_eq!(
        runtime.invalidated.lock().unwrap().as_slice(),
        [LocalAsrRuntime::Foundry, LocalAsrRuntime::Foundry]
    );

    let event = events.try_recv().expect("preference mutation event");
    assert!(matches!(
        event.kind,
        BackendEventKind::PreferencesChanged(_)
    ));
    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn backend_local_asr_storage_change_commits_only_after_runtime_quiesces() {
    let (data_dir, runtime, backend) = local_asr_backend();
    let requested = data_dir.join("external-model-volume");
    std::fs::create_dir_all(&requested).unwrap();
    runtime
        .fail_release
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let error = backend
        .services()
        .local_asr
        .set_models_base_dir(Some(requested.clone()))
        .await
        .expect_err("a busy runtime must stop the preference commit");
    assert_eq!(error.code, BackendErrorCode::Platform);
    assert!(backend
        .get_preferences()
        .local_asr_models_base_dir
        .is_empty());

    runtime
        .fail_release
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let storage = backend
        .services()
        .local_asr
        .set_models_base_dir(Some(requested.clone()))
        .await
        .unwrap();
    assert_eq!(
        storage.models_base_dir.as_deref(),
        Some(requested.as_path())
    );
    assert_eq!(
        storage.models_root_dir,
        requested.join("OpenLess").join("models")
    );
    assert_eq!(
        backend.get_preferences().local_asr_models_base_dir,
        requested.to_string_lossy()
    );
    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn successful_runtime_mutations_publish_the_latest_engine_status() {
    let (data_dir, _, backend) = local_asr_backend();
    let mut events = backend.subscribe();

    backend
        .services()
        .local_asr
        .set_keep_loaded_secs(LocalAsrRuntime::Foundry, 42)
        .await
        .unwrap();
    assert!(matches!(
        events.try_recv().unwrap().kind,
        BackendEventKind::PreferencesChanged(_)
    ));
    let BackendEventKind::LocalAsrEngineChanged(status) = events.try_recv().unwrap().kind else {
        panic!("keep-loaded mutation must publish runtime status");
    };
    assert_eq!(status.runtime, LocalAsrRuntime::Foundry);
    assert_eq!(status.keep_loaded_secs, 42);
    assert!(!status.loaded);

    let target = LocalAsrTarget::parse(LocalAsrRuntime::Foundry, "whisper-small").unwrap();
    backend.services().local_asr.prepare(target).await.unwrap();
    let BackendEventKind::LocalAsrEngineChanged(status) = events.try_recv().unwrap().kind else {
        panic!("completed prepare must publish runtime status");
    };
    assert!(status.loaded);
    assert_eq!(status.model_id.as_deref(), Some("whisper-small"));

    backend
        .services()
        .local_asr
        .release(LocalAsrRuntime::Foundry)
        .await
        .unwrap();
    let BackendEventKind::LocalAsrEngineChanged(status) = events.try_recv().unwrap().kind else {
        panic!("completed release must publish runtime status");
    };
    assert!(!status.loaded);
    assert_eq!(status.model_id, None);

    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn failed_runtime_mutation_does_not_publish_a_success_status() {
    let (data_dir, runtime, backend) = local_asr_backend();
    let mut events = backend.subscribe();
    runtime
        .fail_release
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let error = backend
        .services()
        .local_asr
        .release(LocalAsrRuntime::Foundry)
        .await
        .expect_err("release failure must cross the public Interface");
    assert_eq!(error.code, BackendErrorCode::Platform);
    assert!(matches!(
        events.try_recv(),
        Err(openless_core::EventRecvError::Empty)
    ));

    let _ = std::fs::remove_dir_all(data_dir);
}
