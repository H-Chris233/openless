//! Core-owned provider management operations.
//!
//! The runtime engines in [`crate::cloud_providers`] already own the actual
//! ASR/LLM/Omni protocols.  This module is the management seam around those
//! engines: it resolves a channel, reads its credentials through the typed
//! [`CredentialStore`] port, validates connectivity, and lists models.  Hosts
//! must not duplicate these rules.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;

use crate::cloud_providers::{SharedCloudTextPolisher, SharedCloudTranscriptionEngine};
use crate::credentials::{
    ChannelKind, CredentialKey, CredentialNamespace, CredentialStore, ProviderSlot,
};
use crate::dictation_context::{DictationContext, ProviderInvocation};
use crate::domains::{
    ProviderApi, ProviderCheckResult, ProviderKind, ProviderModelsResult, ProviderRequest,
};
use crate::errors::{BackendError, BackendErrorCode};
use crate::ports::{TextPolisher, TextStreamChunk, TextStreamSink, TranscriptionEngine};
use crate::provider_transport::{
    ProviderCancellation, ProviderTransport, ProviderTransportError, ProviderTransportRequest,
    ReqwestProviderTransport,
};
use crate::shared_types::PipelineMode;
use crate::types::SessionId;
use crate::{encode_dictation_wav, TaskSpawner};

const ASR_MODEL_ACCOUNT: &str = "asr.model";
const ASR_API_KEY_ACCOUNT: &str = "asr.api_key";
const ASR_ENDPOINT_ACCOUNT: &str = "asr.endpoint";
const LLM_MODEL_ACCOUNT: &str = "ark.model";
const LLM_API_KEY_ACCOUNT: &str = "ark.api_key";
const LLM_ENDPOINT_ACCOUNT: &str = "ark.endpoint";
const LLM_EXTRA_HEADERS_ACCOUNT: &str = "ark.extra_headers";
const OMNI_MODEL_ACCOUNT: &str = "omni.model";
const OMNI_API_KEY_ACCOUNT: &str = "omni.api_key";
const OMNI_ENDPOINT_ACCOUNT: &str = "omni.endpoint";
const OMNI_EXTRA_HEADERS_ACCOUNT: &str = "omni.extra_headers";

const MODEL_LIST_MAX_BYTES: usize = 2 * 1024 * 1024;
const MODEL_LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// Shared implementation of [`ProviderApi`] for every non-UI host.
#[derive(Clone)]
pub struct ProviderService {
    credentials: Arc<dyn CredentialStore>,
    task_spawner: Arc<dyn TaskSpawner>,
    transport: Arc<dyn ProviderTransport>,
}

impl ProviderService {
    pub fn new(credentials: Arc<dyn CredentialStore>, task_spawner: Arc<dyn TaskSpawner>) -> Self {
        Self::new_with_transport(
            credentials,
            task_spawner,
            Arc::new(ReqwestProviderTransport::new()),
        )
    }

    /// Construct the service with an explicit model-list transport.
    ///
    /// Production hosts should normally use [`Self::new`].  Tests and hosts
    /// with a different networking policy can inject a transport without
    /// changing provider resolution or response parsing semantics.
    pub fn new_with_transport(
        credentials: Arc<dyn CredentialStore>,
        task_spawner: Arc<dyn TaskSpawner>,
        transport: Arc<dyn ProviderTransport>,
    ) -> Self {
        Self {
            credentials,
            task_spawner,
            transport,
        }
    }

    async fn resolve(&self, request: ProviderRequest) -> Result<ResolvedProvider, BackendError> {
        let (namespace, slot, channel_kind) = match request.kind {
            ProviderKind::Asr => (
                CredentialNamespace::Asr,
                ProviderSlot::Asr,
                ChannelKind::Asr,
            ),
            ProviderKind::Llm => (
                CredentialNamespace::Llm,
                ProviderSlot::Llm,
                ChannelKind::Llm,
            ),
            ProviderKind::Omni => (
                CredentialNamespace::Omni,
                ProviderSlot::Omni,
                ChannelKind::Llm,
            ),
        };
        if request.kind == ProviderKind::Omni && request.channel_id.is_some() {
            return Err(invalid_request("omni provider does not support channel id"));
        }

        let channel_is_explicit = request.channel_id.is_some();
        let provider_id = match request.channel_id {
            Some(id) if !id.trim().is_empty() => id,
            Some(_) => return Err(invalid_request("provider channel id must not be blank")),
            None => {
                let id = self.credentials.active_provider(slot).await?;
                if id.trim().is_empty() {
                    return Err(provider_error("provider channel is not configured"));
                }
                id
            }
        };

        let provider_type = if request.kind == ProviderKind::Omni {
            provider_id.clone()
        } else {
            let channels = self.credentials.list_channels(channel_kind).await?;
            let channel = channels
                .into_iter()
                .find(|channel| channel.id == provider_id);
            if channel_is_explicit && channel.is_none() {
                return Err(provider_error("provider channel is not configured"));
            }
            channel
                .map(|channel| channel.provider_type)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| provider_id.clone())
        };
        if provider_type.trim().is_empty() {
            return Err(invalid_request("provider type must not be blank"));
        }

