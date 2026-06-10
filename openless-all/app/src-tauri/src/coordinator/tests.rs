use super::dictation_session::abort_recording_with_error;
use super::*;
use crate::types::{HotkeyMode, HotkeyTrigger};
use once_cell::sync::Lazy;

static ENV_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

fn session_id(n: u128) -> SessionId {
    Uuid::from_u128(n)
}

#[test]
fn split_polish_translate_parses_both_sections() {
    let out = format!(
        "{POLISH_TRANSLATE_SRC_MARKER}\n你好，世界。\n{POLISH_TRANSLATE_TGT_MARKER}\nHello, world."
    );
    let (source, translation) = split_polish_translate_output(&out).expect("both markers");
    assert_eq!(source.as_deref(), Some("你好，世界。"));
    assert_eq!(translation, "Hello, world.");
}

#[test]
fn split_polish_translate_no_translation_marker_returns_none_for_fallback() {
    // 完全没有译文标记 → None，调用方据此退回专用翻译拿干净译文。
    assert_eq!(split_polish_translate_output("  Hello, world.  "), None);
}

#[test]
fn split_polish_translate_empty_translation_returns_none_for_fallback() {
    // 有译文标记但内容为空（截断 / 只吐标记）→ None，避免空串当成功译文插入光标。
    let out = format!("{POLISH_TRANSLATE_SRC_MARKER}\n你好。\n{POLISH_TRANSLATE_TGT_MARKER}\n   ");
    assert_eq!(split_polish_translate_output(&out), None);
}

#[test]
fn split_polish_translate_only_translation_marker_keeps_clean_translation() {
    let out = format!("noise{POLISH_TRANSLATE_TGT_MARKER}\nHola");
    let (source, translation) = split_polish_translate_output(&out).expect("tgt marker");
    assert_eq!(source, None);
    assert_eq!(translation, "Hola");
}

#[test]
fn split_polish_translate_empty_source_section_is_none() {
    let out = format!("{POLISH_TRANSLATE_SRC_MARKER}\n   \n{POLISH_TRANSLATE_TGT_MARKER}\nHi");
    let (source, translation) = split_polish_translate_output(&out).expect("tgt marker");
    assert_eq!(source, None);
    assert_eq!(translation, "Hi");
}

#[test]
fn build_polish_translate_prompt_contains_markers_and_target() {
    let p = build_polish_translate_system_prompt("日本語");
    assert!(p.contains(POLISH_TRANSLATE_SRC_MARKER));
    assert!(p.contains(POLISH_TRANSLATE_TGT_MARKER));
    assert!(p.contains("日本語"));
    // issue #609 F-02：合一路径以 translate_system_prompt 为 base，必须透传对抗式注入防御。
    assert!(
        p.contains("不可信用户文本"),
        "润色+翻译合一 prompt 必须带对抗式注入防御"
    );
}

#[tokio::test]
async fn hotkey_injection_gate_logs_pressed_and_cancels() {
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .is_test(false)
        .try_init();
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var("OPENLESS_HOTKEY_INJECTION_DRY_RUN", "1");

    let coordinator = Coordinator::new();
    coordinator.inject_hotkey_click_for_dev().await.unwrap();

    assert_eq!(coordinator.inner.state.lock().phase, SessionPhase::Idle);
    std::env::remove_var("OPENLESS_HOTKEY_INJECTION_DRY_RUN");
}

/// 复现并验证目标 2(a)：按下 Less Computer 键必须弹出可见胶囊。
/// 这里直接驱动 bridge 会调用的 handler，断言 begin_session 确实下发了可见胶囊。
#[tokio::test]
async fn less_computer_press_emits_visible_capsule() {
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var("OPENLESS_HOTKEY_INJECTION_DRY_RUN", "1");

    let coordinator = Coordinator::new();
    {
        let mut prefs = coordinator.inner.prefs.get();
        prefs.coding_agent_enabled = true;
        coordinator.inner.prefs.set(prefs).unwrap();
    }
    // 前置：还没弹过任何胶囊。
    assert!(coordinator.inner.last_capsule_state.lock().is_none());

    // 等价于「按下 Less Computer 键」：bridge_loop 收到 Pressed 后就是调这个 handler。
    super::handle_less_computer_pressed(&coordinator.inner).await;

    assert_eq!(
        *coordinator.inner.last_capsule_state.lock(),
        Some(CapsuleState::Recording),
        "按下 Less Computer 键必须进入录音并弹出可见胶囊"
    );
    std::env::remove_var("OPENLESS_HOTKEY_INJECTION_DRY_RUN");
}

#[tokio::test]
async fn begin_session_dry_run_enters_listening_and_clears_stale_edges() {
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var("OPENLESS_HOTKEY_INJECTION_DRY_RUN", "1");

    let coordinator = Coordinator::new();
    let old_session_id = coordinator.inner.state.lock().session_id;
    {
        let mut state = coordinator.inner.state.lock();
        state.pending_stop = true;
        state.cancelled = true;
    }

    coordinator.start_dictation().await.unwrap();

    let state = coordinator.inner.state.lock();
    assert_eq!(state.phase, SessionPhase::Listening);
    assert!(!state.pending_stop);
    assert!(!state.cancelled);
    assert_ne!(state.session_id, old_session_id);

    std::env::remove_var("OPENLESS_HOTKEY_INJECTION_DRY_RUN");
}

