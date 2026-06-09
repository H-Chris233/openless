//! LLM 润色 / 翻译 / provider 构建 / 凭证读取 管线。
//!
//! 从 `coordinator.rs` 机械拆出（行为保持，仅可见性提升）：流式/一次性润色、
//! 润色+翻译合并、各 ASR/LLM provider 凭证读取与 ActiveLLMProvider 构建、
//! provider 分类器与中文字形偏好、词库 phrases/hotwords 读取。
//! polish-translate marker 常量原样保留（父模块 tests 经 super:: 引用）。

use super::*;

pub(crate) fn raw_style_pack_uses_llm(pack: &crate::types::StylePack) -> bool {
    !(pack.kind == crate::types::StylePackKind::Builtin
        && pack.id == crate::types::BUILTIN_STYLE_PACK_RAW_ID
        && pack.prompt == crate::types::StyleSystemPrompts::default().raw)
}

pub(crate) fn raw_mode_uses_llm(style_system_prompt: &str) -> bool {
    style_system_prompt != crate::types::StyleSystemPrompts::default().raw
}

/// `whisper` 是 OpenAI 原生；`siliconflow` / `zhipu` / `groq` 都暴露
/// OpenAI 兼容的 `/audio/transcriptions`，统一走 `WhisperBatchASR`。
/// 新增 OpenAI 兼容 ASR 时只需在这里加一项。
///
/// 注：DashScope 的 Qwen3-ASR-Flash 不在此列——它用 MultiModalConversation
/// (messages=[{content:[{audio:...}]}]) 协议，不是 Whisper multipart，需要
/// 单独 ASR 客户端，留给 V2。
pub(crate) fn is_whisper_compatible_provider(id: &str) -> bool {
    matches!(
        id,
        "whisper" | "siliconflow" | "zhipu" | "groq" | "openrouter"
    )
}

/// 该 provider 的请求体编码方式。OpenRouter 的 `/audio/transcriptions` 是
/// `application/json` + base64 音频（issue #582），其余兼容厂商沿用 multipart。
pub(crate) fn whisper_request_format(provider_id: &str) -> crate::asr::whisper::AsrRequestFormat {
    match provider_id {
        "openrouter" => crate::asr::whisper::AsrRequestFormat::OpenRouterJson,
        _ => crate::asr::whisper::AsrRequestFormat::Multipart,
    }
}

/// 该 provider 的 `/audio/transcriptions` 是否支持 `response_format=verbose_json`
/// 并返回带 `no_speech_prob` / `avg_logprob` / `compression_ratio` 的 segments，
/// 用于幻听过滤。
///
/// - `whisper`（OpenAI）/ `groq`：原生 Whisper，完整支持，过滤有效。
/// - `siliconflow`：模型是 SenseVoice / TeleSpeech，文档无 `response_format`，
///   发送 verbose_json 可能被拒，**保持关闭**走旧的 `json`。
/// - `zhipu`（GLM-ASR）：虽接受 verbose_json，但不产出上述指标，过滤是空转；
///   为最小化行为变更，这里也**保持关闭**，仅对确证有收益的 whisper/groq 开启。
pub(crate) fn whisper_supports_verbose_json(provider_id: &str) -> bool {
    matches!(provider_id, "whisper" | "groq")
}

pub(crate) fn is_bailian_provider(id: &str) -> bool {
    id == crate::asr::bailian::PROVIDER_ID
}

pub(crate) fn is_mimo_provider(id: &str) -> bool {
    id == crate::asr::mimo::PROVIDER_ID
}

pub(crate) fn apply_chinese_script_preference(text: &str, pref: ChineseScriptPreference) -> String {
    if text.is_empty() {
        return String::new();
    }
    let config = match pref {
        ChineseScriptPreference::Simplified => Some(BuiltinConfig::T2s),
        ChineseScriptPreference::Traditional => Some(BuiltinConfig::S2t),
        ChineseScriptPreference::Auto => None,
    };
    let Some(config) = config else {
        return text.to_string();
    };
    match OpenCC::from_config(config) {
        Ok(converter) => converter.convert(text),
        Err(err) => {
            log::warn!("[coord] OpenCC init failed, skip script conversion: {err}");
            text.to_string()
        }
    }
}