        let (model_account, key_account, endpoint_account, extra_headers_account) =
            match request.kind {
                ProviderKind::Asr => (
                    ASR_MODEL_ACCOUNT,
                    ASR_API_KEY_ACCOUNT,
                    ASR_ENDPOINT_ACCOUNT,
                    None,
                ),
                ProviderKind::Llm => (
                    LLM_MODEL_ACCOUNT,
                    LLM_API_KEY_ACCOUNT,
                    LLM_ENDPOINT_ACCOUNT,
                    Some(LLM_EXTRA_HEADERS_ACCOUNT),
                ),
                ProviderKind::Omni => (
                    OMNI_MODEL_ACCOUNT,
                    OMNI_API_KEY_ACCOUNT,
                    OMNI_ENDPOINT_ACCOUNT,
                    Some(OMNI_EXTRA_HEADERS_ACCOUNT),
                ),
            };
        let model = self.read(namespace, &provider_id, model_account).await?;
        let api_key = self.read(namespace, &provider_id, key_account).await?;
        let endpoint = self.read(namespace, &provider_id, endpoint_account).await?;
        let extra_headers = match extra_headers_account {
            Some(account) => self.read(namespace, &provider_id, account).await?,
            None => None,
        };

        Ok(ResolvedProvider {
            kind: request.kind,
            provider_id,
            provider_type,
            model,
            api_key,
            endpoint,
            extra_headers,
        })
    }

    async fn read(
        &self,
        namespace: CredentialNamespace,
        provider_id: &str,
        account: &str,
    ) -> Result<Option<String>, BackendError> {
        let key = CredentialKey::new(namespace, Some(provider_id.to_string()), account)?;
        self.credentials
            .read(key)
            .await
            .map(|value| value.map(crate::SecretValue::into_exposed))
    }

    async fn validate_inner(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderCheckResult, BackendError> {
        let resolved = self.resolve(request).await?;
        ensure_supported_kind(&resolved)?;
        let context = Arc::new(resolved.context());
        let session_id = SessionId::new();
        match resolved.kind {
            ProviderKind::Asr => {
                let engine = SharedCloudTranscriptionEngine::with_task_spawner(
                    Arc::clone(&self.credentials),
                    Arc::clone(&self.task_spawner),
                );
                let session = engine
                    .start(session_id, context, Arc::new(DiscardTextStream))
                    .await?;
                // A 250 ms 16 kHz mono silence probe exercises the same
                // request/handshake path without storing user audio.
                let pcm = vec![0_u8; 16_000 / 2 * 2 / 4];
                let wav = encode_dictation_wav(&pcm)?;
                session.consume_pcm_chunk(&wav[44..]);
                session.finish().await.map(|_| ())?;
            }
            ProviderKind::Llm => {
                let polisher = SharedCloudTextPolisher::new(Arc::clone(&self.credentials));
                polisher
                    .polish(
                        session_id,
                        context,
                        "验证连接".to_string(),
                        Arc::new(DiscardTextStream),
                    )
                    .await?;
            }
            ProviderKind::Omni => {
                crate::cloud_providers::validate_shared_omni_provider(
                    Arc::clone(&self.credentials),
                    context,
                )
                .await?;
            }
        }
        Ok(ProviderCheckResult { ok: true })
    }

    async fn list_models_inner(
        &self,
        request: ProviderRequest,
        cancellation: ProviderCancellation,
    ) -> Result<ProviderModelsResult, BackendError> {
        let resolved = self.resolve(request).await?;
        ensure_supported_kind(&resolved)?;
        if let Some(models) = static_models(&resolved) {
            validate_configuration(&resolved)?;
            return Ok(ProviderModelsResult { models });
        }
        validate_configuration(&resolved)?;
        let models = fetch_models(&resolved, Arc::clone(&self.transport), cancellation).await?;
        Ok(ProviderModelsResult { models })
    }

    /// Cancelable variant used by hosts that expose an explicit in-flight
    /// provider management cancellation action.  The legacy [`ProviderApi`]
    /// method uses a fresh token and remains source-compatible.
    pub fn list_models_with_cancellation(
        &self,
        request: ProviderRequest,
        cancellation: ProviderCancellation,
    ) -> BoxFuture<'static, Result<ProviderModelsResult, BackendError>> {
        let service = self.clone();
        Box::pin(async move { service.list_models_inner(request, cancellation).await })
    }
}