#[tokio::test]
async fn begin_session_ignores_non_idle_phase() {
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var("OPENLESS_HOTKEY_INJECTION_DRY_RUN", "1");

    let coordinator = Coordinator::new();
    let old_session_id = {
        let mut state = coordinator.inner.state.lock();
        state.phase = SessionPhase::Processing;
        state.session_id = session_id(99);
        state.session_id
    };

    coordinator.start_dictation().await.unwrap();

    let state = coordinator.inner.state.lock();
    assert_eq!(state.phase, SessionPhase::Processing);
    assert_eq!(state.session_id, old_session_id);

    std::env::remove_var("OPENLESS_HOTKEY_INJECTION_DRY_RUN");
}

#[test]
fn window_key_matcher_mirrors_windows_trigger_aliases() {
    let cases = [
        (HotkeyTrigger::RightControl, "Control", "ControlRight"),
        (HotkeyTrigger::LeftControl, "Control", "ControlLeft"),
        (HotkeyTrigger::RightOption, "Alt", "AltRight"),
        (HotkeyTrigger::RightAlt, "AltGraph", "AltRight"),
        (HotkeyTrigger::RightCommand, "Meta", "MetaRight"),
        (HotkeyTrigger::LeftOption, "Alt", "AltLeft"),
        // Mirrors Windows trigger_to_vk_code aliases.
        (HotkeyTrigger::Fn, "Control", "ControlRight"),
    ];
    for (trigger, key, code) in cases {
        assert!(
            window_key_matches_trigger(trigger, key, code),
            "{trigger:?} should match {key}/{code}"
        );
    }

    assert!(!window_key_matches_trigger(
        HotkeyTrigger::RightControl,
        "Control",
        "ControlLeft"
    ));
    assert!(!window_key_matches_trigger(
        HotkeyTrigger::LeftOption,
        "Alt",
        "AltRight"
    ));
    assert!(!window_key_matches_trigger(HotkeyTrigger::Fn, "Fn", "Fn"));
}

#[test]
fn windows_local_providers_are_keyless_and_not_whisper_compatible() {
    #[cfg(target_os = "windows")]
    assert!(is_keyless_local_asr_provider(
        crate::asr::local::foundry::PROVIDER_ID
    ));
    #[cfg(target_os = "windows")]
    assert!(is_keyless_local_asr_provider(
        crate::asr::local::sherpa::PROVIDER_ID
    ));
    #[cfg(not(target_os = "windows"))]
    assert!(!is_keyless_local_asr_provider(
        crate::asr::local::foundry::PROVIDER_ID
    ));
    #[cfg(not(target_os = "windows"))]
    assert!(!is_keyless_local_asr_provider(
        crate::asr::local::sherpa::PROVIDER_ID
    ));
    assert!(!is_whisper_compatible_provider(
        crate::asr::local::foundry::PROVIDER_ID
    ));
    assert!(!is_whisper_compatible_provider(
        crate::asr::local::sherpa::PROVIDER_ID
    ));
    assert!(!is_whisper_compatible_provider(
        crate::asr::mimo::PROVIDER_ID
    ));
}

#[test]
fn credential_gate_classifies_mimo_as_api_key_asr_provider() {
    assert_eq!(
        cloud_asr_credential_requirement(crate::asr::mimo::PROVIDER_ID),
        CloudAsrCredentialRequirement::AsrApiKey
    );
}

#[test]
fn verbose_json_enabled_only_for_whisper_family() {
    // verbose_json + 幻听过滤只对返回完整 Whisper 指标的 provider 开启。
    assert!(whisper_supports_verbose_json("whisper"));
    assert!(whisper_supports_verbose_json("groq"));
    // SiliconFlow(SenseVoice/TeleSpeech) / Zhipu(GLM-ASR) 保持旧的 json 行为。
    assert!(!whisper_supports_verbose_json("siliconflow"));
    assert!(!whisper_supports_verbose_json("zhipu"));
}

#[test]
fn openrouter_is_whisper_compatible_json_provider() {
    use crate::asr::whisper::AsrRequestFormat;
    // issue #582：OpenRouter 走 whisper 兼容路由，但请求体是 JSON+base64。
    assert!(is_whisper_compatible_provider("openrouter"));
    assert_eq!(
        whisper_request_format("openrouter"),
        AsrRequestFormat::OpenRouterJson
    );
    // 其余兼容厂商保持 multipart。
    assert_eq!(
        whisper_request_format("whisper"),
        AsrRequestFormat::Multipart
    );
    assert_eq!(whisper_request_format("groq"), AsrRequestFormat::Multipart);
    // OpenRouter 的 JSON 协议不吃 response_format，verbose_json 保持关闭。
    assert!(!whisper_supports_verbose_json("openrouter"));
    // base64 膨胀，长录音保守按 30s 切分。
    assert_eq!(batch_asr_chunk_limit_ms("openrouter"), Some(30_000));
}

