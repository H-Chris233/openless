//! ASR 引擎构建与生命周期：麦克风/凭证前置校验、本地模型释放调度（Foundry/sherpa/
//! local-qwen3）、QA-ASR provider 构建（QaAsrStart）、全局超时常量与各 provider 超时，
//! 以及音频线程驱动的 DeferredAsrBridge。
//!
//! 从 `coordinator.rs` 机械拆出（行为保持，仅可见性提升）。schedule_*_release 的
//! session-staleness 守卫与 DeferredAsrBridge 的 attach/flush 交错是并发敏感路径，只搬不改。
//! ActiveAsr/ActiveAsrProviderKind 及其分类器留在父模块（Inner 字段类型）。

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloudAsrCredentialRequirement {
    AsrApiKey,
    Volcengine,
}

pub(crate) fn cloud_asr_credential_requirement(
    active_asr: &str,
) -> CloudAsrCredentialRequirement {
    if is_whisper_compatible_provider(active_asr)
        || is_bailian_provider(active_asr)
        || is_mimo_provider(active_asr)
    {
        CloudAsrCredentialRequirement::AsrApiKey
    } else {
        CloudAsrCredentialRequirement::Volcengine
    }
}

pub(crate) fn ensure_microphone_permission(_inner: &Arc<Inner>) -> Result<(), String> {
    use crate::permissions::{self, PermissionStatus};

    #[cfg(target_os = "windows")]
    {
        if permissions::windows_microphone_access_explicitly_denied() {
            return Err("需要麦克风权限，当前状态: Denied".to_string());
        }
        return Ok(());
    }

    let status = permissions::check_microphone();
    if matches!(
        status,
        PermissionStatus::Granted | PermissionStatus::NotApplicable
    ) {
        return Ok(());
    }

    // 听写路径不抢前台焦点：缺 mic 权限时直接请求系统授权，不再先 show_main_window。
    // 用户在设置页手动点“请求权限”仍走 request_microphone_from_foreground，那是显式操作。
    // 这里若系统不弹框，后续会通过 capsule error 引导用户主动去权限页处理。详见 #166。
    let requested = permissions::request_microphone();
    if matches!(
        requested,
        PermissionStatus::Granted | PermissionStatus::NotApplicable
    ) {
        Ok(())
    } else {
        Err(format!("需要麦克风权限，当前状态: {requested:?}"))
    }
}

