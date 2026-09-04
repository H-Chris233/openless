use std::collections::HashMap;
use std::fmt;
use std::sync::RwLock;

use futures_util::future::BoxFuture;

use crate::errors::{BackendError, BackendErrorCode};
use crate::shared_types::{CredentialsStatus, UserPreferences};

pub const ASR_API_KEY_ACCOUNT: &str = "asr.api_key";
pub const ASR_ENDPOINT_ACCOUNT: &str = "asr.endpoint";
pub const ASR_MODEL_ACCOUNT: &str = "asr.model";
pub const ASR_VOCABULARY_ID_ACCOUNT: &str = "asr.vocabulary_id";
pub const ASR_ADVANCED_CONFIG_ACCOUNT: &str = "asr.advanced_config";
pub const VOLCENGINE_APP_KEY_ACCOUNT: &str = "volcengine.app_key";
pub const VOLCENGINE_ACCESS_KEY_ACCOUNT: &str = "volcengine.access_key";
pub const VOLCENGINE_RESOURCE_ID_ACCOUNT: &str = "volcengine.resource_id";
pub const VOLCENGINE_AUTH_MODE_ACCOUNT: &str = "volcengine.auth_mode";
pub const VOLCENGINE_API_KEY_ACCOUNT: &str = "volcengine.api_key";
pub const XFYUN_APP_ID_ACCOUNT: &str = "xfyun.app_id";
pub const XFYUN_API_KEY_ACCOUNT: &str = "xfyun.api_key";
pub const LLM_API_KEY_ACCOUNT: &str = "ark.api_key";
pub const LLM_MODEL_ACCOUNT: &str = "ark.model_id";
pub const LLM_ENDPOINT_ACCOUNT: &str = "ark.endpoint";
pub const LLM_EXTRA_HEADERS_ACCOUNT: &str = "ark.extra_headers";
pub const LLM_TEMPERATURE_ACCOUNT: &str = "ark.temperature";
pub const OMNI_API_KEY_ACCOUNT: &str = "omni.api_key";
pub const OMNI_ENDPOINT_ACCOUNT: &str = "omni.endpoint";
pub const OMNI_MODEL_ACCOUNT: &str = "omni.model";
pub const OMNI_EXTRA_HEADERS_ACCOUNT: &str = "omni.extra_headers";
pub const OMNI_TEMPERATURE_ACCOUNT: &str = "omni.temperature";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Asr,
    Llm,
}