#[test]
fn qa_asr_provider_kind_tracks_active_provider() {
    assert_eq!(
        active_asr_provider_kind(crate::asr::bailian::PROVIDER_ID),
        ActiveAsrProviderKind::Bailian
    );
    assert_eq!(
        active_asr_provider_kind("whisper"),
        ActiveAsrProviderKind::WhisperCompatible
    );
    assert_eq!(
        active_asr_provider_kind(crate::asr::mimo::PROVIDER_ID),
        ActiveAsrProviderKind::Mimo
    );
    assert_eq!(
        active_asr_provider_kind("volcengine"),
        ActiveAsrProviderKind::Volcengine
    );
}

#[cfg(target_os = "windows")]
#[test]
fn coordinator_shares_app_foundry_runtime() {
    let runtime = Arc::new(crate::asr::local::FoundryLocalRuntime::new());
    let coordinator = Coordinator::new_with_foundry_runtime(Arc::clone(&runtime));

    assert!(Arc::ptr_eq(
        &runtime,
        &coordinator.inner.foundry_local_runtime
    ));
}

#[cfg(target_os = "windows")]
#[test]
fn foundry_transcribe_skips_global_timeout_for_first_run_provisioning() {
    let provider = Arc::new(crate::asr::local::FoundryLocalWhisperAsr::new(
        Arc::new(crate::asr::local::FoundryLocalRuntime::new()),
        crate::asr::local::foundry::DEFAULT_MODEL_ALIAS.to_string(),
        "auto".to_string(),
        None,
    ));
    let active_asr = ActiveAsr::FoundryLocalWhisper(provider);

    assert!(!asr_transcribe_uses_global_timeout(&active_asr));
}

#[cfg(target_os = "windows")]
#[test]
fn foundry_audio_transcribe_timeout_is_separate_from_prepare() {
    let timeout = foundry_audio_transcribe_timeout_duration();

    assert_eq!(
        timeout,
        std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS)
    );
}

#[test]
fn local_qwen_timeout_floors_at_global_timeout_for_short_audio() {
    // 5s 录音：5 × 0.6 = 3, +10 = 13, max(15) = 15。短录音保留 15s 兜底。
    assert_eq!(
        local_qwen_transcribe_timeout(5.0),
        std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS)
    );
}

#[test]
fn local_qwen_timeout_scales_with_audio_duration() {
    // 60s 录音：60 × 0.6 = 36, +10 = 46s。覆盖 RTF ≈ 0.5 的边界。
    assert_eq!(
        local_qwen_transcribe_timeout(60.0),
        std::time::Duration::from_secs(46)
    );
}

#[test]
fn local_qwen_timeout_ceils_partial_seconds() {
    // 10.1s 录音：10.1 × 0.6 = 6.06, ceil = 7, +10 = 17, max(15) = 17。
    assert_eq!(
        local_qwen_transcribe_timeout(10.1),
        std::time::Duration::from_secs(17)
    );
}

#[test]
fn local_qwen_timeout_handles_zero_duration() {
    // 0 时长（空 buffer 边界）：0 × 0.6 = 0, +10 = 10, max(15) = 15。
    assert_eq!(
        local_qwen_transcribe_timeout(0.0),
        std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS)
    );
}

#[cfg(target_os = "windows")]
#[test]
fn foundry_release_uses_foundry_keep_loaded_preference() {
    let runtime = Arc::new(crate::asr::local::FoundryLocalRuntime::new());
    let coordinator = Coordinator::new_with_foundry_runtime(runtime);
    let mut prefs = coordinator.inner.prefs.get();
    prefs.local_asr_keep_loaded_secs = 3;
    prefs.foundry_local_asr_keep_loaded_secs = 7;
    coordinator.inner.prefs.set(prefs).unwrap();

    assert_eq!(foundry_local_asr_release_keep_secs(&coordinator.inner), 7);
}

#[cfg(target_os = "windows")]
#[test]
fn foundry_release_guard_rejects_stale_dictation_session() {
    let runtime = Arc::new(crate::asr::local::FoundryLocalRuntime::new());
    let coordinator = Coordinator::new_with_foundry_runtime(runtime);
    let old_session_id = coordinator.inner.state.lock().session_id;

    assert!(asr_release_session_is_current(
        &coordinator.inner,
        AsrReleaseSession::Dictation(old_session_id)
    ));

    coordinator.inner.state.lock().session_id = new_session_id();

    assert!(!asr_release_session_is_current(
        &coordinator.inner,
        AsrReleaseSession::Dictation(old_session_id)
    ));
}

#[cfg(target_os = "windows")]
#[test]
fn local_asr_release_guard_rejects_stale_qa_session() {
    let runtime = Arc::new(crate::asr::local::FoundryLocalRuntime::new());
    let coordinator = Coordinator::new_with_foundry_runtime(runtime);
    let old_session_id = coordinator.inner.qa_state.lock().session_id;

    assert!(asr_release_session_is_current(
        &coordinator.inner,
        AsrReleaseSession::Qa(old_session_id)
    ));

    coordinator.inner.qa_state.lock().session_id = new_session_id();

    assert!(!asr_release_session_is_current(
        &coordinator.inner,
        AsrReleaseSession::Qa(old_session_id)
    ));
}