impl ProviderApi for ProviderService {
    fn validate(
        &self,
        request: ProviderRequest,
    ) -> BoxFuture<'static, Result<ProviderCheckResult, BackendError>> {
        let service = self.clone();
        Box::pin(async move { service.validate_inner(request).await })
    }

    fn list_models(
        &self,
        request: ProviderRequest,
    ) -> BoxFuture<'static, Result<ProviderModelsResult, BackendError>> {
        let service = self.clone();
        Box::pin(async move {
            service
                .list_models_inner(request, ProviderCancellation::new())
                .await
        })
    }
}

#[derive(Debug, Clone)]
struct ResolvedProvider {
    kind: ProviderKind,
    provider_id: String,
    provider_type: String,
    model: Option<String>,
    api_key: Option<String>,
    endpoint: Option<String>,
    extra_headers: Option<String>,
}

impl ResolvedProvider {
    fn context(&self) -> DictationContext {
        let mut context = DictationContext::default();
        let invocation = ProviderInvocation {
            provider_id: self.provider_id.clone(),
            provider_type: self.provider_type.clone(),
            model: self.model.clone().filter(|value| !value.trim().is_empty()),
            language: None,
            prompt: None,
            runtime: None,
            keep_loaded_secs: None,
        };
        match self.kind {
            ProviderKind::Asr => context.asr = invocation,
            ProviderKind::Llm => context.llm = invocation,
            ProviderKind::Omni => {
                context.pipeline_mode = PipelineMode::Multimodal;
                context.omni = invocation;
            }
        }
        context
    }
}

fn ensure_supported_kind(resolved: &ResolvedProvider) -> Result<(), BackendError> {
    let supported = match resolved.kind {
        ProviderKind::Asr => crate::SHARED_CLOUD_ASR_PROVIDER_TYPES
            .iter()
            .any(|value| *value == resolved.provider_type),
        ProviderKind::Llm => crate::SHARED_CLOUD_LLM_PROVIDER_TYPES
            .iter()
            .any(|value| *value == resolved.provider_type),
        ProviderKind::Omni => crate::SHARED_OMNI_PROVIDER_TYPES
            .iter()
            .any(|value| *value == resolved.provider_type),
    };
    if supported {
        Ok(())
    } else {
        Err(BackendError::new(
            BackendErrorCode::Unsupported,
            "provider validation is not available for this native or unknown provider",
        ))
    }
}

fn validate_configuration(resolved: &ResolvedProvider) -> Result<(), BackendError> {
    match resolved.kind {
        ProviderKind::Asr => {
            if resolved.provider_type != "openai-compatible"
                && resolved
                    .api_key
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
            {
                return Err(provider_error("ASR API key is not configured"));
            }
            let model = resolved.model.as_deref().unwrap_or_default().trim();
            if model.is_empty() && static_models(resolved).is_none() {
                return Err(invalid_request("ASR model is not configured"));
            }
            if let Some(endpoint) = resolved
                .endpoint
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                crate::endpoint_security::validate_http_endpoint(endpoint)?;
            }
        }
        ProviderKind::Llm => {
            if resolved.provider_type != crate::polish::CODEX_OAUTH_PROVIDER_ID
                && resolved
                    .api_key
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
            {
                return Err(provider_error("LLM API key is not configured"));
            }
            if resolved
                .model
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
                && default_llm_model(&resolved.provider_type).is_none()
            {
                return Err(invalid_request("LLM model is not configured"));
            }
            if resolved.provider_type != crate::polish::CODEX_OAUTH_PROVIDER_ID {
                let endpoint = resolved
                    .endpoint
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| default_llm_endpoint(&resolved.provider_type))
                    .ok_or_else(|| provider_error("LLM endpoint is not configured"))?;
                crate::endpoint_security::validate_http_endpoint(endpoint)?;
                if let Some(headers) = resolved.extra_headers.as_deref() {
                    parse_extra_headers(headers)?;
                }
            }
        }
        ProviderKind::Omni => {
            if resolved
                .api_key
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
            {
                return Err(provider_error("Omni API key is not configured"));
            }
            if resolved
                .model
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
            {
                return Err(invalid_request("Omni model is not configured"));
            }
            let endpoint = resolved
                .endpoint
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| default_omni_endpoint(&resolved.provider_type))
                .ok_or_else(|| provider_error("Omni endpoint is not configured"))?;
            crate::endpoint_security::validate_http_endpoint(endpoint)?;
            if let Some(headers) = resolved.extra_headers.as_deref() {
                parse_extra_headers(headers)?;
            }
        }
    }
    Ok(())
}