pub(crate) fn ensure_asr_credentials() -> Result<(), String> {
    let active_asr = CredentialsVault::get_active_asr();

    // 本地 Qwen3-ASR 没有"凭据"概念，但需要：(a) macOS 平台 (b) 模型已下载。
    if crate::asr::local::is_local_qwen3(&active_asr) {
        #[cfg(not(target_os = "macos"))]
        {
            return Err("本地 ASR 当前仅支持 macOS（Windows 见 issue #256）".to_string());
        }
        #[cfg(target_os = "macos")]
        {
            return ensure_local_qwen3_model_ready();
        }
    }

    // Apple Speech 没有"凭据"也没有要下载的模型，只需：macOS 平台。
    // 系统语音识别资源由 OS 管理，首次使用时弹授权框（见 apple_speech_provider）。
    if crate::asr::local::is_apple_speech(&active_asr) {
        #[cfg(not(target_os = "macos"))]
        {
            return Err("Apple Speech 语音识别仅支持 macOS".to_string());
        }
        #[cfg(target_os = "macos")]
        {
            return Ok(());
        }
    }

    if crate::asr::local::foundry::is_foundry_local_whisper(&active_asr) {
        #[cfg(not(target_os = "windows"))]
        {
            return Err("Foundry Local Whisper 当前仅支持 Windows".to_string());
        }
        #[cfg(target_os = "windows")]
        {
            return Ok(());
        }
    }

    if crate::asr::local::sherpa::is_sherpa_onnx_local(&active_asr) {
        #[cfg(not(target_os = "windows"))]
        {
            return Err("sherpa-onnx local ASR 当前仅支持 Windows".to_string());
        }
        #[cfg(target_os = "windows")]
        {
            return Ok(());
        }
    }

    match cloud_asr_credential_requirement(&active_asr) {
        CloudAsrCredentialRequirement::AsrApiKey => {
            let api_key = CredentialsVault::get(CredentialAccount::AsrApiKey)
                .ok()
                .flatten()
                .unwrap_or_default();
            if api_key.trim().is_empty() {
                return Err("请先在设置中填写 ASR 服务商 API Key".to_string());
            }
            Ok(())
        }
        CloudAsrCredentialRequirement::Volcengine => {
            let creds = read_volc_credentials();
            if creds.app_id.trim().is_empty() || creds.access_token.trim().is_empty() {
                Err("请先在设置中填写火山引擎 ASR App Key 和 Access Key".to_string())
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn is_keyless_local_asr_provider(id: &str) -> bool {
    if crate::asr::local::is_local_qwen3(id) {
        return true;
    }
    #[cfg(target_os = "macos")]
    if crate::asr::local::is_apple_speech(id) {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        crate::asr::local::foundry::is_foundry_local_whisper(id)
            || crate::asr::local::sherpa::is_sherpa_onnx_local(id)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = id;
        false
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn ensure_local_qwen3_model_ready() -> Result<(), String> {
    let prefs = || -> Result<crate::types::UserPreferences, String> {
        // 这里没法拿到 inner，直接读 preferences.json 即可（Coordinator 写盘后总是同步的）。
        crate::persistence::PreferencesStore::new()
            .map_err(|e| e.to_string())
            .map(|s| s.get())
    }()?;
    let model_id = crate::asr::local::ModelId::from_str(&prefs.local_asr_active_model)
        .ok_or_else(|| format!("未知的本地模型 id: {}", prefs.local_asr_active_model))?;
    if !crate::asr::local::models::is_downloaded(model_id) {
        return Err(format!(
            "本地模型 {} 未下载完整，请到 设置 → 模型设置 中下载",
            model_id.as_str()
        ));
    }
    Ok(())
}

/// 一次 dictation 结束后，按 prefs.local_asr_keep_loaded_secs 决定何时释放
/// 内存里的 Qwen3-ASR 引擎。0 = 立即释放；其它值 = sleep N 秒后看 last_used。
/// 多次会话叠加多个 sleep 任务，每个独立 check：只要中间又被使用过就跳过释放。
pub(crate) fn schedule_local_asr_release(inner: &Arc<Inner>) {
    let keep_secs = inner.prefs.get().local_asr_keep_loaded_secs;
    let cache = Arc::clone(&inner.local_asr_cache);
    if keep_secs == 0 {
        cache.release_now();
        return;
    }
    let dur = std::time::Duration::from_secs(keep_secs as u64);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(dur).await;
        cache.release_if_idle(dur);
    });
}

#[cfg(target_os = "windows")]
pub(crate) fn foundry_local_asr_release_keep_secs(inner: &Arc<Inner>) -> u32 {
    inner.prefs.get().foundry_local_asr_keep_loaded_secs
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
pub(crate) enum AsrReleaseSession {
    Dictation(SessionId),
    Qa(SessionId),
}

#[cfg(target_os = "windows")]
pub(crate) fn asr_release_session_is_current(
    inner: &Arc<Inner>,
    session: AsrReleaseSession,
) -> bool {
    match session {
        AsrReleaseSession::Dictation(session_id) => inner.state.lock().session_id == session_id,
        AsrReleaseSession::Qa(session_id) => inner.qa_state.lock().session_id == session_id,
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn schedule_foundry_local_asr_release(inner: &Arc<Inner>, session: AsrReleaseSession) {
    let keep_secs = foundry_local_asr_release_keep_secs(inner);
    let runtime = Arc::clone(&inner.foundry_local_runtime);
    let inner = Arc::clone(inner);
    tauri::async_runtime::spawn(async move {
        if keep_secs > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(keep_secs as u64)).await;
        }
        if !asr_release_session_is_current(&inner, session) {
            return;
        }
        if let Err(error) = runtime.release_now().await {
            log::warn!("[foundry-asr] scheduled release failed: {error:#}");
        }
    });
}

#[cfg(target_os = "windows")]
pub(crate) fn sherpa_onnx_release_keep_secs(inner: &Arc<Inner>) -> u32 {
    inner.prefs.get().sherpa_onnx_keep_loaded_secs
}

/// 与 `schedule_foundry_local_asr_release` 同形：session_id 老旧则不释放，
/// 避免下一轮 session 立即重加载同一个 offline batch 模型。
#[cfg(target_os = "windows")]
pub(crate) fn schedule_sherpa_onnx_release(inner: &Arc<Inner>, session: AsrReleaseSession) {
    let keep_secs = sherpa_onnx_release_keep_secs(inner);
    let runtime = Arc::clone(&inner.sherpa_onnx_runtime);
    let inner = Arc::clone(inner);
    tauri::async_runtime::spawn(async move {
        if keep_secs > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(keep_secs as u64)).await;
        }
        if !asr_release_session_is_current(&inner, session) {
            return;
        }
        if let Err(error) = runtime.release_now().await {
            log::warn!("[sherpa-asr] scheduled release failed: {error:#}");
        }
    });
}

#[cfg(target_os = "macos")]
pub(crate) async fn build_local_qwen3(
    inner: &Arc<Inner>,
) -> anyhow::Result<Arc<crate::asr::local::LocalQwenAsr>> {
    let prefs = inner.prefs.get();
    let model_id = crate::asr::local::ModelId::from_str(&prefs.local_asr_active_model)
        .ok_or_else(|| anyhow::anyhow!("未知本地模型 id: {}", prefs.local_asr_active_model))?;
    let dir = crate::asr::local::models::model_dir(model_id)?;
    let app = inner
        .app
        .lock()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("AppHandle 未绑定"))?;
    // 走缓存：如果已有同 id 的引擎在内存里就直接复用，避免每次会话都重加载
    // 1.2GB+ 模型。第一次加载阻塞数秒，spawn_blocking 不卡 tokio runtime。
    let cache = Arc::clone(&inner.local_asr_cache);
    let mid = model_id.as_str().to_string();
    let engine = tauri::async_runtime::spawn_blocking(move || cache.get_or_load(&mid, &dir))
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking join failed: {e:#}"))??;
    Ok(Arc::new(crate::asr::local::LocalQwenAsr::new(app, engine)))
}

/// 构建 Apple Speech provider。与 build_local_qwen3 不同：无模型、无缓存、无
/// AppHandle 依赖，授权/识别由 provider 内部按需处理（首次弹系统授权框）。
#[cfg(target_os = "macos")]
pub(crate) fn build_apple_speech() -> Arc<crate::asr::local::AppleSpeechAsr> {
    Arc::new(crate::asr::local::AppleSpeechAsr::new())
}

pub(crate) enum QaAsrStart {
    Volcengine {
        asr: Arc<VolcengineStreamingASR>,
        bridge: Arc<DeferredAsrBridge>,
    },
    Bailian {
        asr: Arc<BailianRealtimeASR>,
        bridge: Arc<DeferredAsrBridge>,
    },
    Ready {
        active: ActiveAsr,
        consumer: Arc<dyn crate::recorder::AudioConsumer>,
    },
}

impl QaAsrStart {
    pub(crate) fn active_asr(&self) -> ActiveAsr {
        match self {
            QaAsrStart::Volcengine { asr, .. } => ActiveAsr::Volcengine(Arc::clone(asr)),
            QaAsrStart::Bailian { asr, .. } => ActiveAsr::Bailian(Arc::clone(asr)),
            QaAsrStart::Ready { active, .. } => active.clone(),
        }
    }

    pub(crate) fn recorder_consumer(&self) -> Arc<dyn crate::recorder::AudioConsumer> {
        match self {
            QaAsrStart::Volcengine { bridge, .. } => Arc::clone(bridge) as _,
            QaAsrStart::Bailian { bridge, .. } => Arc::clone(bridge) as _,
            QaAsrStart::Ready { consumer, .. } => Arc::clone(consumer),
        }
    }

    pub(crate) async fn open_streaming_session(&self) -> Result<(), String> {
        match self {
            QaAsrStart::Volcengine { asr, bridge } => {
                asr.open_session().await.map_err(|e| e.to_string())?;
                let target: Arc<dyn crate::asr::AudioConsumer> = Arc::clone(asr) as _;
                let flushed = bridge.attach(target);
                log::info!("[coord] QA ASR connected; flushed {flushed} deferred audio bytes");
                Ok(())
            }
            QaAsrStart::Bailian { asr, bridge } => {
                asr.open_session().await.map_err(|e| e.to_string())?;
                let target: Arc<dyn crate::asr::AudioConsumer> = Arc::clone(asr) as _;
                let flushed = bridge.attach(target);
                log::info!(
                    "[coord] QA Bailian ASR connected; flushed {flushed} deferred audio bytes"
                );
                Ok(())
            }
            QaAsrStart::Ready { .. } => Ok(()),
        }
    }
}

pub(crate) async fn build_qa_asr_start(
    inner: &Arc<Inner>,
    active_asr: &str,
) -> Result<QaAsrStart, String> {
    #[cfg(target_os = "windows")]
    if foundry::is_foundry_local_whisper(active_asr) {
        let prefs = inner.prefs.get();
        let model_alias = if foundry::model_alias_is_known(&prefs.foundry_local_asr_model) {
            prefs.foundry_local_asr_model.clone()
        } else {
            foundry::DEFAULT_MODEL_ALIAS.to_string()
        };
        let language_hint = prefs.foundry_local_asr_language_hint.trim().to_string();
        let language_hint = if language_hint.is_empty() {
            None
        } else {
            Some(language_hint)
        };
        let local = Arc::new(FoundryLocalWhisperAsr::new(
            Arc::clone(&inner.foundry_local_runtime),
            model_alias,
            prefs.foundry_local_runtime_source.clone(),
            language_hint,
        ));
        let active = ActiveAsr::FoundryLocalWhisper(Arc::clone(&local));
        let consumer: Arc<dyn crate::recorder::AudioConsumer> = local;
        return Ok(QaAsrStart::Ready { active, consumer });
    }

    #[cfg(target_os = "windows")]
    if sherpa::is_sherpa_onnx_local(active_asr) {
        let prefs = inner.prefs.get();
        let model_alias = if sherpa::model_alias_is_known(&prefs.sherpa_onnx_model) {
            prefs.sherpa_onnx_model.clone()
        } else {
            sherpa::DEFAULT_MODEL_ALIAS.to_string()
        };
        let language_hint = prefs.sherpa_onnx_language_hint.trim().to_string();
        let language_hint = if language_hint.is_empty() {
            None
        } else {
            Some(language_hint)
        };
        let token_handler = inner.app.lock().clone().map(|app| {
            Arc::new(move |piece: String| {
                if let Err(error) = app.emit("local-asr-token", piece) {
                    log::warn!("[sherpa-asr] emit token failed: {error}");
                }
            }) as crate::asr::local::sherpa_provider::SherpaTokenHandler
        });
        let local = SherpaOnnxAsr::new_for_model(
            Arc::clone(&inner.sherpa_onnx_runtime),
            model_alias,
            language_hint,
            token_handler,
        )
        .await
        .map_err(|e| format!("sherpa-onnx init failed: {e}"))?;
        let local = Arc::new(local);
        let active = ActiveAsr::SherpaOnnxLocal(Arc::clone(&local));
        let consumer: Arc<dyn crate::recorder::AudioConsumer> = local;
        return Ok(QaAsrStart::Ready { active, consumer });
    }

    #[cfg(target_os = "macos")]
    if crate::asr::local::is_local_qwen3(active_asr) {
        let local = build_local_qwen3(inner)
            .await
            .map_err(|e| format!("local ASR init failed: {e}"))?;
        let active = ActiveAsr::Local(Arc::clone(&local));
        let consumer: Arc<dyn crate::recorder::AudioConsumer> = local;
        return Ok(QaAsrStart::Ready { active, consumer });
    }

    #[cfg(target_os = "macos")]
    if crate::asr::local::is_apple_speech(active_asr) {
        let local = build_apple_speech();
        let active = ActiveAsr::AppleSpeech(Arc::clone(&local));
        let consumer: Arc<dyn crate::recorder::AudioConsumer> = local;
        return Ok(QaAsrStart::Ready { active, consumer });
    }

    match active_asr_provider_kind(active_asr) {
        ActiveAsrProviderKind::Bailian => Ok(QaAsrStart::Bailian {
            asr: Arc::new(BailianRealtimeASR::new(read_bailian_credentials())),
            bridge: Arc::new(DeferredAsrBridge::new()),
        }),
        ActiveAsrProviderKind::Mimo => {
            let (api_key, base_url, model) = read_mimo_credentials();
            let mimo = Arc::new(MimoBatchASR::new(api_key, base_url, model));
            let active = ActiveAsr::Mimo(Arc::clone(&mimo));
            let consumer: Arc<dyn crate::recorder::AudioConsumer> = mimo;
            Ok(QaAsrStart::Ready { active, consumer })
        }
        ActiveAsrProviderKind::WhisperCompatible => {
            let (api_key, base_url, model) = read_whisper_credentials();
            let whisper_prompt =
                crate::asr::whisper::build_prompt_from_phrases(&enabled_phrases(inner));
            let whisper = Arc::new(
                WhisperBatchASR::new(
                    api_key,
                    base_url,
                    model,
                    whisper_prompt,
                    batch_asr_chunk_limit_ms(active_asr),
                    whisper_supports_verbose_json(active_asr),
                )
                .with_request_format(whisper_request_format(active_asr)),
            );
            let active = ActiveAsr::Whisper(Arc::clone(&whisper));
            let consumer: Arc<dyn crate::recorder::AudioConsumer> = whisper;
            Ok(QaAsrStart::Ready { active, consumer })
        }
        ActiveAsrProviderKind::Volcengine => Ok(QaAsrStart::Volcengine {
            asr: Arc::new(VolcengineStreamingASR::new(
                read_volc_credentials(),
                enabled_hotwords(inner),
            )),
            bridge: Arc::new(DeferredAsrBridge::new()),
        }),
    }
}

/// Coordinator 全局超时保护：防止 ASR await_final_result() 永远挂起。
/// 设置为 15 秒（比 ASR 的 12 秒 FINAL_RESULT_TIMEOUT 稍长），
/// 只在 ASR 超时机制失效时作为最后的防线触发。
pub(crate) const COORDINATOR_GLOBAL_TIMEOUT_SECS: u64 = 15;

#[cfg(target_os = "windows")]
pub(crate) fn foundry_audio_transcribe_timeout_duration() -> std::time::Duration {
    std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS)
}

/// 本地 Qwen3-ASR 的动态转写超时。固定 15 秒在长录音（≥ 30s）+ 慢机器
/// （RTF ≈ 0.3–0.5）上必然超时把整段内容丢掉。改用 max(15, ceil(audio_s
/// × 0.6) + 10)：基础保留 15s 兜住短录音；长录音按音频长度的 0.6 倍 +
/// 10s 余量，覆盖 RTF ≤ 0.5 的机器。
pub(crate) fn local_qwen_transcribe_timeout(audio_secs: f64) -> std::time::Duration {
    let secs = ((audio_secs * 0.6).ceil() as u64)
        .saturating_add(10)
        .max(COORDINATOR_GLOBAL_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

/// sherpa-onnx offline batch 暂与 Foundry 同档；后续按 Windows 真机 CPU/模型
/// 实测结果再调整。
#[cfg(target_os = "windows")]
pub(crate) fn sherpa_audio_transcribe_timeout_duration() -> std::time::Duration {
    std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS)
}

// ─────────────────────────── audio bridge ───────────────────────────

pub(crate) struct DeferredAsrBridge {
    state: Mutex<DeferredAsrState>,
}

pub(crate) struct DeferredAsrState {
    target: Option<Arc<dyn crate::asr::AudioConsumer>>,
    pending_audio: Vec<u8>,
    attaching: bool,
}

impl DeferredAsrBridge {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(DeferredAsrState {
                target: None,
                pending_audio: Vec::new(),
                attaching: false,
            }),
        }
    }

    pub(crate) fn attach(&self, target: Arc<dyn crate::asr::AudioConsumer>) -> usize {
        let mut flushed_bytes = 0;
        {
            let mut state = self.state.lock();
            state.attaching = true;
        }

        loop {
            let pending = {
                let mut state = self.state.lock();
                if state.pending_audio.is_empty() {
                    state.target = Some(Arc::clone(&target));
                    state.attaching = false;
                    return flushed_bytes;
                }
                std::mem::take(&mut state.pending_audio)
            };
            flushed_bytes += pending.len();
            target.consume_pcm_chunk(&pending);
        }
    }
}

impl crate::recorder::AudioConsumer for DeferredAsrBridge {
    fn consume_pcm_chunk(&self, pcm: &[u8]) {
        let target = {
            let mut state = self.state.lock();
            if state.attaching {
                state.pending_audio.extend_from_slice(pcm);
                return;
            }
            if let Some(target) = state.target.as_ref() {
                Some(Arc::clone(target))
            } else {
                state.pending_audio.extend_from_slice(pcm);
                None
            }
        };

        if let Some(target) = target {
            target.consume_pcm_chunk(pcm);
        }
    }
}
