use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use openless_core::{
    BackendError, BackendErrorCode, ChannelKind, ChannelMutation, ChannelMutationResult,
    ChannelSummary, CredentialKey, CredentialMetadata, CredentialNamespace, CredentialStore,
    CredentialsStatus, ProviderSlot, SecretValue, UserPreferences,
};
use serde::{Deserialize, Serialize};

const METADATA_VERSION: u32 = 1;
#[cfg(target_os = "linux")]
const KEYRING_SERVICE: &str = "top.openless.linux";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedCredentialMetadata {
    version: u32,
    #[serde(default)]
    metadata: CredentialMetadata,
    #[serde(default)]
    keys: Vec<CredentialKey>,
}

/// Linux credential adapter: secrets live in Secret Service/keyring, while
/// non-secret channel ordering and the list of configured keys live in the app
/// data directory.  Secret values are never serialized to the metadata file.
#[derive(Clone)]
pub struct LinuxCredentialStore {
    metadata_path: PathBuf,
    state: Arc<Mutex<PersistedCredentialMetadata>>,
}

impl LinuxCredentialStore {
    pub fn open(data_dir: &Path) -> Result<Self, BackendError> {
        if data_dir.as_os_str().is_empty() {
            return Err(BackendError::new(
                BackendErrorCode::InvalidArgument,
                "Linux credential metadata directory must not be empty",
            ));
        }
        let metadata_path = data_dir.join("credential-metadata.json");
        let state = read_metadata(&metadata_path)?;
        Ok(Self {
            metadata_path,
            state: Arc::new(Mutex::new(state)),
        })
    }

    fn update_metadata<T>(
        &self,
        update: impl FnOnce(&mut PersistedCredentialMetadata) -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        let mut state = self
            .state
            .lock()
            .expect("credential metadata lock poisoned");
        let mut next = state.clone();
        let result = update(&mut next)?;
        persist_metadata(&self.metadata_path, &next)?;
        *state = next;
        Ok(result)
    }

    pub(crate) fn set_active_provider_immediate(
        &self,
        slot: ProviderSlot,
        provider_id: &str,
    ) -> Result<(), BackendError> {
        let provider_id = provider_id.trim().to_string();
        if provider_id.is_empty() {
            return Err(BackendError::new(
                BackendErrorCode::InvalidArgument,
                "active provider id must not be blank",
            ));
        }
        self.update_metadata(|state| {
            state.metadata.set_active_provider(slot, provider_id);
            Ok(())
        })
    }
}