fn static_models(resolved: &ResolvedProvider) -> Option<Vec<String>> {
    let models = match resolved.kind {
        ProviderKind::Asr => match resolved.provider_type.as_str() {
            "bailian" => vec![
                crate::asr::bailian::DEFAULT_MODEL,
                "fun-asr-flash-8k-realtime",
                crate::asr::qwen_realtime::DEFAULT_MODEL,
                "qwen3-asr-flash-realtime-2026-02-10",
                "qwen3-asr-flash-realtime-2025-10-27",
                crate::asr::dashscope_multimodal::QWEN_AUDIO_MODEL,
                crate::asr::dashscope_multimodal::DEFAULT_MODEL,
                "qwen3-asr-flash",
                "fun-asr",
                "fun-asr-2025-11-07",
                "fun-asr-2025-08-25",
                "fun-asr-mtl",
                "fun-asr-mtl-2025-08-25",
                "paraformer-v2",
            ],
            "bailian-qwen3-realtime" => vec![
                crate::asr::qwen_realtime::DEFAULT_MODEL,
                "qwen3-asr-flash-realtime-2026-02-10",
                "qwen3-asr-flash-realtime-2025-10-27",
            ],
            "xiaomi-mimo-asr" => vec![crate::asr::mimo::DEFAULT_MODEL],
            "bailian-fun-asr-flash" => vec![
                crate::asr::dashscope_multimodal::QWEN_AUDIO_MODEL,
                crate::asr::dashscope_multimodal::DEFAULT_MODEL,
            ],
            "elevenlabs" => vec![crate::asr::elevenlabs::DEFAULT_MODEL],
            "dashscope-omni" => vec!["qwen-audio-turbo", "qwen-omni-turbo"],
            _ => return None,
        },
        ProviderKind::Llm => match resolved.provider_type.as_str() {
            crate::polish::CODEX_OAUTH_PROVIDER_ID => vec![
                crate::polish::CODEX_DEFAULT_MODEL,
                "gpt-5.3-codex",
                "gpt-5.4",
                "gpt-5.5",
            ],
            _ => return None,
        },
        ProviderKind::Omni => return None,
    };
    let mut seen = std::collections::HashSet::new();
    Some(
        models
            .into_iter()
            .filter(|model| seen.insert(*model))
            .map(str::to_string)
            .collect(),
    )
}

async fn fetch_models(
    resolved: &ResolvedProvider,
    transport: Arc<dyn ProviderTransport>,
    cancellation: ProviderCancellation,
) -> Result<Vec<String>, BackendError> {
    let endpoint = resolved
        .endpoint
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| default_llm_endpoint(&resolved.provider_type))
        .or_else(|| default_omni_endpoint(&resolved.provider_type))
        .ok_or_else(|| provider_error("provider endpoint is not configured"))?;
    let url = models_url(endpoint)?;
    let is_gemini =
        crate::net::sanitized_url_for_logs(&url).contains("generativelanguage.googleapis.com");
    let mut request_headers = Vec::new();
    if let Some(api_key) = resolved
        .api_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        if is_gemini {
            request_headers.push(("x-goog-api-key".to_string(), api_key.to_string()));
        } else {
            request_headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
        }
    }
    if let Some(extra_headers) = resolved.extra_headers.as_deref() {
        for (name, value) in parse_extra_headers(extra_headers)? {
            request_headers.push((name, value));
        }
    }
    let response = transport
        .execute(
            ProviderTransportRequest {
                url,
                headers: request_headers,
                timeout: MODEL_LIST_TIMEOUT,
                max_response_bytes: MODEL_LIST_MAX_BYTES,
            },
            cancellation,
        )
        .await
        .map_err(map_transport_error)?;
    if !(200..300).contains(&response.status) {
        return Err(BackendError::new(
            BackendErrorCode::Provider,
            format!("providerHttpStatus:{}", response.status),
        ));
    }
    if response.body.len() > MODEL_LIST_MAX_BYTES {
        return Err(provider_error("provider model response is too large"));
    }
    parse_model_list(&response.body, is_gemini)
}