#[test]
fn resolve_ark_endpoint_rejects_blank_key_without_custom_endpoint() {
    assert_eq!(
        resolve_ark_endpoint_with_policy("", None)
            .unwrap_err()
            .to_string(),
        "API Key 为空"
    );
}

#[test]
fn resolve_ark_endpoint_allows_blank_key_with_custom_endpoint() {
    let endpoint = resolve_ark_endpoint_with_policy(
        "",
        Some("https://example.com/v1/chat/completions".to_string()),
    )
    .unwrap();
    assert_eq!(endpoint, "https://example.com/v1/chat/completions");
}

// ───────── issue #609 F-01：SSRF endpoint 校验 ─────────

#[test]
fn validate_llm_endpoint_accepts_default_volces_https() {
    validate_llm_endpoint("https://ark.cn-beijing.volces.com/api/v3/chat/completions")
        .expect("default endpoint must pass");
}

#[test]
fn validate_llm_endpoint_accepts_public_hostname() {
    validate_llm_endpoint("https://api.example.com/v1/chat/completions")
        .expect("public https hostname must pass");
}

#[test]
fn validate_llm_endpoint_accepts_localhost_http() {
    validate_llm_endpoint("http://localhost:8080/v1").expect("localhost http allowed");
    validate_llm_endpoint("http://127.0.0.1:8080/v1").expect("127.0.0.1 http allowed");
}

#[test]
fn validate_llm_endpoint_rejects_metadata_host() {
    assert!(validate_llm_endpoint("http://metadata.google.internal/computeMetadata/v1").is_err());
    assert!(
        validate_llm_endpoint("http://169.254.169.254/latest/meta-data/iam/").is_err(),
        "AWS/GCP metadata IP must be rejected"
    );
}

#[test]
fn validate_llm_endpoint_rejects_link_local_ipv4() {
    // link-local 169.254/16（含云元数据网段）始终拒绝，http/https 都拒。
    assert!(validate_llm_endpoint("https://169.254.10.5/v1").is_err());
    assert!(validate_llm_endpoint("http://169.254.10.5/v1").is_err());
}

#[test]
fn validate_llm_endpoint_allows_rfc1918_private() {
    // F-01 放宽：RFC1918 私网（局域网自托管 ASR/LLM）放行 http 与 https。
    validate_llm_endpoint("http://10.0.0.5/v1").expect("10/8 LAN http allowed");
    validate_llm_endpoint("http://192.168.1.1/v1").expect("192.168/16 LAN http allowed");
    validate_llm_endpoint("http://172.16.0.1/v1").expect("172.16/12 LAN http allowed");
    validate_llm_endpoint("https://192.168.1.1/v1").expect("192.168/16 LAN https allowed");
}

#[test]
fn validate_llm_endpoint_rejects_cgnat() {
    // RFC6598 100.64.0.0/10 始终拒绝（http/https），与公网 100.128/9 区分。
    assert!(validate_llm_endpoint("https://100.64.0.1/v1").is_err());
    assert!(validate_llm_endpoint("http://100.64.0.1/v1").is_err());
    assert!(validate_llm_endpoint("https://100.127.255.254/v1").is_err());
    // 100.128.x 不在段内，是公网 → 需 https。
    validate_llm_endpoint("https://100.128.0.1/v1").expect("100.128/9 is public https");
    assert!(
        validate_llm_endpoint("http://100.128.0.1/v1").is_err(),
        "100.128/9 是公网，http 应拒绝"
    );
}

#[test]
fn validate_llm_endpoint_allows_ipv6_loopback() {
    // `::1` 与 localhost / 127.0.0.1 同属本地白名单，http 放行。
    validate_llm_endpoint("http://[::1]:8080/v1").expect("::1 loopback allowed");
}

#[test]
fn validate_llm_endpoint_allows_ipv6_ula() {
    // F-01 放宽：IPv6 ULA fc00::/7（含 fd00::/8）是局域网，放行 http 与 https。
    validate_llm_endpoint("http://[fc00::1]/v1").expect("fc00::/7 ULA http allowed");
    validate_llm_endpoint("https://[fc00::1]/v1").expect("fc00::/7 ULA https allowed");
    validate_llm_endpoint("http://[fd12:3456::1]/v1").expect("fd00::/8 ULA http allowed");
}

#[test]
fn validate_llm_endpoint_rejects_ipv6_link_local() {
    // fe80::/10 link-local 始终拒绝。
    assert!(validate_llm_endpoint("https://[fe80::1]/v1").is_err());
    assert!(validate_llm_endpoint("http://[fe80::1]/v1").is_err());
}

#[test]
fn validate_llm_endpoint_ipv4_mapped() {
    // ::ffff:192.168.1.1 等价 RFC1918 → 局域网，放行（F-01 放宽）。
    validate_llm_endpoint("http://[::ffff:192.168.1.1]/v1")
        .expect("IPv4-mapped RFC1918 LAN http allowed");
    // ::ffff:169.254.169.254 等价云元数据 link-local → 始终拒绝。
    assert!(
        validate_llm_endpoint("http://[::ffff:169.254.169.254]/v1").is_err(),
        "IPv4-mapped 元数据/link-local 必须拒绝"
    );
}