/// 润色文本；失败时返回原文 + 失败原因，调用方据此弹错误胶囊 + 写历史 error_code。
/// 之前固定返回 String，调用方拿不到失败信号 → 用户感知"为什么风格设置没生效"。issue #57。
/// 流式润色的三态结果。让上层（dictation pipeline）能区分「已经流出去了」、
/// 「降级到一次性」和「真失败了走 raw 兜底」三种 case。
pub(crate) enum StreamingPolishOutcome {
    /// 流式润色成功，`String` 是已经一边流一边交给 `on_delta` 的全部文本（用于写
    /// history、做词条命中统计）。调用方不应再 `inserter.insert(&text)`，因为字符
    /// 已经通过键盘事件落到光标处。
    Streamed(String),
    /// 当前配置不支持流式：用户没开 streaming_insert / Gemini provider / Codex
    /// provider / Raw 模式 / 翻译模式 / 不是 macOS。调用方应回到现有的
    /// `polish_or_passthrough` 一次性路径，跟历史行为完全一致。
    UnsupportedFallback,
    /// 流式过程中失败（HTTP / 解析 / 空流等）。`String` 是失败原因，调用方应当
    /// 走 raw 兜底（同 `polish_or_passthrough` 失败分支的语义）。
    Failed(String),
}

/// 流式润色入口。在不支持流式的所有 case 都返回 `UnsupportedFallback`，让调用方
/// 透明降级。不修改任何持久化 / 焦点 / 光标状态。
///
/// `on_delta` 每收到一个 SSE chunk 就被调用一次（同步），调用方负责把 chunk 实际
/// 模拟键盘事件落到光标 —— 见 `coordinator/dictation.rs` 的流式分支。
/// `should_cancel` 用户取消时返回 true，立即 break SSE 读循环避免烧 quota。
pub(crate) async fn polish_or_passthrough_streaming<F, C>(
    raw: &RawTranscript,
    mode: PolishMode,
    hotwords: &[String],
    style_system_prompt: &str,
    working_languages: &[String],
    chinese_script_preference: ChineseScriptPreference,
    output_language_preference: OutputLanguagePreference,
    llm_thinking_enabled: bool,
    front_app: Option<&str>,
    prior_turns: &[(String, String)],
    on_delta: F,
    should_cancel: C,
) -> StreamingPolishOutcome
where
    F: Fn(&str) + Send + Sync,
    C: Fn() -> bool + Send + Sync,
{
    if mode == PolishMode::Raw && !raw_mode_uses_llm(style_system_prompt) {
        log::info!("[coord] streaming polish skipped: mode=Raw, fall back to one-shot");
        return StreamingPolishOutcome::UnsupportedFallback;
    }
    let active_llm = CredentialsVault::get_active_llm();
    if active_llm == "gemini" {
        log::info!(
            "[coord] streaming polish skipped: active LLM provider=gemini (v1 not implemented), fall back to one-shot"
        );
        return StreamingPolishOutcome::UnsupportedFallback;
    }
    let provider = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        build_active_llm_provider(llm_thinking_enabled)
    })) {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            log::error!("[coord] streaming polish: build provider failed: {e}");
            return StreamingPolishOutcome::Failed(e.to_string());
        }
        Err(panic) => {
            let msg = format!("build_active_llm_provider panicked: {:?}", panic);
            log::error!("[coord] {msg}");
            return StreamingPolishOutcome::Failed(msg);
        }
    };
    if !provider.supports_streaming_polish() {
        log::info!(
            "[coord] streaming polish skipped: provider does not support streaming (likely codex OAuth), fall back to one-shot"
        );
        return StreamingPolishOutcome::UnsupportedFallback;
    }
    log::info!(
        "[coord] streaming polish START: provider=openai-compatible mode={:?} raw_chars={} prior_turns={}",
        mode,
        raw.text.chars().count(),
        prior_turns.len()
    );
    match provider
        .polish_streaming(
            &raw.text,
            mode,
            hotwords,
            style_system_prompt,
            working_languages,
            chinese_script_preference,
            output_language_preference,
            front_app,
            prior_turns,
            on_delta,
            should_cancel,
        )
        .await
    {
        Ok(text) => {
            log::info!(
                "[coord] streaming polish OK: final_chars={}",
                text.chars().count()
            );
            StreamingPolishOutcome::Streamed(text)
        }
        Err(e) => {
            let reason = e.to_string();
            log::error!("[coord] streaming polish FAILED: {reason}");
            StreamingPolishOutcome::Failed(reason)
        }
    }
}

pub(crate) async fn polish_or_passthrough(
    raw: &RawTranscript,
    mode: PolishMode,
    hotwords: &[String],
    style_system_prompt: &str,
    working_languages: &[String],
    chinese_script_preference: ChineseScriptPreference,
    output_language_preference: OutputLanguagePreference,
    llm_thinking_enabled: bool,
    front_app: Option<&str>,
    prior_turns: &[(String, String)],
) -> (String, Option<String>) {
    if mode == PolishMode::Raw && !raw_mode_uses_llm(style_system_prompt) {
        return (raw.text.clone(), None);
    }
    match polish_text(
        &raw.text,
        mode,
        hotwords,
        style_system_prompt,
        working_languages,
        chinese_script_preference,
        output_language_preference,
        llm_thinking_enabled,
        front_app,
        prior_turns,
    )
    .await
    {
        Ok(s) => (s, None),
        Err(e) => {
            let reason = e.to_string();
            log::error!("[coord] polish failed, falling back to raw: {reason}");
            (raw.text.clone(), Some(reason))
        }
    }
}

