use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use openless_core::{
    BackendConfig, BackendDependencies, BackendError, BackendErrorCode, BackendEventKind,
    OpenLessBackend, RemoteInputConfig, RemoteInputRuntimeAdapter, RemoteInputServerBinding,
    RemoteInputServerConfig, RemoteInputService, SecretValue, SessionId,
    REMOTE_INPUT_MAX_PCM_FRAME_BYTES,
};

#[derive(Default)]
struct FixtureRemoteRuntime {
    persisted_pin: Mutex<Option<String>>,
    persist_count: AtomicUsize,
    reject_persist: AtomicBool,
    start_count: AtomicUsize,
    stop_count: AtomicUsize,
    fail_start: AtomicBool,
    audio_start_count: AtomicUsize,
    audio_stop_count: AtomicUsize,
    audio_cancel_count: AtomicUsize,
    frames: Mutex<Vec<(SessionId, Vec<u8>)>>,
}

impl RemoteInputRuntimeAdapter for FixtureRemoteRuntime {
    fn load_pairing_pin(&self) -> BoxFuture<'static, Result<Option<SecretValue>, BackendError>> {
        let pin = self
            .persisted_pin
            .lock()
            .unwrap()
            .clone()
            .map(SecretValue::new);
        Box::pin(async move { Ok(pin) })
    }

    fn persist_pairing_pin(
        &self,
        pin: SecretValue,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        if self.reject_persist.load(Ordering::Acquire) {
            return Box::pin(async {
                Err(BackendError::new(
                    BackendErrorCode::Persistence,
                    "secret persistence details",
                ))
            });
        }
        self.persist_count.fetch_add(1, Ordering::AcqRel);
        *self.persisted_pin.lock().unwrap() = Some(pin.into_exposed());
        Box::pin(async { Ok(()) })
    }

    fn start_server(
        &self,
        config: RemoteInputServerConfig,
    ) -> BoxFuture<'static, Result<RemoteInputServerBinding, BackendError>> {
        self.start_count.fetch_add(1, Ordering::AcqRel);
        let pin_valid = config.pairing_pin.expose_secret().len() == 6;
        let fail = self.fail_start.load(Ordering::Acquire);
        Box::pin(async move {
            assert!(pin_valid);
            if fail {
                return Err(BackendError::new(BackendErrorCode::Platform, "port-in-use"));
            }
            Ok(RemoteInputServerBinding {
                port: config.port,
                urls: vec![format!("https://192.168.1.2:{}", config.port)],
            })
        })
    }

    fn stop_server(&self) -> BoxFuture<'static, Result<(), BackendError>> {
        self.stop_count.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Ok(()) })
    }

    fn list_local_ips(&self) -> BoxFuture<'static, Result<Vec<String>, BackendError>> {
        Box::pin(async { Ok(vec!["192.168.1.2".to_string()]) })
    }

    fn start_audio_session(&self) -> BoxFuture<'static, Result<SessionId, BackendError>> {
        self.audio_start_count.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Ok(SessionId::new()) })
    }

    fn feed_audio(
        &self,
        session_id: SessionId,
        pcm_s16le: Vec<u8>,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        self.frames.lock().unwrap().push((session_id, pcm_s16le));
        Box::pin(async { Ok(()) })
    }

    fn stop_audio_session(
        &self,
        _session_id: SessionId,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        self.audio_stop_count.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Ok(()) })
    }

    fn cancel_audio_session(
        &self,
        _session_id: SessionId,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        self.audio_cancel_count.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Ok(()) })
    }
}