fn parse_model_list(body: &[u8], is_gemini: bool) -> Result<Vec<String>, BackendError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| provider_error("provider model response is invalid JSON"))?;
    let models = if is_gemini {
        value
            .get("models")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| provider_error("provider model response is missing models"))?
            .iter()
            .filter(|item| {
                item.get("supportedGenerationMethods")
                    .and_then(serde_json::Value::as_array)
                    .map(|methods| {
                        methods
                            .iter()
                            .any(|method| method.as_str() == Some("generateContent"))
                    })
                    .unwrap_or(true)
            })
            .filter_map(|item| item.get("name").and_then(serde_json::Value::as_str))
            .map(|name| {
                name.strip_prefix("models/")
                    .unwrap_or(name)
                    .trim()
                    .to_string()
            })
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>()
    } else {
        value
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| provider_error("provider model response is missing data"))?
            .iter()
            .filter_map(|item| item.get("id").and_then(serde_json::Value::as_str))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let mut models = models;
    models.sort();
    models.dedup();
    Ok(models)
}

fn models_url(endpoint: &str) -> Result<String, BackendError> {
    let mut url = url::Url::parse(endpoint.trim())
        .map_err(|_| invalid_request("provider endpoint is invalid"))?;
    let path = url.path().trim_end_matches('/');
    let next_path = if path.ends_with("/models") {
        path.to_string()
    } else if let Some(prefix) = path.strip_suffix("/chat/completions") {
        format!("{prefix}/models")
    } else {
        format!("{path}/models")
    };
    url.set_path(&next_path);
    Ok(url.to_string())
}

fn parse_extra_headers(
    value: &str,
) -> Result<std::collections::HashMap<String, String>, BackendError> {
    if value.trim().is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let headers: std::collections::HashMap<String, String> = serde_json::from_str(value)
        .map_err(|_| invalid_request("LLM extra headers must be a JSON object"))?;
    for name in headers.keys() {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "content-type" | "accept" | "host" | "content-length"
        ) {
            return Err(invalid_request(
                "LLM extra headers contain a reserved header",
            ));
        }
    }
    Ok(headers)
}

fn default_llm_endpoint(provider_type: &str) -> Option<&'static str> {
    match provider_type {
        "ark" => Some("https://ark.cn-beijing.volces.com/api/v3"),
        "deepseek" => Some("https://api.deepseek.com/v1"),
        "siliconflow" => Some("https://api.siliconflow.cn/v1"),
        "atlascloud" => Some("https://api.atlascloud.ai/v1"),
        "openai" => Some("https://api.openai.com/v1"),
        "gemini" => Some("https://generativelanguage.googleapis.com/v1beta"),
        "mimo" => Some("https://api.xiaomimimo.com/v1"),
        "cometapi" => Some("https://api.cometapi.com/v1"),
        "openrouterFree" => Some("https://openrouter.ai/api/v1"),
        "alibabaCoding" => Some("https://coding-intl.dashscope.aliyuncs.com/v1"),
        "codingPlanX" => Some("https://api.codingplanx.ai/v1"),
        "minimax" => Some("https://api.minimaxi.com/v1"),
        "stepfun" => Some("https://api.stepfun.com/v1"),
        _ => None,
    }
}

fn default_llm_model(provider_type: &str) -> Option<&'static str> {
    match provider_type {
        "ark" => Some("deepseek-v3-2"),
        "deepseek" => Some("deepseek-v4-flash"),
        "siliconflow" => Some("Qwen/Qwen2.5-7B-Instruct"),
        "atlascloud" => Some("qwen/qwen3.5-flash"),
        "openai" | "cometapi" => Some("gpt-4o"),
        "gemini" => Some("gemini-2.5-flash"),
        crate::polish::CODEX_OAUTH_PROVIDER_ID => Some(crate::polish::CODEX_DEFAULT_MODEL),
        "mimo" => Some("xiaomi/mimo-v2-flash"),
        "openrouterFree" => Some("qwen/qwen3-coder:free"),
        "alibabaCoding" => Some("qwen3-coder-plus"),
        "codingPlanX" => Some("gpt-5-mini"),
        "minimax" => Some("MiniMax-M3"),
        "stepfun" => Some("step-1o-turbo-vision"),
        _ => None,
    }
}

fn default_omni_endpoint(provider_type: &str) -> Option<&'static str> {
    match provider_type {
        "openai" => Some("https://api.openai.com/v1"),
        "gemini" => Some("https://generativelanguage.googleapis.com/v1beta"),
        "dashscope-omni" => Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        _ => None,
    }
}

fn map_transport_error(error: ProviderTransportError) -> BackendError {
    match error {
        ProviderTransportError::Timeout => {
            BackendError::new(BackendErrorCode::Provider, "provider request timed out")
                .retryable(true)
        }
        ProviderTransportError::Connection => BackendError::new(
            BackendErrorCode::Provider,
            "provider network connection failed",
        )
        .retryable(true),
        ProviderTransportError::Cancelled => {
            BackendError::new(BackendErrorCode::Cancelled, "provider request cancelled")
        }
        ProviderTransportError::ResponseTooLarge => {
            provider_error("provider model response is too large")
        }
        ProviderTransportError::Request => {
            BackendError::new(BackendErrorCode::Provider, "provider request failed")
        }
    }
}

