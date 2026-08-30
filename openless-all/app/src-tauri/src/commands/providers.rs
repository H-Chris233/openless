use super::*;

fn parse_provider_kind(value: &str) -> Result<openless_core::ProviderKind, String> {
    match value {
        "asr" => Ok(openless_core::ProviderKind::Asr),
        "llm" => Ok(openless_core::ProviderKind::Llm),
        "omni" => Ok(openless_core::ProviderKind::Omni),
        other => Err(format!("unknown provider kind: {other}")),
    }
}

/// `channel_id = None` 保留旧 React 语义：验证当前 active provider。
/// 指定 channel 时由 Core 按渠道 metadata 解析凭据和 provider 协议。
#[tauri::command]
pub async fn validate_provider_credentials(
    core: CoreState<'_>,
    kind: String,
    channel_id: Option<String>,
) -> Result<openless_core::ProviderCheckResult, String> {
    let kind = parse_provider_kind(&kind)?;
    core.services()
        .provider
        .validate(openless_core::ProviderRequest { kind, channel_id })
        .await
        .map_err(|error| error.message)
}

#[tauri::command]
pub async fn list_provider_models(
    core: CoreState<'_>,
    kind: String,
    channel_id: Option<String>,
) -> Result<openless_core::ProviderModelsResult, String> {
    let kind = parse_provider_kind(&kind)?;
    core.services()
        .provider
        .list_models(openless_core::ProviderRequest { kind, channel_id })
        .await
        .map_err(|error| error.message)
}
