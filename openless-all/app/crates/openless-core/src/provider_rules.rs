//! Cross-host provider routing and request-shape rules.
//!
//! Platform adapters own sockets, native runtimes, credential access, and UI
//! authorization. This module owns the deterministic decisions that every host
//! must make identically once configuration values have been supplied.

use std::time::Duration;

pub const OPENAI_COMPATIBLE_ASR_PROVIDER_ID: &str = "openai-compatible";
pub const ZENMUX_ASR_PROVIDER_ID: &str = "zenmux";

const BAILIAN_PROVIDER_ID: &str = "bailian";
const QWEN3_REALTIME_PROVIDER_ID: &str = "bailian-qwen3-realtime";
const STEPFUN_REALTIME_PROVIDER_ID: &str = "stepfun-realtime";
const MIMO_PROVIDER_ID: &str = "xiaomi-mimo-asr";
const DASHSCOPE_MULTIMODAL_PROVIDER_ID: &str = "bailian-fun-asr-flash";
const ELEVENLABS_PROVIDER_ID: &str = "elevenlabs";
const XFYUN_PROVIDER_ID: &str = "iflytek";

const BAILIAN_DEFAULT_ENDPOINT: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/inference/";
const QWEN3_REALTIME_DEFAULT_ENDPOINT: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/realtime";
const DASHSCOPE_MULTIMODAL_DEFAULT_ENDPOINT: &str =
    "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation";