#[test]
fn validate_llm_endpoint_rejects_non_https_public() {
    // 外网主机走 http → 拒绝（仅 localhost / 局域网放行 http）。
    assert!(validate_llm_endpoint("http://api.example.com/v1").is_err());
    // 公网字面 IP 走 http → 拒绝；https → 放行。
    assert!(validate_llm_endpoint("http://8.8.8.8/v1").is_err());
    validate_llm_endpoint("https://8.8.8.8/v1").expect("public IP https allowed");
}

// ── issue #609 M-02：F-01 绕过变体（依赖 url crate 的 WHATWG host 归一化）──

#[test]
fn validate_llm_endpoint_normalizes_obfuscated_loopback_to_local() {
    // url crate 把十六进制/十进制点分与整数形式归一化为 127.0.0.1 = 本地白名单 → 接受。
    validate_llm_endpoint("http://0x7f.0.0.1/v1").expect("0x7f.0.0.1 规范化为 127.0.0.1，本地放行");
    validate_llm_endpoint("http://2130706433/v1").expect("2130706433 规范化为 127.0.0.1，本地放行");
}

#[test]
fn validate_llm_endpoint_userinfo_does_not_spoof_host() {
    // userinfo（user@）不参与 host 判定：host 取 169.254.169.254（元数据）→ 拒绝，
    // 不会被 userinfo 骗成「合法 user 名」而放行。
    assert!(
        validate_llm_endpoint("http://user@169.254.169.254/v1").is_err(),
        "userinfo 不应让元数据 host 绕过 SSRF 校验"
    );
    // 反向：host 取 192.168.1.1（局域网）→ 放行（F-01 放宽），userinfo 不影响判定。
    validate_llm_endpoint("http://user@192.168.1.1/v1")
        .expect("userinfo 不应影响局域网 host 的放行判定");
}

#[test]
fn validate_llm_endpoint_rejects_unspecified_and_broadcast_ipv4() {
    // 0.0.0.0 unspecified → 始终拒绝（http/https 都拒）。
    assert!(validate_llm_endpoint("http://0.0.0.0/v1").is_err());
    assert!(validate_llm_endpoint("https://0.0.0.0/v1").is_err());
    // 255.255.255.255 broadcast → 始终拒绝。
    assert!(validate_llm_endpoint("http://255.255.255.255/v1").is_err());
    assert!(validate_llm_endpoint("https://255.255.255.255/v1").is_err());
    // :: unspecified IPv6 → 始终拒绝。
    assert!(validate_llm_endpoint("http://[::]/v1").is_err());
}

// ── issue #609 H-01：Gemini base_url 也过 SSRF 校验 ──

#[test]
fn resolve_gemini_base_url_default_passes() {
    let url = resolve_gemini_base_url(None).expect("默认 Gemini endpoint 必须通过");
    assert_eq!(url, "https://generativelanguage.googleapis.com/v1beta");
}

#[test]
fn resolve_gemini_base_url_endpoint_policy() {
    // F-01 放宽：局域网（RFC1918）http 放行（用户局域网自托管 Gemini 兼容网关）。
    resolve_gemini_base_url(Some("http://192.168.1.1/v1beta".into()))
        .expect("LAN http Gemini endpoint 应放行");
    // 元数据 / 公网 http → 仍必须拒绝，防止带 Key 请求被指向高价值目标 / 明文外泄。
    assert!(resolve_gemini_base_url(Some("http://169.254.169.254/v1beta".into())).is_err());
    assert!(resolve_gemini_base_url(Some("http://api.example.com/v1beta".into())).is_err());
}

// ── issue #609 F-01 孪生 gap：运行期听写路径的 ASR endpoint 也过 SSRF 校验（fail-closed）──

#[test]
fn guard_asr_http_endpoint_passes_public_https() {
    // 公网 https / 默认官方 endpoint：原样返回。
    assert_eq!(
        guard_asr_http_endpoint("https://api.openai.com/v1".into(), "FALLBACK"),
        "https://api.openai.com/v1"
    );
    assert_eq!(
        guard_asr_http_endpoint(crate::asr::mimo::DEFAULT_ENDPOINT.into(), "FALLBACK"),
        crate::asr::mimo::DEFAULT_ENDPOINT
    );
}

#[test]
fn guard_asr_http_endpoint_passes_localhost_and_lan_http() {
    // 本地 Whisper 服务：localhost / 127.0.0.1 http 放行。
    assert_eq!(
        guard_asr_http_endpoint("http://localhost:9000/v1".into(), "FALLBACK"),
        "http://localhost:9000/v1"
    );
    assert_eq!(
        guard_asr_http_endpoint("http://127.0.0.1:9000/v1".into(), "FALLBACK"),
        "http://127.0.0.1:9000/v1"
    );
    // F-01 放宽：局域网（RFC1918）http ASR 网关原样放行，不再回退。
    assert_eq!(
        guard_asr_http_endpoint("http://192.168.1.50:9000/v1".into(), "FALLBACK"),
        "http://192.168.1.50:9000/v1"
    );
    assert_eq!(
        guard_asr_http_endpoint("http://10.0.0.5/v1".into(), "FALLBACK"),
        "http://10.0.0.5/v1"
    );
}