impl ChannelKind {
    pub fn parse(value: &str) -> Result<Self, BackendError> {
        match value {
            "asr" => Ok(Self::Asr),
            "llm" => Ok(Self::Llm),
            other => Err(BackendError::new(
                BackendErrorCode::InvalidArgument,
                format!("unknown channel kind: {other}"),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSlot {
    Asr,
    Llm,
    Omni,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSummary {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub enabled: bool,
    pub order: u32,
    pub last_test: Option<ChannelTestSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelTestSummary {
    pub ok: bool,
    pub latency_ms: Option<u32>,
    pub at: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelMutation {
    Create {
        kind: ChannelKind,
        provider_type: String,
        name: String,
    },
    SetProviderType {
        kind: ChannelKind,
        id: String,
        provider_type: String,
    },
    DeleteIfBlank {
        kind: ChannelKind,
        id: String,
    },
    Rename {
        kind: ChannelKind,
        id: String,
        name: String,
    },
    Delete {
        kind: ChannelKind,
        id: String,
    },
    SetEnabled {
        kind: ChannelKind,
        id: String,
        enabled: bool,
    },
    Reorder {
        kind: ChannelKind,
        ids: Vec<String>,
    },
    RecordTest {
        kind: ChannelKind,
        id: String,
        ok: bool,
        latency_ms: Option<u32>,
        at: i64,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelMutationResult {
    Applied,
    Created(String),
    DeletedIfBlank(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialNamespace {
    Asr,
    Llm,
    Omni,
    Marketplace,
    Application,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialKey {
    pub namespace: CredentialNamespace,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    pub account: String,
}

impl CredentialKey {
    pub fn new(
        namespace: CredentialNamespace,
        provider_id: Option<String>,
        account: impl Into<String>,
    ) -> Result<Self, BackendError> {
        let account = account.into();
        if account.trim().is_empty()
            || provider_id
                .as_deref()
                .is_some_and(|provider| provider.trim().is_empty())
        {
            return Err(BackendError::new(
                BackendErrorCode::InvalidArgument,
                "credential account and provider id must not be blank",
            ));
        }
        Ok(Self {
            namespace,
            provider_id,
            account,
        })
    }
}

/// Secret value with deliberately redacted diagnostics and no serde support.
///
/// Hosts may expose the value only from an explicitly authorised settings
/// surface. Core snapshots, events and errors can never serialize this type.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn into_exposed(self) -> String {
        self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

pub trait CredentialStore: Send + Sync {
    fn status(
        &self,
        preferences: UserPreferences,
    ) -> BoxFuture<'static, Result<CredentialsStatus, BackendError>>;

    fn read(
        &self,
        key: CredentialKey,
    ) -> BoxFuture<'static, Result<Option<SecretValue>, BackendError>>;

    fn write(
        &self,
        key: CredentialKey,
        value: SecretValue,
    ) -> BoxFuture<'static, Result<(), BackendError>>;

    fn remove(&self, key: CredentialKey) -> BoxFuture<'static, Result<(), BackendError>>;

    fn list_channels(
        &self,
        _kind: ChannelKind,
    ) -> BoxFuture<'static, Result<Vec<ChannelSummary>, BackendError>> {
        unsupported_credentials()
    }

    fn mutate_channel(
        &self,
        _mutation: ChannelMutation,
    ) -> BoxFuture<'static, Result<ChannelMutationResult, BackendError>> {
        unsupported_credentials()
    }

    fn active_provider(
        &self,
        _slot: ProviderSlot,
    ) -> BoxFuture<'static, Result<String, BackendError>> {
        unsupported_credentials()
    }

    fn set_active_provider(
        &self,
        _slot: ProviderSlot,
        _provider_id: String,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        unsupported_credentials()
    }
}

/// Non-secret provider/channel metadata that can be persisted by any host.
/// Secret values remain in the platform credential vault and are represented
/// here only through the `has_credentials` callback used for blank cleanup.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialMetadata {
    #[serde(default)]
    channels: HashMap<ChannelKind, Vec<ChannelSummary>>,
    #[serde(default)]
    active_providers: HashMap<ProviderSlot, String>,
}

impl CredentialMetadata {
    pub fn list_channels(&self, kind: ChannelKind) -> Vec<ChannelSummary> {
        self.channels.get(&kind).cloned().unwrap_or_default()
    }

    pub fn active_provider(&self, slot: ProviderSlot) -> String {
        self.active_providers
            .get(&slot)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_active_provider(&mut self, slot: ProviderSlot, provider_id: String) {
        self.active_providers.insert(slot, provider_id);
    }

    pub fn apply_channel_mutation(
        &mut self,
        mutation: ChannelMutation,
        has_credentials: impl Fn(&str) -> bool,
    ) -> Result<ChannelMutationResult, BackendError> {
        let (kind, result) = match mutation {
            ChannelMutation::Create {
                kind,
                provider_type,
                name,
            } => {
                let provider_type = provider_type.trim();
                if provider_type.is_empty() {
                    return Err(BackendError::new(
                        BackendErrorCode::InvalidArgument,
                        "provider type must not be blank",
                    ));
                }
                let channels = self.channels.entry(kind).or_default();
                let id = uuid::Uuid::new_v4().to_string();
                channels.push(ChannelSummary {
                    id: id.clone(),
                    name: name.trim().to_string(),
                    provider_type: provider_type.to_string(),
                    enabled: true,
                    order: channels.len() as u32,
                    last_test: None,
                });
                (kind, ChannelMutationResult::Created(id))
            }
            ChannelMutation::SetProviderType {
                kind,
                id,
                provider_type,
            } => {
                let provider_type = provider_type.trim();
                if provider_type.is_empty() {
                    return Err(BackendError::new(
                        BackendErrorCode::InvalidArgument,
                        "provider type must not be blank",
                    ));
                }
                let channel = find_channel_mut(&mut self.channels, kind, &id)?;
                channel.provider_type = provider_type.to_string();
                channel.last_test = None;
                (kind, ChannelMutationResult::Applied)
            }
            ChannelMutation::DeleteIfBlank { kind, id } => {
                let channels = self.channels.entry(kind).or_default();
                let before = channels.len();
                channels.retain(|channel| {
                    channel.id != id
                        || !channel.name.trim().is_empty()
                        || has_credentials(&channel.id)
                });
                (
                    kind,
                    ChannelMutationResult::DeletedIfBlank(channels.len() != before),
                )
            }
            ChannelMutation::Rename { kind, id, name } => {
                find_channel_mut(&mut self.channels, kind, &id)?.name = name.trim().to_string();
                (kind, ChannelMutationResult::Applied)
            }
            ChannelMutation::Delete { kind, id } => {
                self.channels
                    .entry(kind)
                    .or_default()
                    .retain(|channel| channel.id != id);
                (kind, ChannelMutationResult::Applied)
            }
            ChannelMutation::SetEnabled { kind, id, enabled } => {
                find_channel_mut(&mut self.channels, kind, &id)?.enabled = enabled;
                (kind, ChannelMutationResult::Applied)
            }
            ChannelMutation::Reorder { kind, ids } => {
                let channels = self.channels.entry(kind).or_default();
                for (order, id) in ids.iter().enumerate() {
                    if let Some(channel) = channels.iter_mut().find(|channel| &channel.id == id) {
                        channel.order = order as u32;
                    }
                }
                (kind, ChannelMutationResult::Applied)
            }
            ChannelMutation::RecordTest {
                kind,
                id,
                ok,
                latency_ms,
                at,
                error,
            } => {
                find_channel_mut(&mut self.channels, kind, &id)?.last_test =
                    Some(ChannelTestSummary {
                        ok,
                        latency_ms,
                        at,
                        error,
                    });
                (kind, ChannelMutationResult::Applied)
            }
        };
        normalize_channel_order(self.channels.entry(kind).or_default());
        Ok(result)
    }
}

#[derive(Default)]
pub struct InMemoryCredentialStore {
    values: RwLock<HashMap<CredentialKey, SecretValue>>,
    status: RwLock<CredentialsStatus>,
    metadata: RwLock<CredentialMetadata>,
}

impl InMemoryCredentialStore {
    pub fn set_status(&self, status: CredentialsStatus) {
        *self
            .status
            .write()
            .expect("credential status lock poisoned") = status;
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn status(
        &self,
        preferences: UserPreferences,
    ) -> BoxFuture<'static, Result<CredentialsStatus, BackendError>> {
        let mut status = self
            .status
            .read()
            .expect("credential status lock poisoned")
            .clone();
        status.pipeline_mode = preferences.pipeline_mode;
        Box::pin(async move { Ok(status) })
    }

    fn read(
        &self,
        key: CredentialKey,
    ) -> BoxFuture<'static, Result<Option<SecretValue>, BackendError>> {
        let value = self
            .values
            .read()
            .expect("credential values lock poisoned")
            .get(&key)
            .cloned();
        Box::pin(async move { Ok(value) })
    }

    fn write(
        &self,
        key: CredentialKey,
        value: SecretValue,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        self.values
            .write()
            .expect("credential values lock poisoned")
            .insert(key, value);
        Box::pin(async { Ok(()) })
    }

    fn remove(&self, key: CredentialKey) -> BoxFuture<'static, Result<(), BackendError>> {
        self.values
            .write()
            .expect("credential values lock poisoned")
            .remove(&key);
        Box::pin(async { Ok(()) })
    }

    fn list_channels(
        &self,
        kind: ChannelKind,
    ) -> BoxFuture<'static, Result<Vec<ChannelSummary>, BackendError>> {
        let channels = self
            .metadata
            .read()
            .expect("credential metadata lock poisoned")
            .list_channels(kind);
        Box::pin(async move { Ok(channels) })
    }

    fn mutate_channel(
        &self,
        mutation: ChannelMutation,
    ) -> BoxFuture<'static, Result<ChannelMutationResult, BackendError>> {
        let values = self.values.read().expect("credential values lock poisoned");
        let result = self
            .metadata
            .write()
            .expect("credential metadata lock poisoned")
            .apply_channel_mutation(mutation, |id| {
                values
                    .keys()
                    .any(|key| key.provider_id.as_deref() == Some(id))
            });
        Box::pin(async move { result })
    }

    fn active_provider(
        &self,
        slot: ProviderSlot,
    ) -> BoxFuture<'static, Result<String, BackendError>> {
        let provider = self
            .metadata
            .read()
            .expect("credential metadata lock poisoned")
            .active_provider(slot);
        Box::pin(async move { Ok(provider) })
    }

    fn set_active_provider(
        &self,
        slot: ProviderSlot,
        provider_id: String,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        self.metadata
            .write()
            .expect("credential metadata lock poisoned")
            .set_active_provider(slot, provider_id);
        Box::pin(async { Ok(()) })
    }
}

fn find_channel_mut<'a>(
    all_channels: &'a mut HashMap<ChannelKind, Vec<ChannelSummary>>,
    kind: ChannelKind,
    id: &str,
) -> Result<&'a mut ChannelSummary, BackendError> {
    all_channels
        .entry(kind)
        .or_default()
        .iter_mut()
        .find(|channel| channel.id == id)
        .ok_or_else(|| {
            BackendError::new(
                BackendErrorCode::InvalidArgument,
                format!("unknown {kind:?} channel: {id}"),
            )
        })
}

fn normalize_channel_order(channels: &mut [ChannelSummary]) {
    channels.sort_by_key(|channel| (!channel.enabled, channel.order, channel.id.clone()));
    for (order, channel) in channels.iter_mut().enumerate() {
        channel.order = order as u32;
    }
}

pub struct UnsupportedCredentialStore;

impl CredentialStore for UnsupportedCredentialStore {
    fn status(
        &self,
        preferences: UserPreferences,
    ) -> BoxFuture<'static, Result<CredentialsStatus, BackendError>> {
        Box::pin(async move {
            Ok(CredentialsStatus {
                pipeline_mode: preferences.pipeline_mode,
                ..CredentialsStatus::default()
            })
        })
    }

    fn read(
        &self,
        _key: CredentialKey,
    ) -> BoxFuture<'static, Result<Option<SecretValue>, BackendError>> {
        unsupported_credentials()
    }

    fn write(
        &self,
        _key: CredentialKey,
        _value: SecretValue,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        unsupported_credentials()
    }

    fn remove(&self, _key: CredentialKey) -> BoxFuture<'static, Result<(), BackendError>> {
        unsupported_credentials()
    }
}