pub(crate) async fn polish_text(
    raw: &str,
    mode: PolishMode,
    hotwords: &[String],
    style_system_prompt: &str,
    working_languages: &[String],
    chinese_script_preference: ChineseScriptPreference,
    output_language_preference: OutputLanguagePreference,
    llm_thinking_enabled: bool,
    front_app: Option<&str>,
    prior_turns: &[(String, String)],
) -> anyhow::Result<String> {
    // 谷歌 Gemini 分支：所有 LLM provider 共用 ark.* 凭据槽，唯独 Gemini 走原生
    // generateContent / 自带 thinkingConfig 控制；其余 provider 走 OpenAI
    // 兼容协议，并在该路径里按 provider/channel 下发对应的思考开关。
    let active_llm = CredentialsVault::get_active_llm();
    if active_llm == "gemini" {
        let (api_key, model, base_url) = read_gemini_credentials()?;
        let provider = GeminiProvider::new(
            GeminiConfig::new(api_key, model, base_url).with_thinking_enabled(llm_thinking_enabled),
        );
        return Ok(provider
            .polish(
                raw,
                mode,
                hotwords,
                style_system_prompt,
                working_languages,
                chinese_script_preference,
                output_language_preference,
                front_app,
                prior_turns,
            )
            .await?);
    }

    let provider = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        build_active_llm_provider(llm_thinking_enabled)
    })) {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return Err(e),
        Err(panic) => {
            let msg = format!("build_active_llm_provider panicked: {:?}", panic);
            log::error!("[coord] {msg}");
            anyhow::bail!(msg);
        }
    };
    Ok(provider
        .polish(
            raw,
            mode,
            hotwords,
            style_system_prompt,
            working_languages,
            chinese_script_preference,
            output_language_preference,
            front_app,
            prior_turns,
        )
        .await?)
}

/// 专用翻译（仅翻译、不润色、单轮）。现作为"润色+翻译"合成调用解析失败时的兜底——
/// 模型没按两段格式输出时，退回这里拿一段干净译文，而不是把畸形输出当译文插入。
pub(crate) async fn translate_text(
    raw: &str,
    target_language: &str,
    working_languages: &[String],
    chinese_script_preference: ChineseScriptPreference,
    output_language_preference: OutputLanguagePreference,
    llm_thinking_enabled: bool,
    front_app: Option<&str>,
) -> anyhow::Result<String> {
    // 见 polish_text 顶部注释——同样的 Gemini / OpenAI-compatible 路由逻辑。
    let active_llm = CredentialsVault::get_active_llm();
    if active_llm == "gemini" {
        let (api_key, model, base_url) = read_gemini_credentials()?;
        let provider = GeminiProvider::new(
            GeminiConfig::new(api_key, model, base_url).with_thinking_enabled(llm_thinking_enabled),
        );
        return Ok(provider
            .translate_to(
                raw,
                target_language,
                working_languages,
                chinese_script_preference,
                output_language_preference,
                front_app,
            )
            .await?);
    }

    let provider = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        build_active_llm_provider(llm_thinking_enabled)
    })) {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return Err(e),
        Err(panic) => {
            let msg = format!("build_active_llm_provider panicked: {:?}", panic);
            log::error!("[coord] {msg}");
            anyhow::bail!(msg);
        }
    };
    Ok(provider
        .translate_to(
            raw,
            target_language,
            working_languages,
            chinese_script_preference,
            output_language_preference,
            front_app,
        )
        .await?)
}

/// "润色+翻译"单次调用的两段哨兵。模型按 `SRC\n源文\nTGT\n译文` 输出，解析器据此切分。
/// 这两个串必须与 build_polish_translate_system_prompt 写给模型的完全一致。
pub(crate) const POLISH_TRANSLATE_SRC_MARKER: &str = "[[OPENLESS_POLISHED_SOURCE]]";
pub(crate) const POLISH_TRANSLATE_TGT_MARKER: &str = "[[OPENLESS_TRANSLATION]]";