impl CredentialStore for LinuxCredentialStore {
    fn status(
        &self,
        preferences: UserPreferences,
    ) -> BoxFuture<'static, Result<CredentialsStatus, BackendError>> {
        let state = self
            .state
            .lock()
            .expect("credential metadata lock poisoned")
            .clone();
        Box::pin(async move {
            let active_asr_provider = non_empty(state.metadata.active_provider(ProviderSlot::Asr))
                .unwrap_or(preferences.active_asr_provider);
            let active_llm_provider = non_empty(state.metadata.active_provider(ProviderSlot::Llm))
                .unwrap_or(preferences.active_llm_provider);
            let has = |namespace, provider: &str| {
                state.keys.iter().any(|key| {
                    key.namespace == namespace
                        && key.provider_id.as_deref().is_none_or(|id| id == provider)
                })
            };
            Ok(CredentialsStatus {
                active_asr_provider: active_asr_provider.clone(),
                active_llm_provider: active_llm_provider.clone(),
                pipeline_mode: preferences.pipeline_mode,
                asr_configured: has(CredentialNamespace::Asr, &active_asr_provider),
                llm_configured: has(CredentialNamespace::Llm, &active_llm_provider),
                omni_configured: state
                    .keys
                    .iter()
                    .any(|key| key.namespace == CredentialNamespace::Omni),
                volcengine_configured: has(CredentialNamespace::Asr, "volcengine"),
                ark_configured: has(CredentialNamespace::Llm, "ark"),
            })
        })
    }

    fn read(
        &self,
        key: CredentialKey,
    ) -> BoxFuture<'static, Result<Option<SecretValue>, BackendError>> {
        Box::pin(async move {
            #[cfg(target_os = "linux")]
            {
                tokio::task::spawn_blocking(move || read_secret(&key))
                    .await
                    .map_err(join_error)?
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = key;
                unsupported_keyring()
            }
        })
    }

    fn write(
        &self,
        key: CredentialKey,
        value: SecretValue,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let store = self.clone();
        Box::pin(async move {
            #[cfg(target_os = "linux")]
            {
                tokio::task::spawn_blocking({
                    let key = key.clone();
                    move || write_secret(&key, &value)
                })
                .await
                .map_err(join_error)??;
                store.update_metadata(|state| {
                    if !state.keys.contains(&key) {
                        state.keys.push(key);
                    }
                    Ok(())
                })
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (store, key, value);
                unsupported_keyring()
            }
        })
    }

    fn remove(&self, key: CredentialKey) -> BoxFuture<'static, Result<(), BackendError>> {
        let store = self.clone();
        Box::pin(async move {
            #[cfg(target_os = "linux")]
            {
                tokio::task::spawn_blocking({
                    let key = key.clone();
                    move || remove_secret(&key)
                })
                .await
                .map_err(join_error)??;
                store.update_metadata(|state| {
                    state.keys.retain(|candidate| candidate != &key);
                    Ok(())
                })
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (store, key);
                unsupported_keyring()
            }
        })
    }

    fn list_channels(
        &self,
        kind: ChannelKind,
    ) -> BoxFuture<'static, Result<Vec<ChannelSummary>, BackendError>> {
        let channels = self
            .state
            .lock()
            .expect("credential metadata lock poisoned")
            .metadata
            .list_channels(kind);
        Box::pin(async move { Ok(channels) })
    }

    fn mutate_channel(
        &self,
        mutation: ChannelMutation,
    ) -> BoxFuture<'static, Result<ChannelMutationResult, BackendError>> {
        let store = self.clone();
        Box::pin(async move {
            store.update_metadata(|state| {
                let keys = &state.keys;
                state.metadata.apply_channel_mutation(mutation, |id| {
                    keys.iter()
                        .any(|key| key.provider_id.as_deref() == Some(id))
                })
            })
        })
    }

    fn active_provider(
        &self,
        slot: ProviderSlot,
    ) -> BoxFuture<'static, Result<String, BackendError>> {
        let provider = self
            .state
            .lock()
            .expect("credential metadata lock poisoned")
            .metadata
            .active_provider(slot);
        Box::pin(async move { Ok(provider) })
    }

    fn set_active_provider(
        &self,
        slot: ProviderSlot,
        provider_id: String,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        let store = self.clone();
        Box::pin(async move { store.set_active_provider_immediate(slot, &provider_id) })
    }
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn read_metadata(path: &Path) -> Result<PersistedCredentialMetadata, BackendError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut state: PersistedCredentialMetadata =
                serde_json::from_slice(&bytes).map_err(|error| {
                    BackendError::new(
                        BackendErrorCode::Persistence,
                        format!("invalid Linux credential metadata: {error}"),
                    )
                })?;
            if state.version > METADATA_VERSION {
                return Err(BackendError::new(
                    BackendErrorCode::Persistence,
                    "Linux credential metadata is newer than this application",
                ));
            }
            state.version = METADATA_VERSION;
            Ok(state)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PersistedCredentialMetadata {
                version: METADATA_VERSION,
                ..PersistedCredentialMetadata::default()
            })
        }
        Err(error) => Err(BackendError::new(
            BackendErrorCode::Persistence,
            format!("failed to read Linux credential metadata: {error}"),
        )),
    }
}

