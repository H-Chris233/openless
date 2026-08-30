use std::sync::Arc;

use crate::credentials::{
    ChannelKind, CredentialKey, CredentialNamespace, CredentialStore, ProviderSlot,
};
use crate::dictation_context::ProviderInvocation;
use crate::errors::{BackendError, BackendErrorCode};

pub(crate) async fn resolve_session_provider(
    credential_store: &Arc<dyn CredentialStore>,
    slot: ProviderSlot,
    preference_fallback: &str,
) -> Result<ProviderInvocation, BackendError> {
    let provider_id = match credential_store.active_provider(slot).await {
        Ok(provider) if !provider.trim().is_empty() => provider,
        Ok(_) => preference_fallback.to_string(),
        Err(error) if error.code == BackendErrorCode::Unsupported => {
            preference_fallback.to_string()
        }
        Err(error) => return Err(error),
    };
    let channel_kind = match slot {
        ProviderSlot::Asr => Some(ChannelKind::Asr),
        ProviderSlot::Llm => Some(ChannelKind::Llm),
        ProviderSlot::Omni => None,
    };
    let provider_type = if let Some(kind) = channel_kind {
        match credential_store.list_channels(kind).await {
            Ok(channels) => match channels
                .into_iter()
                .find(|channel| channel.id == provider_id)
            {
                Some(channel) if channel.enabled => channel.provider_type,
                Some(_) => {
                    return Err(BackendError::new(
                        BackendErrorCode::InvalidState,
                        "the selected provider channel is disabled",
                    ));
                }
                None => provider_id.clone(),
            },
            Err(error) if error.code == BackendErrorCode::Unsupported => provider_id.clone(),
            Err(error) => return Err(error),
        }
    } else {
        provider_id.clone()
    };
    let (namespace, channel_id, account) = match slot {
        ProviderSlot::Asr => (
            CredentialNamespace::Asr,
            Some(provider_id.clone()),
            "asr.model",
        ),
        ProviderSlot::Llm => (
            CredentialNamespace::Llm,
            Some(provider_id.clone()),
            "ark.model_id",
        ),
        ProviderSlot::Omni => (CredentialNamespace::Omni, None, "omni.model"),
    };
    let model_key = CredentialKey::new(namespace, channel_id, account)?;
    let model = match credential_store.read(model_key).await {
        Ok(value) => value
            .map(crate::credentials::SecretValue::into_exposed)
            .filter(|value| !value.trim().is_empty()),
        Err(error) if error.code == BackendErrorCode::Unsupported => None,
        Err(error) => return Err(error),
    };
    Ok(ProviderInvocation {
        provider_id,
        provider_type,
        model,
        language: None,
        prompt: None,
        runtime: None,
        keep_loaded_secs: None,
    })
}