/// 合成"先润色源文、再翻译"的系统提示词：在原翻译 prompt 之上追加"额外输出润色后源文"
/// 与严格两段格式（覆盖原 prompt 末尾的"只输出译文"）。译文仍是要插入用户光标的主产物，
/// 故完整保留原翻译规则；润色后的源文只作对话上下文用，轻量清理即可。
pub(crate) fn build_polish_translate_system_prompt(target_language: &str) -> String {
    let base = crate::polish::prompts::translate_system_prompt(target_language);
    format!(
        "{base}\n\n\
         # 额外输出：润色后的源文（仅用于对话上下文，不展示给用户）\n\
         在译文之前，先把上面的原始转写**按它本来的语言**润色一遍：去掉口癖（嗯 / 那个 / um）、\
         补必要标点、纠正明显的识别错误，但**不翻译、不改写风格、不增删意思**。\n\n\
         # 输出格式（覆盖上面\u{201C}只输出译文\u{201D}的说明，严格遵守）\n\
         严格按下面两段输出，两个标记必须原样出现、各占一行，标记之外不要有任何多余文字：\n\
         {src}\n\
         （这里放润色后的源文，保持原语言）\n\
         {tgt}\n\
         （这里放翻译成\u{300C}{lang}\u{300D}的译文）",
        base = base,
        src = POLISH_TRANSLATE_SRC_MARKER,
        tgt = POLISH_TRANSLATE_TGT_MARKER,
        lang = target_language,
    )
}

/// 解析"润色+翻译"单次调用输出 → Some((润色后源文, 译文))。
/// 找到译文标记且译文非空 → Some((源文, 译文))：源文标记缺失 / 源文段为空时源文为 None，
/// 译文取标记之后的干净正文。**没有译文标记、或译文段为空（模型截断 / 只吐了标记）→ None**，
/// 表示没拿到可信译文，交由调用方退回专用翻译——避免把空串当"成功译文"插进光标而丢字。
pub(crate) fn split_polish_translate_output(raw: &str) -> Option<(Option<String>, String)> {
    let tgt_idx = raw.find(POLISH_TRANSLATE_TGT_MARKER)?;
    let translation = raw[tgt_idx + POLISH_TRANSLATE_TGT_MARKER.len()..]
        .trim()
        .to_string();
    if translation.is_empty() {
        return None;
    }
    let before_tgt = &raw[..tgt_idx];
    let source = before_tgt
        .find(POLISH_TRANSLATE_SRC_MARKER)
        .map(|i| {
            before_tgt[i + POLISH_TRANSLATE_SRC_MARKER.len()..]
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty());
    Some((source, translation))
}

/// 翻译路径——单次 LLM 调用同时润色源文 + 翻译。和 polish 一样失败时返回原文 + 失败原因，
/// 避免"不丢字"约定被违反（CLAUDE.md）。返回 (要插入的译文, 润色后源文供上下文用, 失败原因)。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn polish_and_translate_or_passthrough(
    raw: &RawTranscript,
    target_language: &str,
    mode: PolishMode,
    hotwords: &[String],
    working_languages: &[String],
    chinese_script_preference: ChineseScriptPreference,
    output_language_preference: OutputLanguagePreference,
    llm_thinking_enabled: bool,
    front_app: Option<&str>,
    prior_turns: &[(String, String)],
) -> (String, Option<String>, Option<String>) {
    let system_prompt = build_polish_translate_system_prompt(target_language);
    match polish_text(
        &raw.text,
        mode,
        hotwords,
        &system_prompt,
        working_languages,
        chinese_script_preference,
        output_language_preference,
        llm_thinking_enabled,
        front_app,
        prior_turns,
    )
    .await
    {
        Ok(out) => match split_polish_translate_output(&out) {
            Some((source, translation)) => (translation, source, None),
            None => {
                // 模型没按两段格式输出：退回专用翻译拿一段干净译文，避免把畸形输出插进光标。
                // 此时无可信源文，这条翻译历史不参与后续普通润色上下文。
                log::warn!(
                    "[coord] polish+translate output missing markers; falling back to plain translate"
                );
                match translate_text(
                    &raw.text,
                    target_language,
                    working_languages,
                    chinese_script_preference,
                    output_language_preference,
                    llm_thinking_enabled,
                    front_app,
                )
                .await
                {
                    Ok(translation) => (translation, None, None),
                    Err(e) => {
                        let reason = e.to_string();
                        log::error!("[coord] fallback translate failed, using raw: {reason}");
                        (raw.text.clone(), None, Some(reason))
                    }
                }
            }
        },
        Err(e) => {
            let reason = e.to_string();
            log::error!("[coord] polish+translate failed, falling back to raw: {reason}");
            (raw.text.clone(), None, Some(reason))
        }
    }
}