fn invalid_request(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::InvalidArgument, message)
}

fn provider_error(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::Provider, message)
}

struct DiscardTextStream;

impl TextStreamSink for DiscardTextStream {
    fn publish(&self, _chunk: TextStreamChunk) -> Result<(), BackendError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::{
        ChannelMutation, ChannelMutationResult, InMemoryCredentialStore, SecretValue,
    };
    use crate::provider_transport::{ProviderCancellation, ProviderTransportError};
    use crate::testing::FakeProviderTransport;

    async fn service_with_channel() -> (ProviderService, Arc<InMemoryCredentialStore>) {
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let created = credentials
            .mutate_channel(ChannelMutation::Create {
                kind: ChannelKind::Llm,
                provider_type: "openai".to_string(),
                name: "test".to_string(),
            })
            .await
            .unwrap();
        let id = match created {
            ChannelMutationResult::Created(id) => id,
            other => panic!("unexpected mutation result: {other:?}"),
        };
        credentials
            .set_active_provider(ProviderSlot::Llm, id.clone())
            .await
            .unwrap();
        credentials
            .write(
                CredentialKey::new(
                    CredentialNamespace::Llm,
                    Some(id.clone()),
                    LLM_API_KEY_ACCOUNT,
                )
                .unwrap(),
                SecretValue::new("test-key"),
            )
            .await
            .unwrap();
        let credential_store: Arc<dyn CredentialStore> = credentials.clone();
        let service = ProviderService::new(credential_store, Arc::new(crate::TokioTaskSpawner));
        (service, credentials)
    }