const DASHSCOPE_ASYNC_DEFAULT_ENDPOINT: &str =
    "https://dashscope.aliyuncs.com/api/v1/services/audio/asr/transcription";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveAsrProviderKind {
    Bailian,
    Qwen3Realtime,
    StepfunRealtime,
    Mimo,
    DashScopeMultimodal,
    ElevenLabs,
    WhisperCompatible,
    Volcengine,
    Xfyun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrPreflightCredential {
    AsrApiKey,
    VolcAppKey,
    XfyunAppKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrConfiguredFields {
    ApiKeyOnly,
    ApiKeyEndpointModel,
    EndpointModelOnly,
    VolcAppKey,
    XfyunAppKey,
}

impl ActiveAsrProviderKind {
    pub fn preflight_credential(self) -> AsrPreflightCredential {
        match self {
            Self::Bailian
            | Self::Qwen3Realtime
            | Self::StepfunRealtime
            | Self::Mimo
            | Self::DashScopeMultimodal
            | Self::ElevenLabs
            | Self::WhisperCompatible => AsrPreflightCredential::AsrApiKey,
            Self::Volcengine => AsrPreflightCredential::VolcAppKey,
            Self::Xfyun => AsrPreflightCredential::XfyunAppKey,
        }
    }

    pub fn configured_fields(self) -> AsrConfiguredFields {
        match self {
            Self::Bailian | Self::Qwen3Realtime | Self::ElevenLabs => {
                AsrConfiguredFields::ApiKeyOnly
            }
            Self::Mimo | Self::DashScopeMultimodal => AsrConfiguredFields::ApiKeyEndpointModel,
            Self::WhisperCompatible | Self::StepfunRealtime => {
                AsrConfiguredFields::EndpointModelOnly
            }
            Self::Volcengine => AsrConfiguredFields::VolcAppKey,
            Self::Xfyun => AsrConfiguredFields::XfyunAppKey,
        }
    }
}

pub fn active_asr_provider_kind(id: &str) -> ActiveAsrProviderKind {
    match id {
        BAILIAN_PROVIDER_ID => ActiveAsrProviderKind::Bailian,
        QWEN3_REALTIME_PROVIDER_ID => ActiveAsrProviderKind::Qwen3Realtime,
        STEPFUN_REALTIME_PROVIDER_ID => ActiveAsrProviderKind::StepfunRealtime,
        MIMO_PROVIDER_ID => ActiveAsrProviderKind::Mimo,
        DASHSCOPE_MULTIMODAL_PROVIDER_ID => ActiveAsrProviderKind::DashScopeMultimodal,
        ELEVENLABS_PROVIDER_ID => ActiveAsrProviderKind::ElevenLabs,
        XFYUN_PROVIDER_ID => ActiveAsrProviderKind::Xfyun,
        value if is_whisper_compatible_provider(value) => ActiveAsrProviderKind::WhisperCompatible,
        _ => ActiveAsrProviderKind::Volcengine,
    }
}

pub fn is_bailian_provider(id: &str) -> bool {
    id == BAILIAN_PROVIDER_ID
}

pub fn is_qwen3_realtime_provider(id: &str) -> bool {
    id == QWEN3_REALTIME_PROVIDER_ID
}

pub fn is_stepfun_realtime_provider(id: &str) -> bool {
    id == STEPFUN_REALTIME_PROVIDER_ID
}

pub fn is_mimo_provider(id: &str) -> bool {
    id == MIMO_PROVIDER_ID
}

pub fn is_dashscope_multimodal_provider(id: &str) -> bool {
    id == DASHSCOPE_MULTIMODAL_PROVIDER_ID
}

pub fn is_elevenlabs_provider(id: &str) -> bool {
    id == ELEVENLABS_PROVIDER_ID
}

pub fn is_xfyun_provider(id: &str) -> bool {
    id == XFYUN_PROVIDER_ID
}

pub fn is_whisper_compatible_provider(id: &str) -> bool {
    matches!(
        id,
        "whisper" | "siliconflow" | "zhipu" | "groq" | "openrouter" | "stepfun" | "zenmux"
    ) || id == OPENAI_COMPATIBLE_ASR_PROVIDER_ID
}

pub fn resolve_effective_asr_provider(active_asr: &str, model: &str) -> Result<String, String> {
    if !is_bailian_provider(active_asr) {
        if is_dashscope_multimodal_provider(active_asr) {
            validate_dashscope_multimodal_model(model)?;
        }
        if active_asr == "stepfun" && stepfun_model_is_stream(model) {
            return Ok(STEPFUN_REALTIME_PROVIDER_ID.to_string());
        }
        return Ok(active_asr.to_string());
    }

    let model = model.trim();
    if model.is_empty() || is_classic_bailian_realtime_model(model) {
        Ok(BAILIAN_PROVIDER_ID.to_string())
    } else if model.starts_with("qwen3-asr-flash-realtime") {
        Ok(QWEN3_REALTIME_PROVIDER_ID.to_string())
    } else if dashscope_batch_protocol_for_model(model).is_some() {
        Ok(DASHSCOPE_MULTIMODAL_PROVIDER_ID.to_string())
    } else {
        Err(format!(
            "不支持的百炼 ASR 模型：{model}。支持 Fun-ASR、Paraformer、SenseVoice、qwen-audio-3.0-asr-flash 和 Qwen3-ASR 的实时、同步及录音文件模型"
        ))
    }
}

fn is_classic_bailian_realtime_model(model: &str) -> bool {
    model.starts_with("fun-asr-realtime")
        || model.starts_with("fun-asr-flash-8k-realtime")
        || model.starts_with("paraformer-realtime")
        || model.starts_with("paraformer-8k-realtime")
        || model.starts_with("sensevoice-realtime")
        || model.starts_with("sensevoice-8k-realtime")
}

pub fn stepfun_model_is_stream(model: &str) -> bool {
    model.trim().ends_with("-stream")
}

pub fn validate_dashscope_multimodal_model(model: &str) -> Result<(), String> {
    let model = model.trim();
    if model.is_empty() || dashscope_batch_protocol_for_model(model).is_some() {
        return Ok(());
    }
    Err(format!("不支持的 DashScope 录音文件 ASR 模型：{model}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashScopeBatchProtocol {
    Multimodal,
    AsyncTranscription,
}

pub fn dashscope_batch_protocol_for_model(model: &str) -> Option<DashScopeBatchProtocol> {
    let model = model.trim();
    if model.is_empty() || model.contains("realtime") {
        return None;
    }
    if model.starts_with("qwen3-asr-flash-filetrans") {
        return None;
    }
    let qwen_sync = dashscope_uses_qwen_sync_envelope(model);
    let qwen_audio = model.starts_with("qwen-audio") && !model.contains("streaming");
    if model.starts_with("fun-asr-flash") || qwen_sync || qwen_audio {
        return Some(DashScopeBatchProtocol::Multimodal);
    }
    if model == "fun-asr" || model.starts_with("fun-asr-") || model.starts_with("paraformer") {
        return Some(DashScopeBatchProtocol::AsyncTranscription);
    }
    None
}

pub fn dashscope_uses_qwen_sync_envelope(model: &str) -> bool {
    let model = model.trim();
    model.starts_with("qwen3-asr-flash")
        && !model.starts_with("qwen3-asr-flash-filetrans")
        && !model.contains("realtime")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BailianEndpointProtocol {
    ClassicRealtime,
    QwenRealtime,
    Multimodal,
    AsyncTranscription,
}

pub fn derive_bailian_endpoint(
    endpoint: &str,
    protocol: BailianEndpointProtocol,
) -> Result<String, String> {
    let default_endpoint = match protocol {
        BailianEndpointProtocol::ClassicRealtime => BAILIAN_DEFAULT_ENDPOINT,
        BailianEndpointProtocol::QwenRealtime => QWEN3_REALTIME_DEFAULT_ENDPOINT,
        BailianEndpointProtocol::Multimodal => DASHSCOPE_MULTIMODAL_DEFAULT_ENDPOINT,
        BailianEndpointProtocol::AsyncTranscription => DASHSCOPE_ASYNC_DEFAULT_ENDPOINT,
    };
    let source = if endpoint.trim().is_empty() {
        default_endpoint
    } else {
        endpoint.trim()
    };
    let mut url = url::Url::parse(source).map_err(|_| "endpointInvalid".to_string())?;
    if url.host_str().is_none() {
        return Err("endpointInvalid".to_string());
    }
    let (scheme, path) = match protocol {
        BailianEndpointProtocol::ClassicRealtime => ("wss", "/api-ws/v1/inference/"),
        BailianEndpointProtocol::QwenRealtime => ("wss", "/api-ws/v1/realtime"),
        BailianEndpointProtocol::Multimodal => (
            "https",
            "/api/v1/services/aigc/multimodal-generation/generation",
        ),
        BailianEndpointProtocol::AsyncTranscription => {
            ("https", "/api/v1/services/audio/asr/transcription")
        }
    };
    url.set_scheme(scheme)
        .map_err(|_| "endpointInvalid".to_string())?;
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvancedAsrConfig {
    pub verbose_json: bool,
    pub chunk_duration_ms: Option<u64>,
    pub enable_itn: bool,
}

impl Default for AdvancedAsrConfig {
    fn default() -> Self {
        Self {
            verbose_json: false,
            chunk_duration_ms: None,
            enable_itn: true,
        }
    }
}

pub fn parse_advanced_asr_config(raw: Option<&str>) -> AdvancedAsrConfig {
    let Some(raw) = raw else {
        return AdvancedAsrConfig::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return AdvancedAsrConfig::default();
    };
    AdvancedAsrConfig {
        verbose_json: value
            .get("verboseJson")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        chunk_duration_ms: value.get("chunkDurationMs").and_then(|value| {
            value.as_u64().filter(|millis| *millis > 0).or_else(|| {
                value
                    .as_f64()
                    .filter(|millis| {
                        millis.is_finite() && *millis > 0.0 && *millis <= u64::MAX as f64
                    })
                    .map(|millis| millis.floor() as u64)
            })
        }),
        enable_itn: value
            .get("enableItn")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
    }
}

pub fn advanced_asr_config_for(provider_id: &str, raw: Option<&str>) -> AdvancedAsrConfig {
    if provider_id != OPENAI_COMPATIBLE_ASR_PROVIDER_ID && provider_id != ZENMUX_ASR_PROVIDER_ID {
        return AdvancedAsrConfig::default();
    }
    parse_advanced_asr_config(raw)
}

pub fn batch_asr_chunk_limit_ms(provider_id: &str, advanced: AdvancedAsrConfig) -> Option<u64> {
    match provider_id {
        "zhipu" | "openrouter" | "zenmux" => Some(30_000),
        _ => advanced.chunk_duration_ms,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrRequestFormat {
    Multipart,
    OpenRouterJson,
    ZenMuxJson,
}

pub fn whisper_request_format(provider_id: &str) -> AsrRequestFormat {
    match provider_id {
        "openrouter" => AsrRequestFormat::OpenRouterJson,
        "zenmux" => AsrRequestFormat::ZenMuxJson,
        _ => AsrRequestFormat::Multipart,
    }
}

pub fn whisper_uses_hotwords(provider_id: &str) -> bool {
    provider_id == "stepfun"
}

pub fn whisper_supports_verbose_json(provider_id: &str, advanced: AdvancedAsrConfig) -> bool {
    match provider_id {
        "whisper" | "groq" => true,
        "zenmux" => false,
        _ => advanced.verbose_json,
    }
}

pub fn zenmux_language_code(native_name: &str) -> Option<String> {
    let code = match native_name.trim() {
        "简体中文" | "繁体中文" => "zh",
        "English" => "en",
        "日本語" => "ja",
        "한국어" => "ko",
        "Français" => "fr",
        "Deutsch" => "de",
        "Español" => "es",
        "Italiano" => "it",
        "Português" => "pt",
        "Русский" => "ru",
        "العربية" => "ar",
        "Tiếng Việt" => "vi",
        "ไทย" => "th",
        "हिन्दी" => "hi",
        _ => return None,
    };
    Some(code.to_string())
}

pub fn volc_resource_history_label(resource_id: &str) -> Option<String> {
    let id = resource_id.trim();
    let allowed = id.starts_with("volc.")
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    allowed.then(|| id.to_string())
}

pub fn whisper_transcribe_timeout(audio_secs: f64) -> Duration {
    let secs = ((audio_secs * 0.5).ceil() as u64)
        .saturating_add(20)
        .max(30);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_bailian_and_stepfun_models() {
        assert_eq!(
            resolve_effective_asr_provider(BAILIAN_PROVIDER_ID, "fun-asr-realtime").unwrap(),
            BAILIAN_PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider(BAILIAN_PROVIDER_ID, "qwen3-asr-flash-realtime")
                .unwrap(),
            QWEN3_REALTIME_PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider(BAILIAN_PROVIDER_ID, "fun-asr-flash-2026-06-15")
                .unwrap(),
            DASHSCOPE_MULTIMODAL_PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider("stepfun", "stepaudio-2.5-asr-stream").unwrap(),
            STEPFUN_REALTIME_PROVIDER_ID
        );
        assert!(resolve_effective_asr_provider(BAILIAN_PROVIDER_ID, "unknown-asr").is_err());
    }

    #[test]
    fn derives_bailian_protocol_endpoints_without_leaking_source_paths() {
        let source = "https://workspace.ap-southeast-1.maas.aliyuncs.com/custom?x=1";
        assert_eq!(
            derive_bailian_endpoint(source, BailianEndpointProtocol::ClassicRealtime).unwrap(),
            "wss://workspace.ap-southeast-1.maas.aliyuncs.com/api-ws/v1/inference/"
        );
        assert_eq!(
            derive_bailian_endpoint(source, BailianEndpointProtocol::AsyncTranscription).unwrap(),
            "https://workspace.ap-southeast-1.maas.aliyuncs.com/api/v1/services/audio/asr/transcription"
        );
    }

    #[test]
    fn advanced_config_is_scoped_and_conservative() {
        let parsed = advanced_asr_config_for(
            OPENAI_COMPATIBLE_ASR_PROVIDER_ID,
            Some(r#"{"verboseJson":true,"chunkDurationMs":30000.9,"enableItn":false}"#),
        );
        assert!(parsed.verbose_json);
        assert_eq!(parsed.chunk_duration_ms, Some(30_000));
        assert!(!parsed.enable_itn);
        assert_eq!(
            advanced_asr_config_for("whisper", Some(r#"{"verboseJson":true}"#)),
            AdvancedAsrConfig::default()
        );
    }

    #[test]
    fn request_shape_and_timeout_rules_are_stable() {
        assert_eq!(
            whisper_request_format("openrouter"),
            AsrRequestFormat::OpenRouterJson
        );
        assert_eq!(
            whisper_request_format("zenmux"),
            AsrRequestFormat::ZenMuxJson
        );
        assert_eq!(
            batch_asr_chunk_limit_ms("openrouter", AdvancedAsrConfig::default()),
            Some(30_000)
        );
        assert_eq!(whisper_transcribe_timeout(10.0), Duration::from_secs(30));
        assert_eq!(whisper_transcribe_timeout(60.0), Duration::from_secs(50));
    }

    #[test]
    fn secret_like_volc_resource_ids_are_not_attributed() {
        assert_eq!(
            volc_resource_history_label("volc.seedasr.sauc.duration").as_deref(),
            Some("volc.seedasr.sauc.duration")
        );
        assert_eq!(volc_resource_history_label("my-secret-tenant"), None);
        assert_eq!(volc_resource_history_label("volc.a b"), None);
    }
}