/// issue #609 F-01 孪生 gap：ASR 自定义 endpoint 也带 API Key 发请求，运行期听写路径
/// （非「测试连接」按钮）此前完全绕过 SSRF 校验。对 http/https 的 Whisper 兼容 / Mimo
/// endpoint 做与 LLM 路径一致的字面 host/IP 校验；命中元数据/CGNAT/link-local 等被拒地址时
/// **fail-closed** —— 返回提供的安全回退值（Whisper 用空串 → `transcription_url` 解析失败、
/// 请求不发出；Mimo 用官方 DEFAULT_ENDPOINT），绝不把带 Key 的请求指向被拒地址。
/// 注：F-01 放宽后，局域网（RFC1918/ULA）http ASR endpoint 会被 `validate_llm_endpoint`
/// 放行（支持局域网自托管 Whisper 网关），故这里也原样返回、不再回退（见其 doc 安全取舍）。
/// 注：Bailian 走 `wss://`，scheme 与本校验器不兼容，属已知残留（见下方 read_bailian_credentials）。
pub(crate) fn guard_asr_http_endpoint(base_url: String, safe_fallback: &str) -> String {
    if base_url.trim().is_empty() {
        return base_url;
    }
    match validate_llm_endpoint(&base_url) {
        Ok(()) => base_url,
        Err(e) => {
            log::error!(
                "[asr] 自定义 ASR endpoint 被 SSRF 校验拒绝，回退到安全默认值（防凭据外泄）: {e}"
            );
            safe_fallback.to_string()
        }
    }
}

pub(crate) fn read_whisper_credentials() -> (String, String, String) {
    let api_key = CredentialsVault::get(CredentialAccount::AsrApiKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let base_url = CredentialsVault::get(CredentialAccount::AsrEndpoint)
        .ok()
        .flatten()
        .unwrap_or_default();
    // Whisper 无官方默认 endpoint：被拒时回退空串，使 transcription_url 解析失败、请求不发出。
    let base_url = guard_asr_http_endpoint(base_url, "");
    let model = CredentialsVault::get(CredentialAccount::AsrModel)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "whisper-1".to_string());
    (api_key, base_url, model)
}

pub(crate) fn read_mimo_credentials() -> (String, String, String) {
    let api_key = match CredentialsVault::get(CredentialAccount::AsrApiKey) {
        Ok(Some(key)) if !key.trim().is_empty() => key,
        Ok(_) => {
            log::warn!("[coord] MiMo ASR: asr.api_key 未配置或为空");
            String::new()
        }
        Err(e) => {
            log::error!("[coord] MiMo ASR: 读取凭据失败: {e}");
            String::new()
        }
    };
    let base_url = CredentialsVault::get(CredentialAccount::AsrEndpoint)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::mimo::DEFAULT_ENDPOINT.to_string());
    // 被拒时回退到 Mimo 官方 DEFAULT_ENDPOINT（合法 https），不把带 Key 的请求指向内网。
    let base_url = guard_asr_http_endpoint(base_url, crate::asr::mimo::DEFAULT_ENDPOINT);
    let model = CredentialsVault::get(CredentialAccount::AsrModel)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::mimo::DEFAULT_MODEL.to_string());
    (api_key, base_url, model)
}

/// 已知残留（issue #609 F-01 孪生 gap）：Bailian 自定义 endpoint 走 `wss://`，scheme 与
/// `validate_llm_endpoint`（仅 http/https）不兼容，无法直接复用。需要单独的 ws/wss 感知
/// SSRF 校验器，超本次收口范围，留作后续跟进。
pub(crate) fn read_bailian_credentials() -> BailianCredentials {
    let api_key = CredentialsVault::get(CredentialAccount::AsrApiKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let endpoint = CredentialsVault::get(CredentialAccount::AsrEndpoint)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::bailian::DEFAULT_ENDPOINT.to_string());
    let model = CredentialsVault::get(CredentialAccount::AsrModel)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::bailian::DEFAULT_MODEL.to_string());
    let vocabulary_id = CredentialsVault::get(CredentialAccount::AsrVocabularyId)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty());
    BailianCredentials {
        api_key,
        endpoint,
        model,
        vocabulary_id,
    }
}

pub(crate) fn read_volc_credentials() -> VolcengineCredentials {
    let app_id = CredentialsVault::get(CredentialAccount::VolcengineAppKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let access_token = CredentialsVault::get(CredentialAccount::VolcengineAccessKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let resource_id = CredentialsVault::get(CredentialAccount::VolcengineResourceId)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| VolcengineCredentials::default_resource_id().to_string());
    VolcengineCredentials {
        app_id,
        access_token,
        resource_id,
    }
}

pub(crate) fn enabled_hotwords(inner: &Arc<Inner>) -> Vec<DictionaryHotword> {
    inner
        .vocab
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|e| DictionaryHotword {
            phrase: e.phrase,
            enabled: e.enabled,
        })
        .collect()
}