    #[tokio::test]
    async fn channel_resolution_does_not_cross_channels() {
        let (service, credentials) = service_with_channel().await;
        let error = service
            .list_models(ProviderRequest {
                kind: ProviderKind::Llm,
                channel_id: Some("missing".to_string()),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, BackendErrorCode::Provider);
        assert!(!format!("{error:?}").contains("test-key"));
        let _ = credentials;
    }

    #[tokio::test]
    async fn omni_channel_is_rejected_before_credential_access() {
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let service = ProviderService::new(credentials, Arc::new(crate::TokioTaskSpawner));
        let error = service
            .validate(ProviderRequest {
                kind: ProviderKind::Omni,
                channel_id: Some("channel".to_string()),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, BackendErrorCode::InvalidArgument);
    }

    #[tokio::test]
    async fn channel_credentials_are_scoped_and_active_resolution_is_explicit() {
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let first = credentials
            .mutate_channel(ChannelMutation::Create {
                kind: ChannelKind::Llm,
                provider_type: "openai".to_string(),
                name: "first".to_string(),
            })
            .await
            .unwrap();
        let second = credentials
            .mutate_channel(ChannelMutation::Create {
                kind: ChannelKind::Llm,
                provider_type: "gemini".to_string(),
                name: "second".to_string(),
            })
            .await
            .unwrap();
        let first_id = match first {
            ChannelMutationResult::Created(id) => id,
            _ => panic!("first channel was not created"),
        };
        let second_id = match second {
            ChannelMutationResult::Created(id) => id,
            _ => panic!("second channel was not created"),
        };
        for (id, key) in [(&first_id, "first-secret"), (&second_id, "second-secret")] {
            credentials
                .write(
                    CredentialKey::new(
                        CredentialNamespace::Llm,
                        Some(id.clone()),
                        LLM_API_KEY_ACCOUNT,
                    )
                    .unwrap(),
                    SecretValue::new(key),
                )
                .await
                .unwrap();
        }
        credentials
            .set_active_provider(ProviderSlot::Llm, first_id.clone())
            .await
            .unwrap();
        let credential_store: Arc<dyn CredentialStore> = credentials.clone();
        let service = ProviderService::new(credential_store, Arc::new(crate::TokioTaskSpawner));

        let first_resolved = service
            .resolve(ProviderRequest {
                kind: ProviderKind::Llm,
                channel_id: Some(first_id.clone()),
            })
            .await
            .unwrap();
        let second_resolved = service
            .resolve(ProviderRequest {
                kind: ProviderKind::Llm,
                channel_id: Some(second_id.clone()),
            })
            .await
            .unwrap();
        let active_resolved = service
            .resolve(ProviderRequest {
                kind: ProviderKind::Llm,
                channel_id: None,
            })
            .await
            .unwrap();

        assert_eq!(first_resolved.provider_type, "openai");
        assert_eq!(second_resolved.provider_type, "gemini");
        assert_eq!(first_resolved.api_key.as_deref(), Some("first-secret"));
        assert_eq!(second_resolved.api_key.as_deref(), Some("second-secret"));
        assert_eq!(active_resolved.provider_id, first_id);
    }

    #[test]
    fn model_url_preserves_query_and_changes_only_path() {
        let url = models_url("https://example.com/v1/chat/completions?token=query-secret#fragment")
            .unwrap();
        assert_eq!(
            url,
            "https://example.com/v1/models?token=query-secret#fragment"
        );
    }

    #[test]
    fn openai_model_response_is_sorted_deduplicated_and_redacted() {
        let models = parse_model_list(
            br#"{"data":[{"id":"gpt-z"},{"id":""},{"id":"gpt-a"},{"id":"gpt-z"}]}"#,
            false,
        )
        .unwrap();
        assert_eq!(models, vec!["gpt-a", "gpt-z"]);
    }

    #[test]
    fn gemini_model_response_filters_unsupported_methods() {
        let models = parse_model_list(
            br#"{"models":[{"name":"models/gemini-z","supportedGenerationMethods":["generateContent"]},{"name":"models/embedding","supportedGenerationMethods":["embedContent"]},{"name":"gemini-a"}]}"#,
            true,
        )
        .unwrap();
        assert_eq!(models, vec!["gemini-a", "gemini-z"]);
    }

    #[test]
    fn invalid_model_response_is_a_provider_error_without_body() {
        let error = parse_model_list(br#"{"error":"secret-key"}"#, false).unwrap_err();
        assert_eq!(error.code, BackendErrorCode::Provider);
        assert!(!format!("{error:?}").contains("secret-key"));
    }

    async fn service_with_fake_transport() -> (ProviderService, Arc<FakeProviderTransport>, String)
    {
        let credentials = Arc::new(InMemoryCredentialStore::default());
        let created = credentials
            .mutate_channel(ChannelMutation::Create {
                kind: ChannelKind::Llm,
                provider_type: "openai".to_string(),
                name: "transport fixture".to_string(),
            })
            .await
            .unwrap();
        let id = match created {
            ChannelMutationResult::Created(id) => id,
            other => panic!("unexpected mutation result: {other:?}"),
        };
        credentials
            .set_active_provider(ProviderSlot::Llm, id.clone())
            .await
            .unwrap();
        for (account, value) in [
            (LLM_API_KEY_ACCOUNT, "provider-secret"),
            (
                LLM_ENDPOINT_ACCOUNT,
                "https://example.test/v1?token=url-secret",
            ),
            (LLM_EXTRA_HEADERS_ACCOUNT, r#"{"x-tenant":"header-secret"}"#),
        ] {
            credentials
                .write(
                    CredentialKey::new(CredentialNamespace::Llm, Some(id.clone()), account)
                        .unwrap(),
                    SecretValue::new(value),
                )
                .await
                .unwrap();
        }
        let transport = Arc::new(FakeProviderTransport::default());
        let credential_store: Arc<dyn CredentialStore> = credentials;
        let service = ProviderService::new_with_transport(
            credential_store,
            Arc::new(crate::TokioTaskSpawner),
            transport.clone(),
        );
        (service, transport, id)
    }

    #[tokio::test]
    async fn fake_transport_parses_models_and_redacts_request_debug() {
        let (service, transport, channel) = service_with_fake_transport().await;
        transport.push_response(
            200,
            br#"{"data":[{"id":"gpt-z"},{"id":"gpt-a"},{"id":"gpt-z"}]}"#,
        );

        let result = service
            .list_models(ProviderRequest {
                kind: ProviderKind::Llm,
                channel_id: Some(channel),
            })
            .await
            .unwrap();
        assert_eq!(result.models, vec!["gpt-a", "gpt-z"]);

        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer provider-secret"));
        assert!(request
            .headers
            .iter()
            .any(|(name, value)| name == "x-tenant" && value == "header-secret"));
        let debug = format!("{request:?}");
        for secret in ["provider-secret", "header-secret", "url-secret"] {
            assert!(!debug.contains(secret), "transport debug leaked {secret}");
        }
        assert_eq!(
            request.url,
            "https://example.test/v1/models?token=url-secret"
        );
    }

    #[tokio::test]
    async fn fake_transport_maps_status_timeout_cancel_size_and_invalid_json() {
        let (service, transport, channel) = service_with_fake_transport().await;
        for (status, expected) in [
            (401, "providerHttpStatus:401"),
            (403, "providerHttpStatus:403"),
            (429, "providerHttpStatus:429"),
            (500, "providerHttpStatus:500"),
            (302, "providerHttpStatus:302"),
        ] {
            transport.push_response(status, br#"{"data":[]}"#);
            let error = service
                .list_models(ProviderRequest {
                    kind: ProviderKind::Llm,
                    channel_id: Some(channel.clone()),
                })
                .await
                .unwrap_err();
            assert_eq!(error.code, BackendErrorCode::Provider);
            assert_eq!(error.message, expected);
            assert!(!error.retryable);
        }

        transport.push_response(200, br#"not-json secret-body"#);
        let error = service
            .list_models(ProviderRequest {
                kind: ProviderKind::Llm,
                channel_id: Some(channel.clone()),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, BackendErrorCode::Provider);
        assert!(!format!("{error:?}").contains("secret-body"));

        transport.push_response(200, vec![b'x'; MODEL_LIST_MAX_BYTES + 1]);
        let error = service
            .list_models(ProviderRequest {
                kind: ProviderKind::Llm,
                channel_id: Some(channel.clone()),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, BackendErrorCode::Provider);
        assert!(error.message.contains("too large"));

        for (transport_error, code, retryable) in [
            (
                ProviderTransportError::Timeout,
                BackendErrorCode::Provider,
                true,
            ),
            (
                ProviderTransportError::Connection,
                BackendErrorCode::Provider,
                true,
            ),
            (
                ProviderTransportError::Request,
                BackendErrorCode::Provider,
                false,
            ),
            (
                ProviderTransportError::ResponseTooLarge,
                BackendErrorCode::Provider,
                false,
            ),
            (
                ProviderTransportError::Cancelled,
                BackendErrorCode::Cancelled,
                false,
            ),
        ] {
            transport.push_error(transport_error);
            let error = service
                .list_models(ProviderRequest {
                    kind: ProviderKind::Llm,
                    channel_id: Some(channel.clone()),
                })
                .await
                .unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(error.retryable, retryable);
        }
        assert_eq!(transport.requests().len(), 12);
    }

    #[tokio::test]
    async fn cancellation_token_stops_fake_transport_before_dispatch() {
        let (service, transport, channel) = service_with_fake_transport().await;
        transport.push_response(200, br#"{"data":[{"id":"never-used"}]}"#);
        let cancellation = ProviderCancellation::new();
        cancellation.cancel();
        let error = service
            .list_models_with_cancellation(
                ProviderRequest {
                    kind: ProviderKind::Llm,
                    channel_id: Some(channel),
                },
                cancellation,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, BackendErrorCode::Cancelled);
        assert_eq!(transport.requests().len(), 1);
    }

    #[test]
    fn static_model_lists_match_legacy_provider_order_without_duplicates() {
        let expected = [
            (
                "bailian",
                vec![
                    "fun-asr-realtime",
                    "fun-asr-flash-8k-realtime",
                    "qwen3-asr-flash-realtime",
                    "qwen3-asr-flash-realtime-2026-02-10",
                    "qwen3-asr-flash-realtime-2025-10-27",
                    "qwen-audio-3.0-asr-flash",
                    "fun-asr-flash-2026-06-15",
                    "qwen3-asr-flash",
                    "fun-asr",
                    "fun-asr-2025-11-07",
                    "fun-asr-2025-08-25",
                    "fun-asr-mtl",
                    "fun-asr-mtl-2025-08-25",
                    "paraformer-v2",
                ],
            ),
            (
                "bailian-qwen3-realtime",
                vec![
                    "qwen3-asr-flash-realtime",
                    "qwen3-asr-flash-realtime-2026-02-10",
                    "qwen3-asr-flash-realtime-2025-10-27",
                ],
            ),
            ("xiaomi-mimo-asr", vec!["mimo-v2.5-asr"]),
            (
                "bailian-fun-asr-flash",
                vec!["qwen-audio-3.0-asr-flash", "fun-asr-flash-2026-06-15"],
            ),
            ("elevenlabs", vec!["scribe_v2"]),
        ];
        for (provider_type, expected_models) in expected {
            let resolved = ResolvedProvider {
                kind: ProviderKind::Asr,
                provider_id: provider_type.to_string(),
                provider_type: provider_type.to_string(),
                model: None,
                api_key: None,
                endpoint: None,
                extra_headers: None,
            };
            let actual = static_models(&resolved).expect("provider should have static models");
            assert_eq!(actual, expected_models);
            let unique = actual.iter().collect::<std::collections::HashSet<_>>();
            assert_eq!(unique.len(), actual.len());
        }
    }
}