fn backend(runtime: Arc<FixtureRemoteRuntime>) -> (OpenLessBackend, std::path::PathBuf) {
    let data_dir = std::env::temp_dir().join(format!(
        "openless-remote-input-contract-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let mut dependencies = BackendDependencies::unsupported();
    dependencies.services.remote_input =
        Arc::new(RemoteInputService::new(runtime, 8443, "zh-CN").expect("fixture config is valid"));
    let backend = OpenLessBackend::new(
        BackendConfig {
            data_dir: data_dir.clone(),
            ..BackendConfig::default()
        },
        dependencies,
    )
    .unwrap();
    (backend, data_dir)
}

#[tokio::test]
async fn pairing_pin_is_explicit_persisted_and_absent_from_public_surfaces() {
    let runtime = Arc::new(FixtureRemoteRuntime::default());
    let (backend, data_dir) = backend(Arc::clone(&runtime));
    let remote = &backend.services().remote_input;

    let pin = remote.read_pairing_pin().await.unwrap();
    assert_eq!(pin.expose_secret().len(), 6);
    assert!(pin
        .expose_secret()
        .bytes()
        .all(|byte| byte.is_ascii_digit()));
    assert_eq!(runtime.persist_count.load(Ordering::Acquire), 1);
    assert_eq!(
        remote.read_pairing_pin().await.unwrap().expose_secret(),
        pin.expose_secret()
    );
    assert_eq!(runtime.persist_count.load(Ordering::Acquire), 1);

    let json = format!(
        "{} {}",
        serde_json::to_string(&remote.status().unwrap()).unwrap(),
        serde_json::to_string(&backend.replay_events_after(0)).unwrap()
    )
    .to_ascii_lowercase();
    assert!(!json.contains(pin.expose_secret()));
    assert!(!json.contains("\"pin\""));
    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn enable_disable_and_port_change_are_idempotent_and_evented() {
    let runtime = Arc::new(FixtureRemoteRuntime::default());
    let (backend, data_dir) = backend(Arc::clone(&runtime));
    let remote = &backend.services().remote_input;

    remote
        .configure(RemoteInputConfig {
            enabled: true,
            port: 8443,
        })
        .await
        .unwrap();
    remote
        .configure(RemoteInputConfig {
            enabled: true,
            port: 8443,
        })
        .await
        .unwrap();
    assert_eq!(runtime.start_count.load(Ordering::Acquire), 1);
    assert!(remote.status().unwrap().running);

    remote
        .configure(RemoteInputConfig {
            enabled: true,
            port: 9443,
        })
        .await
        .unwrap();
    assert_eq!(runtime.start_count.load(Ordering::Acquire), 2);
    assert_eq!(runtime.stop_count.load(Ordering::Acquire), 1);
    assert_eq!(remote.status().unwrap().port, 9443);

    remote
        .configure(RemoteInputConfig {
            enabled: false,
            port: 9443,
        })
        .await
        .unwrap();
    remote
        .configure(RemoteInputConfig {
            enabled: false,
            port: 9443,
        })
        .await
        .unwrap();
    assert_eq!(runtime.stop_count.load(Ordering::Acquire), 2);
    assert!(!remote.status().unwrap().running);
    assert!(backend
        .replay_events_after(0)
        .events
        .iter()
        .any(|event| matches!(event.kind, BackendEventKind::RemoteInputStatusChanged(_))));
    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn port_conflict_is_classified_without_leaking_runtime_details() {
    let runtime = Arc::new(FixtureRemoteRuntime::default());
    runtime.fail_start.store(true, Ordering::Release);
    let (backend, data_dir) = backend(runtime);
    let error = backend
        .services()
        .remote_input
        .configure(RemoteInputConfig {
            enabled: true,
            port: 8443,
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, BackendErrorCode::Platform);
    assert_eq!(error.message, "port-in-use");
    assert!(backend
        .replay_events_after(0)
        .events
        .iter()
        .any(|event| matches!(
            event.kind,
            BackendEventKind::RemoteInputFailed(ref failure)
                if failure.reason == "port-in-use" && failure.port == 8443
        )));
    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn stream_association_validates_frames_and_rejects_duplicates_and_late_pcm() {
    let runtime = Arc::new(FixtureRemoteRuntime::default());
    let (backend, data_dir) = backend(Arc::clone(&runtime));
    let remote = &backend.services().remote_input;
    remote
        .configure(RemoteInputConfig {
            enabled: true,
            port: 8443,
        })
        .await
        .unwrap();
    let connection_id = SessionId::new();
    remote.connect(connection_id).await.unwrap();
    let session_id = remote.start_stream(connection_id).await.unwrap();
    assert_eq!(
        remote.start_stream(connection_id).await.unwrap_err().code,
        BackendErrorCode::Busy
    );
    assert_eq!(
        remote
            .feed_pcm(connection_id, session_id, vec![0])
            .await
            .unwrap_err()
            .code,
        BackendErrorCode::InvalidArgument
    );
    assert_eq!(
        remote
            .feed_pcm(
                connection_id,
                session_id,
                vec![0; REMOTE_INPUT_MAX_PCM_FRAME_BYTES + 2],
            )
            .await
            .unwrap_err()
            .code,
        BackendErrorCode::InvalidArgument
    );
    remote
        .feed_pcm(connection_id, session_id, vec![0, 1, 2, 3])
        .await
        .unwrap();
    remote.stop_stream(connection_id, session_id).await.unwrap();
    assert_eq!(runtime.audio_stop_count.load(Ordering::Acquire), 1);
    assert_eq!(runtime.frames.lock().unwrap().len(), 1);
    assert_eq!(
        remote
            .feed_pcm(connection_id, session_id, vec![0, 0])
            .await
            .unwrap_err()
            .code,
        BackendErrorCode::Cancelled
    );
    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn disconnect_and_pin_rotation_cancel_active_streams_before_transport_restart() {
    let runtime = Arc::new(FixtureRemoteRuntime::default());
    let (backend, data_dir) = backend(Arc::clone(&runtime));
    let remote = &backend.services().remote_input;
    remote
        .configure(RemoteInputConfig {
            enabled: true,
            port: 8443,
        })
        .await
        .unwrap();
    let first_connection = SessionId::new();
    remote.connect(first_connection).await.unwrap();
    remote.start_stream(first_connection).await.unwrap();
    remote.disconnect(first_connection).await.unwrap();
    remote.disconnect(first_connection).await.unwrap();
    assert_eq!(runtime.audio_cancel_count.load(Ordering::Acquire), 1);

    let second_connection = SessionId::new();
    remote.connect(second_connection).await.unwrap();
    remote.start_stream(second_connection).await.unwrap();
    remote.regenerate_pairing_pin().await.unwrap();
    assert_eq!(runtime.audio_cancel_count.load(Ordering::Acquire), 2);
    assert_eq!(runtime.start_count.load(Ordering::Acquire), 2);
    assert_eq!(runtime.stop_count.load(Ordering::Acquire), 1);
    assert_eq!(remote.status().unwrap().connection_count, 0);
    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn failed_pin_persistence_keeps_the_committed_pin_and_server_state() {
    let runtime = Arc::new(FixtureRemoteRuntime::default());
    let (backend, data_dir) = backend(Arc::clone(&runtime));
    let remote = &backend.services().remote_input;
    remote
        .configure(RemoteInputConfig {
            enabled: true,
            port: 8443,
        })
        .await
        .unwrap();
    let old_pin = remote.read_pairing_pin().await.unwrap().into_exposed();
    runtime.reject_persist.store(true, Ordering::Release);
    let error = remote.regenerate_pairing_pin().await.unwrap_err();
    assert_eq!(error.message, "remote input operation failed");
    assert_eq!(
        remote.read_pairing_pin().await.unwrap().expose_secret(),
        old_pin
    );
    assert!(remote.status().unwrap().running);
    assert_eq!(runtime.start_count.load(Ordering::Acquire), 1);
    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn locale_and_connection_status_are_core_owned() {
    let runtime = Arc::new(FixtureRemoteRuntime::default());
    let (backend, data_dir) = backend(runtime);
    let remote = &backend.services().remote_input;
    remote.set_locale("en".to_string()).await.unwrap();
    assert_eq!(remote.status().unwrap().locale, "en");
    assert_eq!(
        remote.set_locale("fr".to_string()).await.unwrap_err().code,
        BackendErrorCode::InvalidArgument
    );
    assert_eq!(
        remote
            .configure(RemoteInputConfig {
                enabled: true,
                port: 0,
            })
            .await
            .unwrap_err()
            .code,
        BackendErrorCode::InvalidArgument
    );
    assert_eq!(remote.list_local_ips().await.unwrap(), vec!["192.168.1.2"]);
    let _ = std::fs::remove_dir_all(data_dir);
}

#[tokio::test]
async fn backend_shutdown_stops_transport_and_cancels_active_remote_audio() {
    let runtime = Arc::new(FixtureRemoteRuntime::default());
    let (backend, data_dir) = backend(Arc::clone(&runtime));
    backend.start().await.unwrap();
    let remote = &backend.services().remote_input;
    remote
        .configure(RemoteInputConfig {
            enabled: true,
            port: 8443,
        })
        .await
        .unwrap();
    let connection_id = SessionId::new();
    remote.connect(connection_id).await.unwrap();
    remote.start_stream(connection_id).await.unwrap();

    backend.shutdown().await.unwrap();

    let status = remote.status().unwrap();
    assert!(!status.enabled);
    assert!(!status.running);
    assert_eq!(runtime.audio_cancel_count.load(Ordering::Acquire), 1);
    assert_eq!(runtime.stop_count.load(Ordering::Acquire), 1);
    let _ = std::fs::remove_dir_all(data_dir);
}