pub(crate) async fn answer_chat_dispatch<F, C>(
    messages: &[crate::types::QaChatMessage],
    working_languages: &[String],
    chinese_script_preference: ChineseScriptPreference,
    output_language_preference: OutputLanguagePreference,
    llm_thinking_enabled: bool,
    front_app: Option<&str>,
    on_delta: F,
    should_cancel: C,
) -> anyhow::Result<String>
where
    F: Fn(&str) + Send + Sync,
    C: Fn() -> bool + Send + Sync,
{
    // 见 polish_text 顶部注释——同样的 Gemini / OpenAI-compatible 路由逻辑，
    // QA 流式回答走 Gemini 原生 :streamGenerateContent?alt=sse。
    let active_llm = CredentialsVault::get_active_llm();
    if active_llm == "gemini" {
        let (api_key, model, base_url) = read_gemini_credentials()?;
        let provider = GeminiProvider::new(
            GeminiConfig::new(api_key, model, base_url).with_thinking_enabled(llm_thinking_enabled),
        );
        return Ok(provider
            .answer_chat_streaming(
                messages,
                working_languages,
                chinese_script_preference,
                output_language_preference,
                front_app,
                on_delta,
                should_cancel,
            )
            .await?);
    }

    let provider = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        build_active_llm_provider(llm_thinking_enabled)
    })) {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return Err(e),
        Err(panic) => {
            let msg = format!("build_active_llm_provider panicked: {:?}", panic);
            log::error!("[coord] {msg}");
            anyhow::bail!(msg);
        }
    };
    Ok(provider
        .answer_chat_streaming(
            messages,
            working_languages,
            chinese_script_preference,
            output_language_preference,
            front_app,
            on_delta,
            should_cancel,
        )
        .await?)
}

/// 读 Gemini 凭据。所有 LLM provider 共用 ark.* 槽位（persistence 没做 per-provider
/// 隔离），所以这里也是从 `ArkApiKey` / `ArkModelId` / `ArkEndpoint` 三个槽读，
/// 但回退默认值改成谷歌的：base_url 默认 `https://generativelanguage.googleapis.com/v1beta`，
/// 模型默认 `gemini-2.5-flash`。Settings.tsx::onLlmProviderChange 在用户切到 gemini
/// 时会强制把 endpoint/model 覆盖为这两个默认值，所以 99% 情况下槽里读出来就是
/// 这两个；这里的 `unwrap_or_else` 是给极端情况兜底（如旧版本切换 bug 留下的脏数据）。
///
/// base_url 末尾去掉 `/`，让 `llm_gemini::generate_content_url` 拼接稳定。
/// 不去 `/chat/completions` 后缀——OpenAI 兼容路径才会有那个后缀，原生 Gemini 不会。
pub(crate) fn read_gemini_credentials() -> anyhow::Result<(String, String, String)> {
    let api_key = CredentialsVault::get(CredentialAccount::ArkApiKey)?.unwrap_or_default();
    let model = CredentialsVault::get(CredentialAccount::ArkModelId)?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "gemini-2.5-flash".to_string());
    let base_url =
        CredentialsVault::get(CredentialAccount::ArkEndpoint)?.filter(|s| !s.trim().is_empty());
    if api_key.trim().is_empty() {
        anyhow::bail!("API Key 为空");
    }
    let base_url = resolve_gemini_base_url(base_url)?;
    Ok((api_key, model, base_url))
}

/// 归一化并校验 Gemini base_url（纯函数，便于单测）。
///
/// issue #609 H-01：Gemini 的 base_url 同样是 attacker-controlled（用户可在凭据库改
/// `ArkEndpoint`），必须和 ark/OpenAI 路径一样过 SSRF 配置校验，否则带 API Key 的请求
/// 可被指向内网/元数据服务。
pub(crate) fn resolve_gemini_base_url(endpoint: Option<String>) -> anyhow::Result<String> {
    let base_url = endpoint
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string())
        .trim_end_matches('/')
        .to_string();
    validate_llm_endpoint(&base_url)?;
    Ok(base_url)
}

pub(crate) fn build_active_llm_provider(
    llm_thinking_enabled: bool,
) -> anyhow::Result<ActiveLLMProvider> {
    let active = CredentialsVault::get_active_llm();
    let model =
        CredentialsVault::get(CredentialAccount::ArkModelId)?.filter(|s| !s.trim().is_empty());
    if active == CODEX_OAUTH_PROVIDER_ID {
        let config =
            CodexOAuthConfig::new(model.unwrap_or_else(|| CODEX_DEFAULT_MODEL.to_string()))
                .with_thinking_enabled(llm_thinking_enabled);
        return Ok(ActiveLLMProvider::Codex(CodexOAuthLLMProvider::new(config)));
    }

    let api_key = CredentialsVault::get(CredentialAccount::ArkApiKey)?.unwrap_or_default();
    let model = model.unwrap_or_else(|| "deepseek-v3-2".to_string());
    let endpoint = resolve_ark_endpoint(&api_key)?;
    let base_url = endpoint
        .trim_end_matches("/chat/completions")
        .trim_end_matches('/')
        .to_string();
    let config = OpenAICompatibleConfig::new(active, "OpenLess LLM", base_url, api_key, model)
        .with_thinking_enabled(llm_thinking_enabled);
    Ok(ActiveLLMProvider::OpenAI(OpenAICompatibleLLMProvider::new(
        config,
    )))
}