#[test]
fn guard_asr_http_endpoint_rejects_metadata_and_public_http_falls_back() {
    // 元数据 / 非 https 公网：fail-closed 回退到安全默认值，不把带 Key 的请求指向被拒地址。
    assert_eq!(
        guard_asr_http_endpoint("http://169.254.169.254/v1".into(), "FALLBACK"),
        "FALLBACK"
    );
    assert_eq!(
        guard_asr_http_endpoint("http://api.example.com/v1".into(), "FALLBACK"),
        "FALLBACK"
    );
    // CGNAT 始终拒绝。Whisper 无官方默认：回退空串 → transcription_url 解析失败、请求不发出。
    assert_eq!(
        guard_asr_http_endpoint("http://100.64.0.1/v1".into(), ""),
        ""
    );
}

#[test]
fn guard_asr_http_endpoint_empty_is_unchanged() {
    // 空 endpoint（未配置）原样透传，交由下游处理，不误报。
    assert_eq!(guard_asr_http_endpoint(String::new(), "FALLBACK"), "");
}

#[test]
fn deferred_asr_bridge_flushes_startup_audio_before_live_chunks() {
    #[derive(Default)]
    struct RecordingConsumer {
        bytes: Mutex<Vec<u8>>,
    }

    impl crate::asr::AudioConsumer for RecordingConsumer {
        fn consume_pcm_chunk(&self, pcm: &[u8]) {
            self.bytes.lock().extend_from_slice(pcm);
        }
    }

    let bridge = DeferredAsrBridge::new();
    crate::recorder::AudioConsumer::consume_pcm_chunk(&bridge, &[1, 2]);
    crate::recorder::AudioConsumer::consume_pcm_chunk(&bridge, &[3, 4]);

    let target = Arc::new(RecordingConsumer::default());
    let target_for_attach: Arc<dyn crate::asr::AudioConsumer> = target.clone();
    assert_eq!(bridge.attach(target_for_attach), 4);

    crate::recorder::AudioConsumer::consume_pcm_chunk(&bridge, &[5, 6]);
    assert_eq!(&*target.bytes.lock(), &[1, 2, 3, 4, 5, 6]);
}

#[tokio::test]
async fn manual_stop_during_starting_is_queued() {
    let coordinator = Coordinator::new();
    {
        let mut state = coordinator.inner.state.lock();
        state.phase = SessionPhase::Starting;
        state.pending_stop = false;
    }

    coordinator.stop_dictation().await.unwrap();

    let state = coordinator.inner.state.lock();
    assert_eq!(state.phase, SessionPhase::Starting);
    assert!(state.pending_stop);
}

#[tokio::test]
async fn stop_dictation_from_listening_without_asr_returns_idle() {
    let coordinator = Coordinator::new();
    {
        let mut state = coordinator.inner.state.lock();
        state.phase = SessionPhase::Listening;
        state.session_id = session_id(123);
    }

    coordinator.stop_dictation().await.unwrap();

    assert_eq!(coordinator.inner.state.lock().phase, SessionPhase::Idle);
}

#[test]
fn cancel_session_state_machine_is_table_driven() {
    let cases = [
        (SessionPhase::Idle, SessionPhase::Idle, false),
        (SessionPhase::Starting, SessionPhase::Idle, true),
        (SessionPhase::Listening, SessionPhase::Idle, true),
        (SessionPhase::Processing, SessionPhase::Processing, true),
        (SessionPhase::Inserting, SessionPhase::Inserting, false),
    ];

    for (initial, expected_phase, expected_cancelled) in cases {
        let coordinator = Coordinator::new();
        {
            let mut state = coordinator.inner.state.lock();
            state.phase = initial;
            state.cancelled = false;
            state.focus_target = Some(1);
        }

        coordinator.cancel_dictation();

        let state = coordinator.inner.state.lock();
        assert_eq!(state.phase, expected_phase, "initial={initial:?}");
        assert_eq!(state.cancelled, expected_cancelled, "initial={initial:?}");
        if matches!(initial, SessionPhase::Starting | SessionPhase::Listening) {
            assert!(state.focus_target.is_none(), "initial={initial:?}");
        }
    }
}

#[test]
fn recorder_runtime_error_aborts_active_session() {
    let coordinator = Coordinator::new();
    {
        let mut state = coordinator.inner.state.lock();
        state.phase = SessionPhase::Listening;
        state.cancelled = false;
    }

    abort_recording_with_error(&coordinator.inner, "录音中断: stream failed".to_string());

    let state = coordinator.inner.state.lock();
    assert_eq!(state.phase, SessionPhase::Idle);
    assert!(state.cancelled);
    assert!(coordinator.inner.recorder.lock().is_none());
    assert!(coordinator.inner.asr.lock().is_none());
}

#[test]
fn abort_recording_keeps_session_non_idle_until_restore_can_run() {
    let mut state = SessionState::default();
    state.phase = SessionPhase::Listening;
    state.cancelled = false;
    state.session_id = session_id(7);

    let abort = begin_recording_abort_before_restore(&mut state).unwrap();

    assert_eq!(abort.session_id, session_id(7));
    assert!(state.cancelled);
    assert_eq!(state.phase, SessionPhase::Listening);

    publish_abort_idle_after_restore(&mut state, abort.session_id);

    assert_eq!(state.phase, SessionPhase::Idle);
}