fn persist_metadata(path: &Path, state: &PersistedCredentialMetadata) -> Result<(), BackendError> {
    let parent = path.parent().ok_or_else(|| {
        BackendError::new(
            BackendErrorCode::Persistence,
            "Linux credential metadata has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        BackendError::new(
            BackendErrorCode::Persistence,
            format!("failed to create Linux credential directory: {error}"),
        )
    })?;
    let bytes = serde_json::to_vec_pretty(state).map_err(|error| {
        BackendError::new(
            BackendErrorCode::Persistence,
            format!("failed to serialize Linux credential metadata: {error}"),
        )
    })?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temporary, bytes).map_err(|error| {
        BackendError::new(
            BackendErrorCode::Persistence,
            format!("failed to stage Linux credential metadata: {error}"),
        )
    })?;
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path).map_err(|error| {
            BackendError::new(
                BackendErrorCode::Persistence,
                format!("failed to replace test credential metadata: {error}"),
            )
        })?;
    }
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        BackendError::new(
            BackendErrorCode::Persistence,
            format!("failed to commit Linux credential metadata: {error}"),
        )
    })
}

#[cfg(target_os = "linux")]
fn keyring_entry(key: &CredentialKey) -> Result<keyring::Entry, BackendError> {
    let account = serde_json::to_string(key).map_err(|error| {
        BackendError::new(
            BackendErrorCode::Internal,
            format!("failed to encode credential key: {error}"),
        )
    })?;
    keyring::Entry::new(KEYRING_SERVICE, &account).map_err(keyring_error)
}

#[cfg(target_os = "linux")]
fn read_secret(key: &CredentialKey) -> Result<Option<SecretValue>, BackendError> {
    match keyring_entry(key)?.get_password() {
        Ok(value) => Ok(Some(SecretValue::new(value))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(keyring_error(error)),
    }
}

#[cfg(target_os = "linux")]
fn write_secret(key: &CredentialKey, value: &SecretValue) -> Result<(), BackendError> {
    keyring_entry(key)?
        .set_password(value.expose_secret())
        .map_err(keyring_error)
}

#[cfg(target_os = "linux")]
fn remove_secret(key: &CredentialKey) -> Result<(), BackendError> {
    match keyring_entry(key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(keyring_error(error)),
    }
}

#[cfg(target_os = "linux")]
fn keyring_error(error: keyring::Error) -> BackendError {
    BackendError::new(
        BackendErrorCode::Persistence,
        format!("Linux credential vault operation failed: {error}"),
    )
}

#[cfg(target_os = "linux")]
fn join_error(error: tokio::task::JoinError) -> BackendError {
    BackendError::new(
        BackendErrorCode::Internal,
        format!("Linux credential task failed: {error}"),
    )
}

#[cfg(not(target_os = "linux"))]
fn unsupported_keyring<T>() -> Result<T, BackendError> {
    Err(BackendError::new(
        BackendErrorCode::Unsupported,
        "Linux Secret Service credential adapter is unavailable on this target",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn metadata_round_trips_without_secret_values() {
        let root = std::env::temp_dir().join(format!(
            "openless-linux-credential-metadata-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = LinuxCredentialStore::open(&root).unwrap();
        store
            .set_active_provider(ProviderSlot::Asr, "local-qwen".into())
            .await
            .unwrap();
        store
            .mutate_channel(ChannelMutation::Create {
                kind: ChannelKind::Asr,
                provider_type: "openai-compatible".into(),
                name: "Primary".into(),
            })
            .await
            .unwrap();

        let reopened = LinuxCredentialStore::open(&root).unwrap();
        assert_eq!(
            reopened.active_provider(ProviderSlot::Asr).await.unwrap(),
            "local-qwen"
        );
        assert_eq!(
            reopened
                .list_channels(ChannelKind::Asr)
                .await
                .unwrap()
                .len(),
            1
        );
        let persisted = std::fs::read_to_string(root.join("credential-metadata.json")).unwrap();
        assert!(!persisted.contains("secret"));
        assert!(!persisted.contains("password"));
        let _ = std::fs::remove_dir_all(root);
    }
}