pub(crate) fn resolve_ark_endpoint(api_key: &str) -> anyhow::Result<String> {
    let endpoint = CredentialsVault::get(CredentialAccount::ArkEndpoint)?.filter(|s| !s.is_empty());
    resolve_ark_endpoint_with_policy(api_key, endpoint)
}

pub(crate) fn resolve_ark_endpoint_with_policy(
    api_key: &str,
    endpoint: Option<String>,
) -> anyhow::Result<String> {
    if api_key.trim().is_empty() && endpoint.is_none() {
        anyhow::bail!("API Key 为空");
    }
    let mut resolved = endpoint
        .unwrap_or_else(|| "https://ark.cn-beijing.volces.com/api/v3/chat/completions".to_string());
    // 兜底：用户可能在自定义 endpoint 时只写了 "192.168.1.100:8080/v1" 漏了 scheme，
    // 被 reqwest::IntoUrl 自动补 https:// 后对纯 HTTP 的 llama.cpp 发请求会 TLS 握手失败。
    // 检测到无 scheme 时自动补 http://（局域网自托管服务最常见的形态）。
    // 用 to_ascii_lowercase 防用户输大写如 HTTP://... 时被误判为缺 scheme。
    let lower = resolved.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        resolved = format!("http://{resolved}");
        log::info!("[llm] endpoint missing scheme, auto-prepended http://");
    }
    // issue #609 F-01（SSRF）：用户自定义 endpoint 是 attacker-controlled，直接拿来发
    // 带 API Key 的请求等于把凭据指哪打哪。这里对 host/IP 段 + scheme 做配置时校验，
    // 默认官方 endpoint 也过一遍（它本就合法）。注意这只防住"配置即字面内网地址"，
    // **DNS 重绑定**（主机名解析后落到内网 IP）属已知残留，超本 PR 范围。
    validate_llm_endpoint(&resolved)?;
    Ok(resolved)
}