#[tokio::test]
async fn pressed_edge_during_inserting_does_not_start_new_session() {
    let coordinator = Coordinator::new();
    {
        let mut state = coordinator.inner.state.lock();
        state.phase = SessionPhase::Inserting;
        state.session_id = session_id(41);
    }

    handle_pressed_edge(&coordinator.inner).await;

    let state = coordinator.inner.state.lock();
    assert_eq!(state.phase, SessionPhase::Inserting);
    assert_eq!(state.session_id, session_id(41));
}

#[tokio::test]
async fn repeated_pressed_edge_during_hold_session_does_not_restart() {
    let coordinator = Coordinator::new();
    coordinator
        .inner
        .prefs
        .set(crate::types::UserPreferences {
            hotkey: crate::types::HotkeyBinding {
                trigger: HotkeyTrigger::RightControl,
                mode: HotkeyMode::Hold,
                keys: None,
            },
            ..Default::default()
        })
        .unwrap();
    coordinator.inner.state.lock().phase = SessionPhase::Listening;
    coordinator
        .inner
        .hotkey_trigger_held
        .store(true, Ordering::SeqCst);

    handle_pressed_edge(&coordinator.inner).await;

    assert_eq!(
        coordinator.inner.state.lock().phase,
        SessionPhase::Listening
    );
    assert!(coordinator.inner.hotkey_trigger_held.load(Ordering::SeqCst));
}

#[test]
fn enabling_shortcut_recording_clears_dictation_hold_latch() {
    let coordinator = Coordinator::new();
    coordinator
        .inner
        .hotkey_trigger_held
        .store(true, Ordering::SeqCst);

    coordinator.set_shortcut_recording_active(true);

    assert!(!coordinator.inner.hotkey_trigger_held.load(Ordering::SeqCst));
}

#[test]
fn window_hotkey_fallback_is_disabled_when_no_explicit_fallback_is_advertised() {
    assert_eq!(
        window_hotkey_fallback_enabled(),
        crate::types::HotkeyCapability::current().explicit_fallback_available
    );
}

#[test]
fn capsule_ignore_cursor_only_in_non_interactive_states() {
    // issue #631：有 ✕/✓ 按钮的三个状态必须可点；终态/空闲（含 toast 停留与
    // 离场动画期间）点击穿透，避免误触激活 OpenLess 弹出主界面。
    assert!(!capsule_ignore_cursor_for_state(CapsuleState::Recording));
    assert!(!capsule_ignore_cursor_for_state(CapsuleState::Transcribing));
    assert!(!capsule_ignore_cursor_for_state(CapsuleState::Polishing));
    assert!(capsule_ignore_cursor_for_state(CapsuleState::Done));
    assert!(capsule_ignore_cursor_for_state(CapsuleState::Cancelled));
    assert!(capsule_ignore_cursor_for_state(CapsuleState::Error));
    assert!(capsule_ignore_cursor_for_state(CapsuleState::Idle));
}

#[test]
fn capsule_show_strategy_matches_platform_activation_contract() {
    // 平台列表必须与 capsule_show_strategy_for_platform 的 cfg 完全一致：
    // 改实现里的 #[cfg] 时，一并改这两个 #[cfg]，否则 Linux CI 直接红
    // （fcitx5 PR #451 把 Linux 加进 NoActivate 但漏改本测试，CI 失败）。
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    assert_eq!(
        capsule_show_strategy_for_platform(),
        CapsuleShowStrategy::NoActivate
    );

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    assert_eq!(
        capsule_show_strategy_for_platform(),
        CapsuleShowStrategy::FallbackShow
    );
}

#[test]
#[cfg(target_os = "windows")]
fn prepared_windows_ime_slot_is_taken_only_for_matching_session() {
    let mut slots = vec![PreparedWindowsImeSessionSlot {
        session_id: session_id(2),
        prepared: PreparedWindowsImeSession::unavailable(),
    }];

    assert!(take_matching_prepared_windows_ime_session(&mut slots, session_id(1)).is_none());
    assert_eq!(
        slots.iter().map(|slot| slot.session_id).collect::<Vec<_>>(),
        vec![session_id(2)]
    );

    assert!(take_matching_prepared_windows_ime_session(&mut slots, session_id(2)).is_some());
    assert!(slots.is_empty());
}

#[test]
#[cfg(target_os = "windows")]
fn prepared_windows_ime_sessions_keep_overlapping_snapshots() {
    let mut slots = Vec::new();
    store_prepared_windows_ime_session(
        &mut slots,
        session_id(1),
        PreparedWindowsImeSession::unavailable(),
    );
    store_prepared_windows_ime_session(
        &mut slots,
        session_id(2),
        PreparedWindowsImeSession::unavailable(),
    );

    assert_eq!(
        slots.iter().map(|slot| slot.session_id).collect::<Vec<_>>(),
        vec![session_id(1), session_id(2)]
    );

    assert!(take_matching_prepared_windows_ime_session(&mut slots, session_id(1)).is_some());
    assert_eq!(
        slots.iter().map(|slot| slot.session_id).collect::<Vec<_>>(),
        vec![session_id(2)]
    );
}