fn unsupported_credentials<T>() -> BoxFuture<'static, Result<T, BackendError>> {
    Box::pin(async {
        Err(BackendError::new(
            BackendErrorCode::Unsupported,
            "credential store is not configured",
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_store_round_trips_secrets_without_exposing_debug_or_serde() {
        let store = InMemoryCredentialStore::default();
        let key = CredentialKey::new(
            CredentialNamespace::Asr,
            Some("fixture".to_string()),
            "api_key",
        )
        .unwrap();
        let secret = SecretValue::new("do-not-log-this");
        assert_eq!(format!("{secret:?}"), "SecretValue([REDACTED])");

        store.write(key.clone(), secret).await.unwrap();
        assert_eq!(
            store
                .read(key.clone())
                .await
                .unwrap()
                .unwrap()
                .expose_secret(),
            "do-not-log-this"
        );
        store.remove(key.clone()).await.unwrap();
        assert!(store.read(key).await.unwrap().is_none());
    }

    #[test]
    fn credential_keys_reject_blank_identifiers() {
        assert_eq!(
            CredentialKey::new(CredentialNamespace::Llm, None, " ")
                .unwrap_err()
                .code,
            BackendErrorCode::InvalidArgument
        );
        assert_eq!(
            CredentialKey::new(CredentialNamespace::Llm, Some(" ".to_string()), "api_key")
                .unwrap_err()
                .code,
            BackendErrorCode::InvalidArgument
        );
    }
}