/// issue #609 F-01：对 LLM endpoint 做 SSRF 配置校验。
///
/// 规则：
/// - 已知云元数据主机名/地址子串（`metadata.google.internal`、`169.254.169.254`）：无条件拒绝。
/// - loopback（`localhost` / `127.0.0.1` / `::1`）与**局域网**（RFC1918 私网 10/8、172.16/12、
///   192.168/16；IPv6 ULA `fc00::/7`）：放行 http 或 https。
/// - blocked 保留段（link-local 169.254/16、fe80::/10；CGNAT 100.64.0.0/10(RFC6598)；
///   unspecified 0.0.0.0、`::`；broadcast 255.255.255.255）：始终拒绝。
/// - 公网字面 IP 与公网主机名：强制 `https`（非 https 则拒绝）。
/// - IPv4-mapped IPv6（`::ffff:a.b.c.d`）解包后按内层 v4 规则判定。
///
/// **有意的安全取舍（issue #609 F-01 放宽，用户产品决策）**：
/// 本应用支持「局域网自托管 ASR/LLM 服务」这一真实使用场景——很多用户在内网机器上跑
/// ollama / LM Studio / 自建 Whisper 网关，endpoint 是 `http://192.168.x.x` 这类局域网地址。
/// 为此我们**主动放行 RFC1918 私网与 IPv6 ULA 的 http 请求**。代价是：本校验器**不再防护
/// 「endpoint 指向私网 HTTP 服务」类型的 SSRF**。这是可接受的，因为：
/// 1. endpoint 完全由**用户本人**在设置界面手动配置，不是远程可注入的 attacker-controlled 输入；
/// 2. 我们仍然拦住了**最高价值**的 SSRF 目标——云元数据服务（169.254.169.254 /
///    metadata.google.internal，可偷取云凭据）、CGNAT、link-local、unspecified、broadcast；
/// 3. 公网地址仍强制 https，防止凭据在公网明文外泄。
///
/// **已知残留**：本函数只在配置时校验字面 host/IP，无法防住 DNS 重绑定
/// （主机名解析后再落到内网 IP）。完整防护需在每次请求前解析 + 校验解析结果，
/// 不在本 PR 范围。
pub(crate) fn validate_llm_endpoint(raw: &str) -> anyhow::Result<()> {
    use std::net::IpAddr;

    let url =
        url::Url::parse(raw).map_err(|e| anyhow::anyhow!("LLM endpoint 不是合法 URL：{e}"))?;

    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("LLM endpoint 缺少主机名"))?
        .to_ascii_lowercase();

    // ── 判定顺序：元数据黑名单 → loopback/LAN 放行 → blocked 段拒绝 → 公网强制 https ──

    // 1) 已知云元数据主机名/地址子串：无条件拒绝（最高价值 SSRF 目标，始终挡）。
    const METADATA_HOSTS: [&str; 2] = ["metadata.google.internal", "169.254.169.254"];
    if METADATA_HOSTS.iter().any(|m| host.contains(m)) {
        anyhow::bail!("LLM endpoint 指向云元数据服务，已拒绝：{host}");
    }

    let scheme = url.scheme();
    // `url::Host` 对 IPv6 字面量保留方括号（`host_str()` 返回 `[::1]`、`[fc00::1]`）。
    // 先剥一次方括号得到裸 IP 形式，后面主机名判定与 IpAddr 解析都用它。
    let bare_host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host.as_str());

    // 2) host 不是字面 IP（纯主机名）：localhost 放行 http；其它公网域名强制 https。
    let Ok(ip) = bare_host.parse::<IpAddr>() else {
        if bare_host == "localhost" {
            return Ok(());
        }
        if scheme != "https" {
            anyhow::bail!("LLM endpoint 必须使用 https（仅 localhost / 局域网允许 http）：{raw}");
        }
        return Ok(());
    };

    // IPv4-mapped IPv6（::ffff:a.b.c.d）：解包出内层 v4，统一按 v4 规则判 LAN/blocked。
    let canonical = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };

    // 3) loopback / 局域网（RFC1918、ULA）：放行，不看 scheme。用户的本机/局域网自托管服务。
    let is_lan = match canonical {
        IpAddr::V4(v4) => ip_v4_is_lan(v4),
        IpAddr::V6(v6) => ip_v6_is_lan(v6),
    };
    if is_lan {
        return Ok(());
    }

    // 4) blocked 保留段（link-local / CGNAT / unspecified / broadcast）：始终拒绝。
    let is_blocked = match canonical {
        IpAddr::V4(v4) => ip_v4_is_blocked(v4),
        IpAddr::V6(v6) => ip_v6_is_blocked(v6),
    };
    if is_blocked {
        anyhow::bail!("LLM endpoint 指向保留/危险地址，已拒绝（防 SSRF）：{ip}");
    }

    // 5) 公网字面 IP：强制 https，防止凭据明文外泄。
    if scheme != "https" {
        anyhow::bail!("LLM endpoint 必须使用 https（仅 localhost / 局域网允许 http）：{raw}");
    }

    Ok(())
}

/// 判 IPv4 是否属于「可放行 http」的本机/局域网段：
/// loopback（127/8）、RFC1918 私网（10/8、172.16/12、192.168/16）。
/// 这些是用户自己的本机/局域网，支持局域网自托管服务（见 `validate_llm_endpoint` 安全取舍）。
fn ip_v4_is_lan(ip: std::net::Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private()
}

/// 判 IPv4 是否属于「始终拒绝」的保留/危险段：
/// link-local（169.254/16，含云元数据）、CGNAT（100.64.0.0/10，RFC6598）、
/// unspecified（0.0.0.0）、broadcast（255.255.255.255）。
fn ip_v4_is_blocked(ip: std::net::Ipv4Addr) -> bool {
    // CGNAT 100.64.0.0/10（RFC6598）—— std 没有现成判定，手算。
    let octets = ip.octets();
    let is_cgnat = octets[0] == 100 && (64..=127).contains(&octets[1]);
    ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast() || is_cgnat
}

/// 判 IPv6 是否属于「可放行 http」的本机/局域网段：
/// loopback（::1）、ULA `fc00::/7`。
fn ip_v6_is_lan(ip: std::net::Ipv6Addr) -> bool {
    let segs = ip.segments();
    // ULA fc00::/7：首字节高 7 位 == 0b1111110。
    let is_ula = (segs[0] & 0xfe00) == 0xfc00;
    ip.is_loopback() || is_ula
}

/// 判 IPv6 是否属于「始终拒绝」的保留/危险段：
/// link-local（fe80::/10）、unspecified（::）。
fn ip_v6_is_blocked(ip: std::net::Ipv6Addr) -> bool {
    let segs = ip.segments();
    // link-local fe80::/10。
    let is_link_local = (segs[0] & 0xffc0) == 0xfe80;
    ip.is_unspecified() || is_link_local
}

pub(crate) fn enabled_phrases(inner: &Arc<Inner>) -> Vec<String> {
    inner
        .vocab
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.enabled)
        .map(|e| e.phrase)
        .collect()
}