#[test]
#[cfg(target_os = "windows")]
fn stale_prepared_windows_ime_restore_discards_old_snapshot_without_restoring() {
    let mut slots = Vec::new();
    store_prepared_windows_ime_session(
        &mut slots,
        session_id(1),
        PreparedWindowsImeSession::unavailable(),
    );
    store_prepared_windows_ime_session(
        &mut slots,
        session_id(2),
        PreparedWindowsImeSession::unavailable(),
    );

    assert!(take_current_prepared_windows_ime_session_for_restore(
        &mut slots,
        session_id(1),
        session_id(2)
    )
    .is_none());
    assert_eq!(
        slots.iter().map(|slot| slot.session_id).collect::<Vec<_>>(),
        vec![session_id(2)]
    );
}

#[test]
#[cfg(target_os = "windows")]
fn non_tsf_insertion_fallback_gate_blocks_only_when_disabled() {
    assert!(should_try_non_tsf_insertion_fallback(
        true,
        InsertStatus::CopiedFallback
    ));
    assert!(should_try_non_tsf_insertion_fallback(
        true,
        InsertStatus::Failed
    ));
    assert!(!should_try_non_tsf_insertion_fallback(
        true,
        InsertStatus::Inserted
    ));
    assert!(!should_try_non_tsf_insertion_fallback(
        false,
        InsertStatus::CopiedFallback
    ));
    assert!(!should_try_non_tsf_insertion_fallback(
        false,
        InsertStatus::Failed
    ));
}

#[test]
fn focus_restore_failure_uses_specific_error_code_when_insert_fails() {
    assert_eq!(
        dictation_error_code(InsertStatus::Failed, false, false, false),
        Some("focusRestoreFailed")
    );
}

#[test]
#[cfg(target_os = "windows")]
fn missing_windows_hwnd_is_not_present() {
    use windows::Win32::Foundation::HWND;

    assert!(!windows_hwnd_is_present(HWND::default()));
}

#[test]
#[cfg(target_os = "windows")]
fn tsf_required_failure_keeps_tsf_error_when_focus_was_ready() {
    assert_eq!(
        dictation_error_code(InsertStatus::Failed, false, true, false),
        Some("windowsImeTsfRequired")
    );
}

#[test]
fn startup_race_check_treats_newer_session_as_stale() {
    let mut state = SessionState::default();
    state.phase = SessionPhase::Starting;
    state.cancelled = false;
    state.session_id = session_id(2);

    assert_eq!(
        startup_race_status(&state, session_id(1)),
        StartupRaceStatus::StaleContinuation
    );
}

#[test]
fn startup_race_check_is_table_driven_for_begin_session_edges() {
    let cases = [
        (
            SessionPhase::Starting,
            false,
            session_id(7),
            StartupRaceStatus::ActiveStarting,
        ),
        (
            SessionPhase::Starting,
            true,
            session_id(7),
            StartupRaceStatus::CancelRaced,
        ),
        (
            SessionPhase::Idle,
            false,
            session_id(7),
            StartupRaceStatus::CancelRaced,
        ),
        (
            SessionPhase::Listening,
            false,
            session_id(7),
            StartupRaceStatus::CancelRaced,
        ),
        (
            SessionPhase::Starting,
            false,
            session_id(8),
            StartupRaceStatus::StaleContinuation,
        ),
    ];

    for (phase, cancelled, actual_session_id, expected) in cases {
        let mut state = SessionState::default();
        state.phase = phase;
        state.cancelled = cancelled;
        state.session_id = actual_session_id;

        assert_eq!(
            startup_race_status(&state, session_id(7)),
            expected,
            "phase={phase:?} cancelled={cancelled} actual_session={actual_session_id}"
        );
    }
}

#[test]
fn begin_recording_abort_is_noop_after_prior_cancel_or_idle() {
    let cases = [
        (SessionPhase::Idle, false),
        (SessionPhase::Processing, false),
        (SessionPhase::Listening, true),
    ];

    for (phase, cancelled) in cases {
        let mut state = SessionState::default();
        state.phase = phase;
        state.cancelled = cancelled;

        assert!(begin_recording_abort_before_restore(&mut state).is_none());
        assert_eq!(state.phase, phase);
        assert_eq!(state.cancelled, cancelled);
    }
}

#[test]
fn stale_startup_cleanup_keeps_newer_asr_resource() {
    let coordinator = Coordinator::new();
    let newer_asr = Arc::new(WhisperBatchASR::new(
        "key".to_string(),
        "http://localhost".to_string(),
        "model".to_string(),
        None,
        None,
        false,
    ));
    *coordinator.inner.asr.lock() = Some(SessionResource::new(
        session_id(2),
        ActiveAsr::Whisper(Arc::clone(&newer_asr)),
    ));

    discard_startup_resources_for_session(&coordinator.inner, session_id(1));

    assert_eq!(
        coordinator
            .inner
            .asr
            .lock()
            .as_ref()
            .map(|resource| resource.session_id),
        Some(session_id(2))
    );

    discard_startup_resources_for_session(&coordinator.inner, session_id(2));

    assert!(coordinator.inner.asr.lock().is_none());
}
