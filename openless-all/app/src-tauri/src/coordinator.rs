#![cfg_attr(
    target_os = "linux",
    allow(dead_code, unused_imports, unused_variables)
)]
//! Dictation coordinator.
//!
//! Mirrors the Swift `DictationCoordinator` state machine. Single owner of
//! session state. Receives hotkey edges, drives recorder + ASR + polish +
//! insertion, persists history, emits `capsule:state` events to the capsule
//! window.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use ferrous_opencc::{config::BuiltinConfig, OpenCC};
use parking_lot::Mutex;
use uuid::Uuid;

#[cfg(target_os = "windows")]
use crate::asr::local::{
    foundry, sherpa, FoundryLocalRuntime, FoundryLocalWhisperAsr, SherpaOnnxAsr, SherpaOnnxRuntime,
};
use crate::asr::{
    BailianCredentials, BailianRealtimeASR, DashScopeMultimodalASR, DictionaryHotword,
    ElevenLabsBatchASR, MimoBatchASR, Qwen3RealtimeASR, Qwen3RealtimeCredentials, RawTranscript,
    VolcengineCredentials, VolcengineStreamingASR, WhisperBatchASR,
};
use crate::combo_hotkey::{ComboHotkeyError, ComboHotkeyEvent, ComboHotkeyMonitor};
use crate::coordinator_state::{
    begin_cancel_session_state, begin_recording_abort_before_restore, begin_session_state,
    begin_session_state_with_id, finish_cancel_session_state, finish_starting_session_state,
    new_session_id, publish_abort_idle_after_restore, start_processing_if_listening,
    startup_race_status, BeginOutcome, SessionId, SessionPhase, SessionState, StartupRaceStatus,
};
use crate::hotkey::{HotkeyEvent, HotkeyMonitor};
use crate::insertion::TextInserter;
use crate::persistence::{
    ActivityStore, CorrectionRuleStore, CredentialAccount, CredentialsVault, DictionaryStore,
    HistoryStore, PreferencesStore, StylePackStore,
};
use crate::qa_adapter::TauriQaHostContext;

use crate::llm_gemini::{GeminiConfig, GeminiProvider};
use crate::polish::{
    openai_compatible_temperature_for_provider, ActiveLLMProvider, CodexOAuthConfig,
    CodexOAuthLLMProvider, OpenAICompatibleConfig, OpenAICompatibleLLMProvider,
    CODEX_DEFAULT_MODEL, CODEX_OAUTH_PROVIDER_ID,
};
use crate::qa_hotkey::{QaHotkeyError, QaHotkeyEvent, QaHotkeyMonitor};
use crate::recorder::{Recorder, RecorderError};
#[cfg(target_os = "windows")]
use crate::types::PasteShortcut;
use crate::types::{
    CapsulePayload, CapsuleState, CapsuleStyle, ChineseScriptPreference, DictationSession,
    HotkeyCapability, HotkeyStatus, HotkeyStatusState, InsertStatus, OutputLanguagePreference,
    PolishMode,
};
#[cfg(target_os = "windows")]
use crate::windows_ime_ipc::ImeSubmitTarget;
#[cfg(target_os = "windows")]
use crate::windows_ime_session::{PreparedWindowsImeSession, WindowsImeSessionController};

pub(crate) mod asr_wiring;
mod capsule_focus;
mod dictation;
mod hotkey_loops;
mod polish_flow;
mod qa;
mod qa_session;
mod resources;
#[cfg(all(not(mobile), target_os = "windows"))]
pub(crate) mod selection_voice_session;
mod silence_auto_stop;

use asr_wiring::*;
use capsule_focus::*;
pub(crate) use capsule_focus::{
    capture_external_focus_target, capture_focus_target, capture_frontmost_app,
    restore_focus_target_if_possible,
};
use hotkey_loops::*;
use polish_flow::*;

// Instance-local Less Computer replay source used by the compatibility command.
pub(crate) use dictation::{less_computer_event_replay_after, LessComputerEventReplay};

pub(super) fn qa_event_target() -> &'static str {
    #[cfg(target_os = "android")]
    {
        "main"
    }
    #[cfg(not(target_os = "android"))]
    {
        "qa"
    }
}

#[cfg(test)]
use dictation::dictation_error_code;
use dictation::{
    begin_session, cancel_session, end_session, handle_pressed_edge, handle_released_edge,
    handle_trigger_combined, request_stop_during_starting,
};
#[cfg(any(debug_assertions, test))]
use dictation::{handle_pressed, handle_released};
use qa::handle_qa_hotkey_pressed;
#[cfg(test)]
use resources::discard_startup_resources_for_session;
use resources::{SessionResource, SharedRecordingMuteState};

/// 是否在回答期间显示「处理中 / 润色中」胶囊反馈。
///
/// 语音 / 听写路径显示（用户熟悉的小录音条状态机；Linux 下映射到 fcitx5
/// auxDown，显示在候选词栏下方）；打字提问路径不显示（回答在 QA 面板内
/// 流式可见，不应在输入法候选栏闪「✨ 润色中...」）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CapsuleFeedback {
    Show,
    #[cfg(test)]
    Hide,
}

/// 词条建议卡片的窗口尺寸（逻辑点）。
///
/// 显示卡片时必须把胶囊窗口缩到这个大小 —— 见 [`show_vocab_suggestion_card`] 里关于
/// 鼠标穿透的说明。
const VOCAB_CARD_WIDTH: f64 = 320.0;
/// 一行建议的高度：勾叉按钮 28pt + 行间距 8pt，与 `VocabSuggestionCard.tsx` 对齐。
const VOCAB_CARD_ROW_HEIGHT: f64 = 36.0;
/// 标题行 + 卡片内边距 + 留给投影的外边距。
const VOCAB_CARD_CHROME_HEIGHT: f64 = 72.0;
/// 卡片离屏幕右边缘留多少。
const VOCAB_CARD_EDGE_MARGIN: f64 = 24.0;

/// 把「要不要记住这个词」的卡片弹到胶囊那个位置。
///
/// 复用胶囊窗口而不是新开一个：多显示器定位、Space 贴附（macOS 26 上那个把窗口钉死在
/// 单个桌面的坑）、nonactivating panel 都是踩过坑才对的，重开一个窗口等于重踩一遍。
///
/// 但有一处必须动：**胶囊平时是鼠标完全穿透的**（`set_ignore_cursor_events(true)`），
/// 因为它浮在别的 app 上面，不能挡住用户点下面的东西。卡片要能点，就得临时关掉穿透；
/// 而透明窗口一旦不穿透，**连透明的部分也会拦鼠标**。所以显示卡片时把窗口缩到卡片实际
/// 大小，挡住的范围就只有卡片本身；收起时再恢复。
fn show_vocab_suggestion_card(inner: &Arc<Inner>) {
    let pending = inner.backend.pending_corrections();
    if pending.is_empty() {
        return;
    }
    let Some(capsule) = inner.host.capsule_window() else {
        return;
    };
    let height = VOCAB_CARD_CHROME_HEIGHT + VOCAB_CARD_ROW_HEIGHT * pending.len() as f64;
    let inner_for_main = Arc::clone(inner);
    let _ = capsule.run_on_main_thread(move |capsule| {
        let inner = inner_for_main;
        // **最后一道闸：听写不在 Idle 就绝不弹卡片。**
        //
        // 上游那些判据（观察器代次、`pending_corrections` 是否为空）全都是「读一次再去
        // 干活」，读完到这里还隔着一次跨线程调度 —— 排队的这段时间里 `begin_session_as`
        // 完全可能已经跑完：解除观察器、收起卡片、开启新一轮听写。那种 check-then-act
        // 无论怎么加都堵不住这一段。
        //
        // 判据放在这里才有意义：这是碰窗口之前的最后一个时点，而且问的是**真正的不变量**
        // —— 卡片和录音胶囊共用一个窗口，显示卡片要把窗口缩到卡片大小，在听写进行中弹
        // 出来就是把那次听写的胶囊弄没了（真机踩过，表现是「热键像是坏了」）。
        //
        // `begin_session_as` 是先置 phase 再收卡片的，所以只要它开了头，这里必然看得见。
        if inner.state.lock().phase != crate::coordinator_state::SessionPhase::Idle {
            log::debug!("[vocab-card] suppressed: a dictation session is in flight");
            inner.backend.dismiss_pending_corrections();
            return;
        }
        inner.vocab_card_visible.store(true, Ordering::SeqCst);
        // 卡片是要点的，穿透必须关掉。
        // Android 没有胶囊窗口，tauri 的 set_ignore_cursor_events 在其上不存在
        //（与 capsule_focus.rs 里同一处理）。
        #[cfg(not(mobile))]
        if let Err(e) = capsule.set_cursor_passthrough(false) {
            log::warn!("[vocab-card] set_ignore_cursor_events(false) failed: {e}");
        }
        if let Err(e) = capsule.set_size(VOCAB_CARD_WIDTH, height) {
            log::warn!("[vocab-card] resize failed: {e}");
        }
        if let Err(e) =
            capsule.position_vocab_card(VOCAB_CARD_WIDTH, height, VOCAB_CARD_EDGE_MARGIN)
        {
            log::warn!("[vocab-card] position failed: {e}");
        }
        // 位置同理：`maybe_position_capsule_bottom_center` 的去重缓存只记「显示器 +
        // 翻译态」，卡片这一挪它一无所知。不清掉的话，下一次录音时它会拿相同的
        // 显示器快照判定「没变化」→ 跳过重新定位 → 胶囊留在卡片挪过去的右下角。
        capsule.invalidate_layout();
        capsule.show_for_recording(true);
        #[cfg(target_os = "macos")]
        capsule.restore_main_window_key_if_active();
    });
}

/// 收起卡片：把窗口完整还给胶囊。
///
/// 四条路径都会走到这里 —— 用户点了「好」/「都不用」、10 秒到时、新一轮听写开始。
///
/// **没有卡片时必须原样返回。** `begin_session_as` 每次听写都会调它，如果无条件去
/// `hide()` 那个窗口，就会和 `emit_capsule` 的 show 抢同一个窗口 —— 胶囊时隐时不显，
/// 用户会以为热键坏了。
fn hide_vocab_suggestion_card(inner: &Arc<Inner>) {
    inner.backend.dismiss_pending_corrections();
    if !inner.vocab_card_visible.swap(false, Ordering::SeqCst) {
        return;
    }
    let Some(capsule) = inner.host.capsule_window() else {
        return;
    };
    let _ = capsule.run_on_main_thread(move |capsule| {
        // 先隐藏再改几何：复原要同时动尺寸和位置，窗口还亮着时改就有概率被合成出
        // 一帧「卡片被拉宽、还横着飞过半个屏幕」。
        let _ = capsule.hide();
        // 穿透必须还回去，否则胶囊会一直挡着屏幕底部那一块。
        #[cfg(not(mobile))]
        if let Err(e) = capsule.set_cursor_passthrough(true) {
            log::warn!("[vocab-card] restoring cursor passthrough failed: {e}");
        }
        // 尺寸也必须还回去 —— 卡片把窗口缩到过自己的大小，不复原的话下一次胶囊
        // 就挤在一个 320×108 的窗口里，等于看不见。
        let bounds = crate::capsule_window_bounds(false);
        if let Err(e) = capsule.set_size(bounds.width, bounds.height) {
            log::warn!("[vocab-card] restoring capsule size failed: {e}");
        }
        // 位置一样要还 —— 卡片把窗口挪到了右下角，胶囊的位置是底部居中。
        // 只还尺寸不还位置，下一次录音胶囊就出现在右下角（真机上就是这个 bug）。
        //
        // 清缓存和这次重定位是两件事，都要做：清缓存保证「就算这次重定位失败，
        // 下一次 emit_capsule 也一定会重算」，重定位保证「就算有哪条路径绕过了
        // emit_capsule 直接 show，窗口也已经在对的地方」。
        capsule.invalidate_layout();
        if let Err(e) = capsule.position_capsule_bottom_center(false) {
            log::warn!("[vocab-card] restoring capsule position failed: {e}");
        }
    });
}

/// 解除手改观察器 —— **唯一的解除入口，三条路径都必须走它。**
///
/// 两步缺一不可，而这正是它必须收口成一个函数的原因：
///
/// 1. `*slot = None` 丢掉 `EditWatcher`，其 `Drop` 置位停止 flag；
/// 2. 推进代次，让还在路上的上报当场失效。
///
/// 只做第 1 步是不够的：解除是**异步**的，观察线程要到下一次 runloop 轮转（≤1s）才看得见
/// flag，而 AX 通知回调正跑在那次轮转里面。漏掉第 2 步，一条属于上一轮的建议就会在新会话
/// 进行中弹出卡片 —— 而卡片会把胶囊窗口缩到卡片大小，等于把正在进行的那次听写的胶囊
/// 弄没了（真机踩过，表现是「热键像是坏了」）。
///
/// 这个函数是补出来的：代次守卫刚加进来时，`arm_edit_watch` 和 `disarm_edit_watch` 各自
/// 推了代次，唯独 `begin_session_as` 还是裸的 `*slot = None` —— 而它恰好是「新会话开始」
/// 这条主路径，也就是上面那个 bug 的实际触发路径。三处各写各的，漏一处就等于没修。
fn disarm_edit_watch(inner: &Arc<Inner>) {
    *inner.edit_watcher.lock() = None;
    inner.edit_watch_generation.fetch_add(1, Ordering::SeqCst);
}

/// 兜底卡片的窗口宽度（逻辑点）。比词条卡片宽一点 —— 这张要放一整段话。
const FALLBACK_CARD_WIDTH: f64 = 360.0;
/// Webview 首次渲染前的安全高度。真实高度由卡片 DOM 测量后通过 IPC 回报。
const FALLBACK_CARD_INITIAL_HEIGHT: f64 = 260.0;
/// 尺寸 IPC 的原生安全边界，不表达任何 CSS 布局规则。
const FALLBACK_CARD_MIN_HEIGHT: f64 = 96.0;
const FALLBACK_CARD_MAX_HEIGHT: f64 = 320.0;

fn validated_fallback_card_height(
    active_presentation_id: Option<u64>,
    presentation_id: u64,
    height: f64,
) -> Result<Option<f64>, String> {
    if !height.is_finite() {
        return Err("fallback card height must be finite".into());
    }
    if active_presentation_id != Some(presentation_id) {
        return Ok(None);
    }
    Ok(Some(
        height
            .ceil()
            .clamp(FALLBACK_CARD_MIN_HEIGHT, FALLBACK_CARD_MAX_HEIGHT),
    ))
}

/// 文本没能落到目标 app 时，把它连同一个复制按钮弹出来。
///
/// 为什么需要这张卡片：这些场景下唯一的兜底是「把文本写进剪贴板」，而它既依赖一个
/// 默认可关的开关，用户也**根本不知道文本在剪贴板里** —— 没有任何提示。屏幕上要么
/// 什么都没有，要么只有半截。
///
/// 窗口机制整套照搬 [`show_vocab_suggestion_card`]（复用胶囊窗口、关穿透、缩尺寸、
/// 右下角定位），理由见那里。多的一件事是 `insert_fallback_card_visible`：这张卡片
/// 在会话收尾那一刻弹出，而收尾自己安排了一次 `schedule_capsule_idle` → `hide()`，
/// 必须让那次 hide 认得出卡片并让路。
fn show_insert_fallback_card(inner: &Arc<Inner>, text: String, reason: &'static str) {
    if text.trim().is_empty() {
        return;
    }
    let Some(capsule) = inner.host.capsule_window() else {
        return;
    };
    let inner_for_main = Arc::clone(inner);
    let _ = capsule.run_on_main_thread(move |capsule| {
        let inner = inner_for_main;
        // 与词条卡片同一道闸、同一理由：听写不在 Idle 就绝不碰这个窗口，否则等于把
        // 正在进行的那次听写的胶囊弄没了。收尾路径是先把 phase 置回 Idle 再走到这里的。
        if inner.state.lock().phase != crate::coordinator_state::SessionPhase::Idle {
            log::debug!("[fallback-card] suppressed: a dictation session is in flight");
            inner.insert_fallback_text.lock().take();
            return;
        }
        let presentation_id = inner.host.begin_insert_fallback_card();
        let payload = crate::types::InsertFallbackCardPayload {
            text,
            reason: reason.to_string(),
            presentation_id,
        };
        #[cfg(not(mobile))]
        if let Err(e) = capsule.set_cursor_passthrough(false) {
            log::warn!("[fallback-card] set_ignore_cursor_events(false) failed: {e}");
        }
        if let Err(e) = capsule.set_size(FALLBACK_CARD_WIDTH, FALLBACK_CARD_INITIAL_HEIGHT) {
            log::warn!("[fallback-card] resize failed: {e}");
        }
        if let Err(e) =
            capsule.position_fallback_card(FALLBACK_CARD_WIDTH, FALLBACK_CARD_INITIAL_HEIGHT)
        {
            log::warn!("[fallback-card] position failed: {e}");
        }
        // 位置同理：`maybe_position_capsule_bottom_center` 的去重缓存只记「显示器 +
        // 翻译态」，卡片这一挪它一无所知。不清掉的话下一次录音会判定「没变化」→
        // 跳过重新定位 → 胶囊留在卡片挪过去的右下角。
        capsule.invalidate_layout();
        inner.host.emit_insert_fallback(&payload);
        capsule.show_for_recording(true);
        #[cfg(target_os = "macos")]
        capsule.restore_main_window_key_if_active();
        log::info!(
            "[fallback-card] shown: reason={reason} chars={}",
            payload.text.chars().count()
        );
    });
}

fn report_insert_fallback_card_height(
    inner: &Arc<Inner>,
    presentation_id: u64,
    height: f64,
) -> Result<(), String> {
    let active_presentation_id = inner.host.active_insert_fallback_presentation_id();
    let Some(height) =
        validated_fallback_card_height(active_presentation_id, presentation_id, height)?
    else {
        return Ok(());
    };
    let Some(capsule) = inner.host.capsule_window() else {
        return Ok(());
    };
    let inner_for_main = Arc::clone(inner);
    capsule.run_on_main_thread(move |capsule| {
        if !inner_for_main
            .host
            .insert_fallback_presentation_is_current(presentation_id)
        {
            return;
        }
        if let Err(e) = capsule.set_size(FALLBACK_CARD_WIDTH, height) {
            log::warn!("[fallback-card] measured resize failed: {e}");
        }
        if let Err(e) = capsule.position_fallback_card(FALLBACK_CARD_WIDTH, height) {
            log::warn!("[fallback-card] measured position failed: {e}");
        }
    })
}

/// 收起兜底卡片：把窗口完整还给胶囊。
///
/// 与 [`hide_vocab_suggestion_card`] 同款：**没有卡片时必须原样返回**，否则每次听写
/// 开始都会去 hide 那个窗口，和 `emit_capsule` 的 show 抢。
fn hide_insert_fallback_card(inner: &Arc<Inner>) {
    inner.insert_fallback_text.lock().take();
    let _event_guard = inner.capsule_event_lock.lock();
    let (was_visible, deferred_capsule) = inner.host.dismiss_insert_fallback_card();
    if !was_visible {
        return;
    }
    let Some(capsule) = inner.host.capsule_window() else {
        return;
    };
    let host = inner.host.clone();
    let backend = Arc::clone(&inner.backend);
    let _ = capsule.run_on_main_thread(move |capsule| {
        host.clear_insert_fallback();
        // 先隐藏再改几何：复原要同时动尺寸和位置，窗口还亮着时改就有概率被合成出
        // 一帧「卡片被拉宽、还横着飞过半个屏幕」。
        let _ = capsule.hide();
        // 穿透必须还回去，否则胶囊会一直挡着屏幕那一块。
        #[cfg(not(mobile))]
        if let Err(e) = capsule.set_cursor_passthrough(true) {
            log::warn!("[fallback-card] restoring cursor passthrough failed: {e}");
        }
        // 尺寸也必须还回去 —— 卡片把窗口缩到过自己的大小，不复原的话下一次胶囊
        // 就挤在一个卡片大小的窗口里，等于看不见。
        let bounds = crate::capsule_window_bounds(false);
        if let Err(e) = capsule.set_size(bounds.width, bounds.height) {
            log::warn!("[fallback-card] restoring capsule size failed: {e}");
        }
        // 位置一样要还 —— 卡片把窗口挪到了右下角，胶囊的位置是底部居中。只还尺寸
        // 不还位置，下一次录音胶囊就出现在右下角（词条卡片在真机上踩过这个 bug）。
        // 清缓存和这次重定位两件都要做，理由见 `hide_vocab_suggestion_card`。
        capsule.invalidate_layout();
        if let Err(e) = capsule.position_capsule_bottom_center(false) {
            log::warn!("[fallback-card] restoring capsule position failed: {e}");
        }
        if let Some(payload) = deferred_capsule {
            // 卡片期间 QA / Selection Polish 仍会推进胶囊状态，只是不能碰共享窗口。
            // 卡片释放后把最新状态一次性应用回来；若最新是 Idle，该 helper 会正常隐藏。
            let preferences = backend.get_preferences();
            let show_capsule = payload.selection_polish || preferences.show_capsule;
            let classic_style = matches!(preferences.capsule_style, CapsuleStyle::Classic);
            capsule.apply_capsule_payload(&payload, show_capsule, classic_style, true);
        }
    });
}

#[derive(Clone)]
enum ActiveAsr {
    Volcengine(Arc<VolcengineStreamingASR>),
    Whisper(Arc<WhisperBatchASR>),
    Mimo(Arc<MimoBatchASR>),
    /// 百炼 Fun-ASR-Flash 录音文件识别（DashScope multimodal-generation 批量 HTTP）。
    DashScopeMultimodal(Arc<DashScopeMultimodalASR>),
    ElevenLabs(Arc<ElevenLabsBatchASR>),
    Bailian(Arc<BailianRealtimeASR>),
    /// 百炼 Qwen3-ASR-Flash 实时（OpenAI Realtime 风格 WS 协议）。
    Qwen3Realtime(Arc<Qwen3RealtimeASR>),
    /// 阶跃星辰 StepAudio 实时（OpenAI Realtime 风格 WS，收尾靠静音帧驱动 VAD）。
    StepfunRealtime(Arc<crate::asr::StepfunRealtimeASR>),
    /// 讯飞开放平台实时语音转写（RTASR）流式。
    Xfyun(Arc<crate::asr::XfyunStreamingASR>),
    #[cfg(target_os = "windows")]
    FoundryLocalWhisper(Arc<FoundryLocalWhisperAsr>),
    /// Windows sherpa-onnx 本地 ASR（offline batch + 实验 online streaming）。
    #[cfg(target_os = "windows")]
    SherpaOnnxLocal(Arc<SherpaOnnxAsr>),
    /// 本地 Qwen3-ASR；macOS 可选 MLX/C，Linux 使用 C。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    Local(Arc<crate::asr::local::LocalQwenAsr>),
    /// 本地 Whisper Large-v3 Turbo；只在 macOS + 模型已迁移时可达。
    #[cfg(target_os = "macos")]
    LocalWhisper(Arc<crate::asr::local::LocalWhisperAsr>),
    /// Apple Speech（SFSpeechRecognizer）系统本地 ASR；只在 macOS 可达。
    #[cfg(target_os = "macos")]
    AppleSpeech(Arc<crate::asr::local::AppleSpeechAsr>),
}

fn asr_transcribe_uses_global_timeout(asr: &ActiveAsr) -> bool {
    match asr {
        #[cfg(target_os = "windows")]
        ActiveAsr::FoundryLocalWhisper(_) => false,
        // sherpa-onnx 首次加载 / 下载 / 推理的耗时类似 Foundry，不走
        // COORDINATOR_GLOBAL_TIMEOUT；各 provider 自己里面控制細粒度超时。
        #[cfg(target_os = "windows")]
        ActiveAsr::SherpaOnnxLocal(_) => false,
        #[cfg(target_os = "macos")]
        ActiveAsr::LocalWhisper(_) => false,
        _ => true,
    }
}

// Provider routing is cross-host business policy. Re-export it temporarily for
// compatibility modules while keeping the single implementation in Core.
pub(crate) use openless_core::provider_rules::{
    active_asr_provider_kind, advanced_asr_config_for, derive_bailian_endpoint,
    resolve_effective_asr_provider, volc_resource_history_label, whisper_request_format,
    whisper_transcribe_timeout, ActiveAsrProviderKind, AsrPreflightCredential,
    BailianEndpointProtocol, OPENAI_COMPATIBLE_ASR_PROVIDER_ID, ZENMUX_ASR_PROVIDER_ID,
};
#[cfg(test)]
use openless_core::provider_rules::{
    parse_advanced_asr_config, stepfun_model_is_stream, validate_dashscope_multimodal_model,
    AsrConfiguredFields,
};
use openless_core::provider_rules::{
    batch_asr_chunk_limit_ms as core_batch_asr_chunk_limit_ms, AdvancedAsrConfig,
};

pub(crate) fn batch_asr_chunk_limit_ms(provider_id: &str) -> Option<u64> {
    core_batch_asr_chunk_limit_ms(provider_id, read_advanced_asr_config(provider_id))
}

/// 读取某 ASR provider 的高级配置。仅 `openai-compatible` / `zenmux` 读 vault；
/// 其余命名厂商走硬编码行为（这里返回默认值），避免破坏已测通的路径。
fn read_advanced_asr_config(provider_id: &str) -> AdvancedAsrConfig {
    let raw =
        CredentialsVault::get_for_asr_provider(provider_id, CredentialAccount::AsrAdvancedConfig)
            .ok()
            .flatten();
    advanced_asr_config_for(provider_id, raw.as_deref())
}

pub struct Coordinator {
    inner: Arc<Inner>,
}

fn shared_backend_from_stores(
    history: &HistoryStore,
    activity: &ActivityStore,
    prefs: &PreferencesStore,
    style_packs: &StylePackStore,
    vocab: &DictionaryStore,
    correction_rules: &CorrectionRuleStore,
    app: crate::core_adapters::AppHandleSlot,
    native_asr: crate::core_adapters::TauriNativeAsrDependencies,
    hotkey_status: Arc<Mutex<HotkeyStatus>>,
    qa_context: Arc<TauriQaHostContext>,
) -> Arc<openless_core::OpenLessBackend> {
    let data_dir = crate::persistence::data_dir().unwrap_or_else(|error| {
        log::warn!("[core] data directory unavailable, using fallback config path: {error}");
        std::env::temp_dir().join("openless-core-fallback")
    });
    let locale = std::env::var("LANG")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "en-US".to_string());
    let repositories = openless_core::BackendRepositories {
        preferences: prefs.core(),
        history: history.core(),
        activity: activity.core(),
        vocabulary: vocab.core(),
        correction_rules: correction_rules.core(),
        style_packs: style_packs.core(),
    };
    let backend_slot = crate::core_adapters::backend_slot();
    let mut dependencies = crate::core_adapters::backend_dependencies(
        app,
        Arc::clone(&backend_slot),
        native_asr,
        Arc::clone(&repositories.preferences),
        hotkey_status,
        qa_context,
    );
    dependencies.marketplace_config = Some(openless_core::MarketplaceConfig::production());
    let backend = Arc::new(
        openless_core::OpenLessBackend::new_with_repositories(
            openless_core::BackendConfig {
                cache_dir: data_dir.join("cache"),
                data_dir,
                home_dir: std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(std::path::PathBuf::from),
                resource_dir: None,
                platform: crate::types::PlatformCapabilities::current(),
                locale,
            },
            dependencies,
            repositories,
        )
        .expect("shared backend config always has a non-empty data directory"),
    );
    *backend_slot.lock() = Some(Arc::downgrade(&backend));
    backend
}

/// Install the only narrow callback the QA runtime needs from the Tauri host:
/// attaching an opaque selection insertion target to a Core-owned preview.
/// The callback only captures the shared opaque-target state; the QA adapter
/// never performs a Tauri managed-state lookup back into `Coordinator`.
#[cfg(all(not(mobile), target_os = "windows"))]
fn bind_qa_selection_voice_target(
    qa_context: &Arc<TauriQaHostContext>,
    selection_voice_host: &Arc<Mutex<selection_voice_session::SelectionVoiceHostState>>,
) {
    let selection_voice_host = Arc::clone(selection_voice_host);
    qa_context.set_selection_voice_target_binder(Arc::new(move |session_id, target| {
        selection_voice_session::bind_selection_voice_target_state(
            &selection_voice_host,
            session_id,
            target,
        )
    }));
}

struct StylePackHotkeyRegistration {
    binding: crate::types::ShortcutBinding,
    _monitor: ComboHotkeyMonitor,
}

fn core_session_id(session_id: SessionId) -> openless_core::SessionId {
    openless_core::SessionId::from_uuid(session_id)
}

fn core_completion_insert_status(status: InsertStatus) -> openless_core::DictationInsertStatus {
    match status {
        InsertStatus::Inserted => openless_core::DictationInsertStatus::Inserted,
        InsertStatus::CopiedFallback => openless_core::DictationInsertStatus::CopiedFallback,
        InsertStatus::PasteSent | InsertStatus::Failed => {
            openless_core::DictationInsertStatus::Unknown
        }
    }
}

#[cfg(target_os = "windows")]
fn sherpa_transcript_handler(
    inner: &Arc<Inner>,
    session_id: Option<SessionId>,
) -> crate::asr::local::sherpa_provider::SherpaTokenHandler {
    let events = inner.backend.event_publisher();
    let session_id = session_id.map(core_session_id);
    let offset = Arc::new(AtomicU64::new(0));
    Arc::new(move |piece: String| {
        let current_offset = offset.fetch_add(piece.chars().count() as u64, Ordering::AcqRel);
        events.publish(
            session_id,
            openless_core::BackendEventKind::TranscriptDelta(openless_core::TranscriptDelta {
                text: piece,
                offset: current_offset,
                is_final: false,
            }),
        );
    })
}

struct Inner {
    host: crate::tauri_coordinator_host::TauriCoordinatorHost,
    backend: Arc<openless_core::OpenLessBackend>,
    /// 实际安装在宿主上的快捷键目标。设置事务只通过显式 target 更新这里，
    /// 监听器安装/恢复不得回读尚未提交或已回滚的 preferences。
    hotkey_runtime_target: Mutex<openless_core::HotkeyRuntimeTarget>,
    /// 串行化 Tauri 侧“Core 设置事务 + 宿主 effect”以及风格包删除 effect，
    /// 防止两个命令把显式 runtime target 乱序安装。
    settings_host_gate: Mutex<()>,
    inserter: TextInserter,
    #[cfg(target_os = "windows")]
    windows_ime: WindowsImeSessionController,
    #[cfg(target_os = "windows")]
    prepared_windows_ime_session: Arc<Mutex<Vec<PreparedWindowsImeSessionSlot>>>,
    state: Mutex<SessionState>,
    asr: Mutex<Option<SessionResource<ActiveAsr>>>,
    /// 与 `asr` 同生命周期的构建时快照：本次会话实际构建的 (provider, model)。
    /// store_asr_for_session 一并写入，end_session 取走落 history——比事后重读
    /// 全局设置可靠：会话中途切 provider/model 不会污染归因（PR #826 review）。
    asr_label: Mutex<Option<SessionResource<AsrCallLabel>>>,
    /// 多模态（Omni）模式下的 dictation 录音 PCM 缓冲。只在
    /// `multimodal_pipeline_enabled && pipeline_mode == multimodal` 时使用，
    /// 与 asr 槽互斥——同一会话二者有且仅有一个。
    omni_pcm: Mutex<Option<SessionResource<Arc<resources::PcmBufferConsumer>>>>,
    /// 本地 Qwen3-ASR MLX 引擎缓存。跨会话复用，避免每次重加载 1.2GB+ 模型。
    /// 释放时机由 prefs.local_asr_keep_loaded_secs 决定。
    local_asr_cache: Arc<crate::asr::local::LocalAsrCache>,
    #[cfg(target_os = "macos")]
    local_whisper_cache: Arc<crate::asr::local::LocalWhisperCache>,
    /// 串行化 Qwen / Whisper 的大模型加载与主动释放。供应商切换会先更新 Vault，
    /// 再等待正在进行的旧加载完成并释放，避免旧预加载在切换后把非目标模型写回 cache。
    local_asr_lifecycle: Arc<Mutex<()>>,
    #[cfg(target_os = "windows")]
    foundry_local_runtime: Arc<FoundryLocalRuntime>,
    /// Windows sherpa-onnx 本地 ASR runtime。与 Foundry 同处一个
    /// 位置、同一 lifecycle 语义；上层通过 `ActiveAsr::SherpaOnnxLocal` 后只调
    /// runtime，不会跨模块调。
    #[cfg(target_os = "windows")]
    sherpa_onnx_runtime: Arc<SherpaOnnxRuntime>,
    recorder: Mutex<Option<SessionResource<Recorder>>>,
    /// 当前 dictation / QA session 的 wav 归档是否真的被写到磁盘上。
    /// 由 Recorder::start 返回值 (archive_active) 写入；history.append 路径读取，
    /// 决定 DictationSession.has_audio_recording 字段。比单纯读 prefs.record_audio_for_debug
    /// 更准确：用户开了开关但路径无法创建（权限 / 磁盘满）也算 false。
    audio_archive_active: AtomicBool,
    /// 上一次落字之后武装的手改监听（macOS）。
    ///
    /// 存在 `Inner` 上只为了「下一次听写开始时解除上一次的」这一条生命周期规则 ——
    /// 覆盖这个 Option 会 drop 掉旧的 watcher，drop 即解除。另外三条（60 秒超时、
    /// 前台 app 切换、焦点元素消失）由观察线程自己负责。
    edit_watcher: Mutex<Option<crate::host_document::EditWatcher>>,
    /// 观察器代次。每武装一次 +1；上报时对不上号的一律丢弃。
    ///
    /// 解除是**异步**的：drop `EditWatcher` 只是置一个 flag，观察线程要到下一次 runloop
    /// 轮转（≤1s）才看得见，而 AX 通知回调正跑在那次轮转**里面**。也就是说「已解除」和
    /// 「还能再上报一次」有一段重叠 —— 光靠 flag 只能缩小这个窗口，关不死它。
    ///
    /// 迟到的上报不是小事：卡片会把胶囊窗口缩到卡片大小，一条属于上一轮的建议在**新
    /// 会话进行中**弹出来，等于把正在进行的那次听写的胶囊弄没了。真机上踩过一次，
    /// 表现是「热键像是坏了」。
    ///
    /// 所以判据不放在线程那边，放在这里：只有代次对得上的上报才算数。
    edit_watch_generation: std::sync::atomic::AtomicU64,
    /// 建议卡片是不是正占着胶囊窗口。
    ///
    /// 门控 `hide_vocab_suggestion_card`：没有卡片时它必须什么都不做，否则每次听写
    /// 开始都会去 hide 胶囊窗口，和 `emit_capsule` 的 show 抢同一个窗口。
    vocab_card_visible: AtomicBool,
    /// 「流式上屏被焦点守卫拦下」的信号，值是那次的**完整**文本。
    ///
    /// 只有那条路径会往里放东西——它是唯一一处「屏幕上的内容 ≠ 完整结果」的场景：
    /// `polished` 按约定只保留真打出去的半截，而切走窗口的用户要的是整段。收尾处
    /// (`maybe_show_insert_fallback_card`) 取走它，据此把 `InsertStatus` 从 `Inserted`
    /// 纠正成 `CopiedFallback`，并决定卡片弹什么内容、标题怎么写。
    ///
    /// **取走即消费**，不是「卡片当前内容」的镜像——卡片内容随事件发给前端，后端不留。
    /// 会话被取消时这里可能有残留，下一轮 `begin_session_as` 的 hide 会清掉。
    insert_fallback_text: Mutex<Option<String>>,
    recording_mute: Mutex<SharedRecordingMuteState>,
    hotkey: Mutex<Option<HotkeyMonitor>>,
    hotkey_status: Arc<Mutex<HotkeyStatus>>,
    hotkey_trigger_held: AtomicBool,
    /// 当前主听写热键按下的代次。组合键撤销通道使用同一代次，避免迟到事件
    /// 误取消下一次按下开启的会话。
    hotkey_press_generation: AtomicU64,
    /// 当前代次是否真的开出了会话；0 表示没有可撤销的会话。
    hotkey_press_began_session: AtomicU64,
    /// 组合键事件可能先于 Pressed 事件抵达协调器，暂存其代次供仲裁窗口消费。
    /// 用队列而不是单个槽，避免主 bridge 忙于上一轮仲裁时覆盖连续按下的事件。
    hotkey_combo_pending_presses: Mutex<std::collections::VecDeque<u64>>,
    /// 防抖时间戳：handle_pressed_edge 入口检查与本字段的距离，< 250ms 的边沿直接
    /// 丢弃（误触双击 / 微动开关回弹 / 用户连点过快造成的空转写报错）。
    /// 与 `hotkey_trigger_held` 互补 —— held 防 press-without-release，本字段防
    /// press-release-press 三连过快。
    last_hotkey_dispatch_at: Mutex<Option<std::time::Instant>>,
    /// Auto 模式下这次会话「按下」的事件时刻。松手时用按下/松开的事件时间戳差值
    /// 判定短按（Toggle 锁存）还是长按（Hold 松手即停）。见 dictation.rs 的
    /// AUTO_HOLD_THRESHOLD。
    hotkey_press_at: Mutex<Option<std::time::Instant>>,
    /// 会话收尾（成功 / 取消 / 失败）将 phase 设为 Idle 时记录的时间戳 + POST_SESSION_COOLDOWN_MS。
    /// handle_pressed 在 (Toggle, Idle) 分支检查此字段：未过期则忽略该次按键，防止胶囊离场
    /// 动画期间误激活新听写（issue #545）；也让识别中排队的热键按下在收尾后一律静默丢弃（issue #856）。
    session_cooldown_until: Mutex<Option<std::time::Instant>>,
    shortcut_recording_active: AtomicBool,
    /// Less Computer modifier 热键的按下代次与待处理组合键事件。
    less_computer_press_generation: AtomicU64,
    less_computer_combo_pending_press: AtomicU64,
    /// 自定义组合键监听器（global-hotkey crate）。当 `prefs.hotkey.trigger == Custom` 时
    /// 代替 modifier-only 的 hotkey monitor。`None` 表示不使用自定义组合键或还没成功安装。
    combo_hotkey: Mutex<Option<ComboHotkeyMonitor>>,
    side_aware_combo: Mutex<Option<crate::side_aware_combo::SideAwareComboMonitor>>,
    translation_hotkey: Mutex<Option<ComboHotkeyMonitor>>,
    switch_style_hotkey: Mutex<Option<ComboHotkeyMonitor>>,
    open_app_hotkey: Mutex<Option<ComboHotkeyMonitor>>,
    /// 风格包直达快捷键监听器（issue #759）：pack_id → 实际绑定 + monitor。
    /// 绑定元数据让 supervisor 能区分「同一 pack_id 但按键已变化」，并在任何
    /// 非事务设置路径注册失败后继续重试到实际状态与 prefs 一致。
    style_pack_hotkeys: Mutex<std::collections::HashMap<String, StylePackHotkeyRegistration>>,
    /// 选区润色快捷键：modifier-only 复用 `HotkeyMonitor`，其它组合键复用
    /// `ComboHotkeyMonitor`。桌面（非 mobile）专属。
    #[cfg(not(mobile))]
    selection_polish_hotkey: Mutex<Option<ComboHotkeyMonitor>>,
    /// 选区语音宿主资源。业务 session/prompt/preview 由 openless-core 独占。
    #[cfg(all(not(mobile), target_os = "windows"))]
    selection_voice_host: Arc<Mutex<selection_voice_session::SelectionVoiceHostState>>,
    /// 「本次会话真的要翻译」。每次 begin_session 重置为 false；hotkey 监听器在
    /// Listening / Starting 阶段看到 Shift down 边沿（或安卓浮层请求）时，经
    /// `arm_translation_if_effective` 判定翻译确实会生效（设了目标语言、且不等于唯一工作语言）
    /// 后才 set true。
    ///
    /// 判定收在写入侧：读取侧之一是音频回调线程上的 emit_capsule，不能碰偏好锁。
    /// 胶囊提示与 end_session 的 polish 分派因此读到同一个真值。详见 issue #4。
    translation_active: AtomicBool,
    /// 划词语音问答（issue #118）：与 dictation hotkey 平行的全局快捷键
    /// 监听器（global-hotkey crate）。`None` 表示功能关闭或还没成功安装。
    qa_hotkey: Mutex<Option<QaHotkeyMonitor>>,
    coding_agent_modifier_hotkey: Mutex<Option<HotkeyMonitor>>,
    coding_agent_combo_hotkey: Mutex<Option<ComboHotkeyMonitor>>,
    /// 最近一次 emit_capsule 下发的 state，纯内省/测试用途（在 app 句柄校验之前写入，
    /// 因此无 GUI 的测试环境也能断言「按下热键 → 弹了哪种胶囊」）。写入是单次廉价
    /// 加锁，对 ~30Hz 录音回调可忽略。
    last_capsule_state: Mutex<Option<CapsuleState>>,
    /// 每次 capsule payload 递增。选区润色的终态自动隐藏会带上该代数，防止旧 timer
    /// 覆盖新的选区润色/语音/QA 可见状态。
    capsule_event_epoch: AtomicU64,
    /// 将 capsule 事件与自动隐藏线性化。这样一个旧 timer 要么在新的 payload 之前收起
    /// 旧提示，要么发现代数已改变直接放弃，绝不会在新会话之后补发 Idle。
    capsule_event_lock: Mutex<()>,
    /// 选区润色的轻量提示仍在显示或处理中。已有语音/QA 的旧 auto-hide timer 必须在
    /// 此期间让路，避免把选区润色浮窗提前收掉。
    selection_polish_capsule_active: AtomicBool,
    /// Tauri QA window visibility. All QA business state belongs to openless-core.
    qa_context: Arc<TauriQaHostContext>,
    /// 预备态标志：按下热键即"乐观显示"胶囊（带入场动画），此时麦克风还在 cpal
    /// init 窗口内、没有第一帧 PCM。为 true 时 emit_capsule 把 Recording payload 的
    /// `warming` 打成 true（前端渲染"待命"光效）；`level_handler` 首次触发（PCM 真的
    /// 流入）后置 false，光条"点亮"进入正式录音。begin_session 每次入场重置为 true。
    capsule_warming: AtomicBool,
    /// Selection Voice still uses legacy native ASR/recorder handles.
    selection_voice_asr: Mutex<Option<SessionResource<ActiveAsr>>>,
    selection_voice_recorder: Mutex<Option<SessionResource<Recorder>>>,
    /// Coordinator 退出信号。各 hotkey supervisor loop 在每轮重试 sleep 之前会检查
    /// 此 flag；为 true 时 loop 立刻 return。生产场景里 process exit 一并 reap 所有
    /// supervisor 线程，但 integration test 和未来 RunEvent::Exit 钩子需要这条
    /// 显式退出路径。审计 3.1.2。
    shutdown: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionHotkeyKind {
    SwitchStyle,
    OpenApp,
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
struct PreparedWindowsImeSessionSlot {
    session_id: SessionId,
    prepared: PreparedWindowsImeSession,
}

impl Coordinator {
    #[cfg(mobile)]
    pub(crate) fn bind_selection_voice_target(
        &self,
        _session_id: openless_core::SessionId,
        _insertion_target: crate::selection::SelectionInsertionTarget,
    ) -> Result<(), String> {
        Err("selectionVoiceTargetUnavailable".to_string())
    }

    pub fn new() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::new_with_local_runtimes(
                Arc::new(FoundryLocalRuntime::new()),
                Arc::new(SherpaOnnxRuntime::new()),
            )
        }

        #[cfg(not(target_os = "windows"))]
        {
            #[cfg(target_os = "android")]
            const PERSIST_DEGRADE_SUFFIX: &str = " (Android 禁止 /data/local/tmp)";
            #[cfg(not(target_os = "android"))]
            const PERSIST_DEGRADE_SUFFIX: &str = "";

            let history = HistoryStore::new().unwrap_or_else(|e| {
                log::error!(
                    "[coord] HistoryStore init failed: {e}; 降级为空历史记录{PERSIST_DEGRADE_SUFFIX}"
                );
                HistoryStore::new_fallback()
            });
            let prefs = PreferencesStore::new().unwrap_or_else(|e| {
                log::error!(
                    "[coord] PreferencesStore init failed: {e}; 降级为默认偏好设置{PERSIST_DEGRADE_SUFFIX}"
                );
                PreferencesStore::new_fallback()
            });
            // 启动即同步系统代理开关（issue #869），让首个请求就按用户设置建客户端。
            crate::net::set_use_system_proxy(prefs.get().use_system_proxy);
            let style_packs = StylePackStore::new(&prefs).unwrap_or_else(|e| {
                log::error!(
                    "[coord] StylePackStore init failed: {e}; 降级为空样式包列表{PERSIST_DEGRADE_SUFFIX}"
                );
                StylePackStore::new_fallback()
            });
            let vocab = DictionaryStore::new().unwrap_or_else(|e| {
                log::error!(
                    "[coord] DictionaryStore init failed: {e}; 降级为空词库{PERSIST_DEGRADE_SUFFIX}"
                );
                DictionaryStore::new_fallback()
            });
            let correction_rules = CorrectionRuleStore::new().unwrap_or_else(|e| {
                log::error!(
                    "[coord] CorrectionRuleStore init failed: {e}; 降级为空纠错规则{PERSIST_DEGRADE_SUFFIX}"
                );
                CorrectionRuleStore::new_fallback()
            });

            let activity = ActivityStore::load().unwrap_or_else(|e| {
                log::error!("[coord] ActivityStore init failed: {e}; 活动计数降级为内存态");
                ActivityStore::new_fallback()
            });

            let app = crate::core_adapters::app_handle_slot();
            let native_asr = crate::core_adapters::TauriNativeAsrDependencies::new();
            let hotkey_status = Arc::new(Mutex::new(HotkeyStatus::default()));
            let qa_context = Arc::new(TauriQaHostContext::default());
            let backend = shared_backend_from_stores(
                &history,
                &activity,
                &prefs,
                &style_packs,
                &vocab,
                &correction_rules,
                Arc::clone(&app),
                native_asr.clone(),
                Arc::clone(&hotkey_status),
                Arc::clone(&qa_context),
            );

            let host = crate::tauri_coordinator_host::TauriCoordinatorHost::new(Arc::clone(&app));
            let hotkey_runtime_target = (&backend.get_preferences()).into();
            let inner = Arc::new(Inner {
                    host,
                    backend,
                    hotkey_runtime_target: Mutex::new(hotkey_runtime_target),
                    settings_host_gate: Mutex::new(()),
                    inserter: TextInserter::new(),
                    state: Mutex::new(SessionState::default()),
                    asr: Mutex::new(None),
                    asr_label: Mutex::new(None),
                    omni_pcm: Mutex::new(None),
                    recorder: Mutex::new(None),
                    audio_archive_active: AtomicBool::new(false),
                    edit_watcher: Mutex::new(None),
                    edit_watch_generation: std::sync::atomic::AtomicU64::new(0),
                    vocab_card_visible: AtomicBool::new(false),
                    insert_fallback_text: Mutex::new(None),
                    recording_mute: Mutex::new(SharedRecordingMuteState::new()),
                    hotkey: Mutex::new(None),
                    hotkey_status,
                    hotkey_trigger_held: AtomicBool::new(false),
                    hotkey_press_generation: AtomicU64::new(0),
                    hotkey_press_began_session: AtomicU64::new(0),
                    hotkey_combo_pending_presses: Mutex::new(std::collections::VecDeque::new()),
                    last_hotkey_dispatch_at: Mutex::new(None),
                    hotkey_press_at: Mutex::new(None),
                    session_cooldown_until: Mutex::new(None),
                    shortcut_recording_active: AtomicBool::new(false),
                    less_computer_press_generation: AtomicU64::new(0),
                    less_computer_combo_pending_press: AtomicU64::new(0),
                    combo_hotkey: Mutex::new(None),
                    side_aware_combo: Mutex::new(None),
                    translation_hotkey: Mutex::new(None),
                    switch_style_hotkey: Mutex::new(None),
                    open_app_hotkey: Mutex::new(None),
                    style_pack_hotkeys: Mutex::new(std::collections::HashMap::new()),
                    #[cfg(not(mobile))]
                    selection_polish_hotkey: Mutex::new(None),
                    #[cfg(all(not(mobile), target_os = "windows"))]
                    selection_voice_host: Arc::new(Mutex::new(
                        selection_voice_session::SelectionVoiceHostState::default(),
                    )),
                    translation_active: AtomicBool::new(false),
                    qa_hotkey: Mutex::new(None),
                    coding_agent_modifier_hotkey: Mutex::new(None),
                    coding_agent_combo_hotkey: Mutex::new(None),
                    last_capsule_state: Mutex::new(None),
                    capsule_event_epoch: AtomicU64::new(0),
                    capsule_event_lock: Mutex::new(()),
                    selection_polish_capsule_active: AtomicBool::new(false),
                    qa_context: Arc::clone(&qa_context),
                    capsule_warming: AtomicBool::new(false),
                    selection_voice_asr: Mutex::new(None),
                    selection_voice_recorder: Mutex::new(None),
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    local_asr_cache: native_asr.qwen_cache(),
                    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                    local_asr_cache: Arc::new(crate::asr::local::LocalAsrCache::new()),
                    #[cfg(target_os = "macos")]
                    local_whisper_cache: native_asr.whisper_cache(),
                    local_asr_lifecycle: Arc::new(Mutex::new(())),
                    shutdown: AtomicBool::new(false),
                });
            #[cfg(all(not(mobile), target_os = "windows"))]
            bind_qa_selection_voice_target(&qa_context, &inner.selection_voice_host);
            Self { inner }
        }
    }

    /// 保留旧构造函数：现有调用点（含单元测试）只传 Foundry runtime。
    /// sherpa-onnx runtime 这里创建默认 offline batch 实例；入产后（lib.rs）请走
    /// `new_with_local_runtimes`，确保 Tauri State 共享同一个 Arc。
    #[cfg(target_os = "windows")]
    pub fn new_with_foundry_runtime(foundry_local_runtime: Arc<FoundryLocalRuntime>) -> Self {
        Self::new_with_local_runtimes(foundry_local_runtime, Arc::new(SherpaOnnxRuntime::new()))
    }

    #[cfg(target_os = "windows")]
    pub fn new_with_local_runtimes(
        foundry_local_runtime: Arc<FoundryLocalRuntime>,
        sherpa_onnx_runtime: Arc<SherpaOnnxRuntime>,
    ) -> Self {
        let history = HistoryStore::new().unwrap_or_else(|e| {
            log::error!("[coord] HistoryStore init failed: {e}; 降级为空历史记录");
            HistoryStore::new_fallback()
        });
        let prefs = PreferencesStore::new().unwrap_or_else(|e| {
            log::error!("[coord] PreferencesStore init failed: {e}; 降级为默认偏好设置");
            PreferencesStore::new_fallback()
        });
        // 启动即同步系统代理开关（issue #869），让首个请求就按用户设置建客户端。
        crate::net::set_use_system_proxy(prefs.get().use_system_proxy);
        let style_packs = StylePackStore::new(&prefs).unwrap_or_else(|e| {
            log::error!("[coord] StylePackStore init failed: {e}; 降级为空样式包列表");
            StylePackStore::new_fallback()
        });
        let vocab = DictionaryStore::new().unwrap_or_else(|e| {
            log::error!("[coord] DictionaryStore init failed: {e}; 降级为空词库");
            DictionaryStore::new_fallback()
        });
        let correction_rules = CorrectionRuleStore::new().unwrap_or_else(|e| {
            log::error!("[coord] CorrectionRuleStore init failed: {e}; 降级为空纠错规则");
            CorrectionRuleStore::new_fallback()
        });

        let activity = ActivityStore::load().unwrap_or_else(|e| {
            log::error!("[coord] ActivityStore init failed: {e}; 活动计数降级为内存态");
            ActivityStore::new_fallback()
        });

        let app = crate::core_adapters::app_handle_slot();
        let hotkey_status = Arc::new(Mutex::new(HotkeyStatus::default()));
        let selection_voice_host = Arc::new(Mutex::new(
            selection_voice_session::SelectionVoiceHostState::default(),
        ));
        let qa_context = Arc::new(TauriQaHostContext::default());
        let backend = shared_backend_from_stores(
            &history,
            &activity,
            &prefs,
            &style_packs,
            &vocab,
            &correction_rules,
            Arc::clone(&app),
            crate::core_adapters::TauriNativeAsrDependencies::new(
                Arc::clone(&foundry_local_runtime),
                Arc::clone(&sherpa_onnx_runtime),
            ),
            Arc::clone(&hotkey_status),
            Arc::clone(&qa_context),
        );

        let host = crate::tauri_coordinator_host::TauriCoordinatorHost::new(Arc::clone(&app));
        let hotkey_runtime_target = (&backend.get_preferences()).into();
        let inner = Arc::new(Inner {
                host,
                backend,
                hotkey_runtime_target: Mutex::new(hotkey_runtime_target),
                settings_host_gate: Mutex::new(()),
                inserter: TextInserter::new(),
                windows_ime: WindowsImeSessionController::new(),
                prepared_windows_ime_session: Arc::new(Mutex::new(Vec::new())),
                state: Mutex::new(SessionState::default()),
                asr: Mutex::new(None),
                asr_label: Mutex::new(None),
                omni_pcm: Mutex::new(None),
                recorder: Mutex::new(None),
                audio_archive_active: AtomicBool::new(false),
                edit_watcher: Mutex::new(None),
                edit_watch_generation: std::sync::atomic::AtomicU64::new(0),
                vocab_card_visible: AtomicBool::new(false),
                insert_fallback_text: Mutex::new(None),
                recording_mute: Mutex::new(SharedRecordingMuteState::new()),
                hotkey: Mutex::new(None),
                hotkey_status,
                hotkey_trigger_held: AtomicBool::new(false),
                hotkey_press_generation: AtomicU64::new(0),
                hotkey_press_began_session: AtomicU64::new(0),
                hotkey_combo_pending_presses: Mutex::new(std::collections::VecDeque::new()),
                last_hotkey_dispatch_at: Mutex::new(None),
                hotkey_press_at: Mutex::new(None),
                session_cooldown_until: Mutex::new(None),
                shortcut_recording_active: AtomicBool::new(false),
                less_computer_press_generation: AtomicU64::new(0),
                less_computer_combo_pending_press: AtomicU64::new(0),
                combo_hotkey: Mutex::new(None),
                side_aware_combo: Mutex::new(None),
                translation_hotkey: Mutex::new(None),
                switch_style_hotkey: Mutex::new(None),
                open_app_hotkey: Mutex::new(None),
                style_pack_hotkeys: Mutex::new(std::collections::HashMap::new()),
                #[cfg(not(mobile))]
                selection_polish_hotkey: Mutex::new(None),
                #[cfg(all(not(mobile), target_os = "windows"))]
                selection_voice_host: Arc::clone(&selection_voice_host),
                translation_active: AtomicBool::new(false),
                qa_hotkey: Mutex::new(None),
                coding_agent_modifier_hotkey: Mutex::new(None),
                coding_agent_combo_hotkey: Mutex::new(None),
                last_capsule_state: Mutex::new(None),
                capsule_event_epoch: AtomicU64::new(0),
                capsule_event_lock: Mutex::new(()),
                selection_polish_capsule_active: AtomicBool::new(false),
                qa_context: Arc::clone(&qa_context),
                capsule_warming: AtomicBool::new(false),
                selection_voice_asr: Mutex::new(None),
                selection_voice_recorder: Mutex::new(None),
                local_asr_cache: Arc::new(crate::asr::local::LocalAsrCache::new()),
                #[cfg(target_os = "macos")]
                local_whisper_cache: Arc::new(crate::asr::local::LocalWhisperCache::new()),
                local_asr_lifecycle: Arc::new(Mutex::new(())),
                foundry_local_runtime,
                sherpa_onnx_runtime,
                shutdown: AtomicBool::new(false),
            });
        bind_qa_selection_voice_target(&qa_context, &selection_voice_host);
        Self { inner }
    }

    /// 后台预加载当前本地 Qwen3-ASR / Whisper 后端；切到对应 provider 时调一次。
    /// 加载是阻塞且数秒，所以放 spawn_blocking 里，不影响 UI 响应。
    /// 模型未下载或当前平台不支持该后端时静默跳过。
    pub fn preload_local_asr_in_background(self: &Arc<Self>) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let inner = Arc::clone(&self.inner);
            let host = inner.host.clone();
            host.spawn(async move {
                // Vault 是运行时 ASR 路由的单一真相；设置页会在随后同步 preferences，
                // 这里不能读取可能仍是旧值的 prefs 快照。
                let provider = CredentialsVault::get_active_asr();
                if crate::asr::local::is_local_qwen3(&provider) {
                    if let Err(error) = preload_local_qwen3(&inner, &provider).await {
                        log::warn!("[coord] local Qwen3 preload failed: {error:#}");
                    }
                    return;
                }

                #[cfg(target_os = "macos")]
                if crate::asr::local::is_local_whisper(&provider) {
                    if let Err(error) = preload_local_whisper(&inner).await {
                        log::warn!("[coord] local Whisper preload failed: {error:#}");
                    }
                    return;
                }
            });
        }
    }

    /// 供应商切换时只释放 Qwen，状态由切换流程在所有 runtime 处理完成后统一上报。
    pub(crate) fn release_local_qwen_engine(&self) {
        self.release_local_asr_engines(true, false);
    }

    /// 供应商切换时只释放 Whisper；非 macOS 平台没有该 cache，保持 no-op。
    pub(crate) fn release_local_whisper_engine(&self) {
        self.release_local_asr_engines(false, true);
    }

    /// 在同一生命周期门闩内释放所有非目标 Qwen / Whisper cache，避免两个粒度化
    /// release 之间有旧预加载任务插入模型。Foundry / Sherpa 由各自 runtime 管理。
    pub(crate) fn release_inactive_local_asr_engines(
        &self,
        release_qwen: bool,
        release_whisper: bool,
    ) {
        self.release_local_asr_engines(release_qwen, release_whisper);
    }

    fn release_local_asr_engines(&self, release_qwen: bool, release_whisper: bool) {
        release_local_asr_engines_now(&self.inner, release_qwen, release_whisper);
    }

    /// 释放当前缓存的本地 ASR 引擎（用户主动点 / 或 删除模型时调）。
    pub fn release_local_asr_engine(&self) {
        self.release_local_asr_engines(true, true);
        emit_local_asr_engine_status(&self.inner);
    }

    pub fn local_asr_loaded_model(&self) -> Option<String> {
        active_local_asr_loaded_model(&self.inner)
    }

    /// 主动把当前本地 ASR 引擎状态推给前端（keepLoadedSecs 变更等命令侧调用）。
    pub fn emit_local_asr_engine_status(&self) {
        emit_local_asr_engine_status(&self.inner);
    }

    pub fn backend(&self) -> Arc<openless_core::OpenLessBackend> {
        Arc::clone(&self.inner.backend)
    }

    pub fn show_core_insert_fallback(&self, text: String, reason: &str) {
        let reason = match reason {
            "partial_stream" => crate::types::INSERT_FALLBACK_REASON_PARTIAL_STREAM,
            _ => crate::types::INSERT_FALLBACK_REASON_INSERT_FAILED,
        };
        show_insert_fallback_card(&self.inner, text, reason);
    }

    pub(crate) fn tauri_host(&self) -> crate::tauri_coordinator_host::TauriCoordinatorHost {
        self.inner.host.clone()
    }

    pub fn android_insert_strategy(&self) -> crate::types::AndroidInsertStrategy {
        self.inner.backend.get_preferences().android_insert_strategy
    }

    pub fn android_overlay_trigger(&self) -> crate::types::AndroidOverlayTrigger {
        self.inner
            .backend
            .get_preferences()
            .android_overlay_trigger
            .normalized()
    }

    pub fn apply_android_overlay_settings_change(
        &self,
        previous: &crate::types::UserPreferences,
        next: &crate::types::UserPreferences,
    ) {
        #[cfg(target_os = "android")]
        {
            use crate::types::android_types::{
                classify_android_overlay_settings_change, AndroidOverlaySettingsAction,
            };
            match classify_android_overlay_settings_change(previous, next) {
                AndroidOverlaySettingsAction::None => {}
                AndroidOverlaySettingsAction::RefreshLayout => {
                    self.refresh_android_overlay_layout();
                }
                AndroidOverlaySettingsAction::Transition { from, to } => {
                    self.transition_android_overlay_trigger(from, to);
                }
            }
        }
        let _ = (previous, next);
    }

    pub fn transition_android_overlay_trigger(
        &self,
        from: crate::types::AndroidOverlayTrigger,
        to: crate::types::AndroidOverlayTrigger,
    ) {
        #[cfg(target_os = "android")]
        {
            use crate::types::AndroidOverlayTrigger;
            fn overlay_trigger_log_name(trigger: AndroidOverlayTrigger) -> &'static str {
                match trigger.normalized() {
                    AndroidOverlayTrigger::Background => "background",
                    AndroidOverlayTrigger::Keyboard => "keyboard",
                    AndroidOverlayTrigger::Always => "always",
                }
            }
            if from == to {
                return;
            }
            log::info!(
                "[coord] overlay transition from={} to={}",
                overlay_trigger_log_name(from),
                overlay_trigger_log_name(to),
            );
            match (from, to) {
                (
                    AndroidOverlayTrigger::Background | AndroidOverlayTrigger::Keyboard,
                    AndroidOverlayTrigger::Always,
                ) => {
                    let _ = crate::android::replace_android_overlay();
                }
                (
                    AndroidOverlayTrigger::Always,
                    AndroidOverlayTrigger::Background | AndroidOverlayTrigger::Keyboard,
                ) => {
                    let _ = crate::android::hide_android_overlay();
                }
                _ => {}
            }
        }
        let _ = (from, to);
    }

    pub fn apply_android_overlay_on_startup(&self) {
        #[cfg(target_os = "android")]
        {
            use crate::types::AndroidOverlayTrigger;
            match self.android_overlay_trigger() {
                AndroidOverlayTrigger::Always => {
                    let _ = crate::android::replace_android_overlay();
                }
                AndroidOverlayTrigger::Background | AndroidOverlayTrigger::Keyboard => {
                    let _ = crate::android::hide_android_overlay();
                }
            }
        }
    }

    pub fn refresh_android_overlay_layout(&self) {
        #[cfg(target_os = "android")]
        {
            let _ = crate::android::refresh_android_overlay_layout();
        }
    }

    /// 让所有 hotkey supervisor loop（dictation / qa / combo / translation /
    /// switch_style / open_app / style_pack / selection_polish）在下一轮 sleep / poll
    /// 后退出。生产场景下进程退出
    /// 一并 reap 所有线程，但 integration test 和未来 RunEvent::Exit 钩子需要
    /// 显式退出路径。审计 3.1.2。
    #[allow(dead_code)]
    pub fn request_shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
    }

    pub fn start_hotkey_listener(&self) {
        // 起一个守护线程，反复尝试安装 hotkey hook。Accessibility 一被授予就立即生效，
        // 用户不需要手动重启 OpenLess。
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("openless-hotkey-supervisor".into())
            .spawn(move || hotkey_supervisor_loop(inner))
            .ok();
    }

    pub fn stop_hotkey_listener(&self) {
        self.inner.hotkey.lock().take();
    }

    /// 启动 QA hotkey supervisor（issue #118）。和 `start_hotkey_listener` 平行：
    /// 守护线程反复尝试注册（用户可能改了组合键），失败则 3s 后重试。
    pub fn start_qa_hotkey_listener(&self) {
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("openless-qa-hotkey-supervisor".into())
            .spawn(move || qa_hotkey_supervisor_loop(inner))
            .ok();
    }

    /// 启动「快速 Agent」双热键 supervisor。与 QA hotkey 平行；功能默认关闭，
    /// 仅在 `coding_agent_enabled` 时注册。
    pub fn start_coding_agent_hotkey_listener(&self) {
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("openless-coding-agent-hotkey-supervisor".into())
            .spawn(move || coding_agent_hotkey_supervisor_loop(inner))
            .ok();
    }

    pub fn stop_coding_agent_hotkey_listener(&self) {
        take_coding_agent_hotkeys_on_main_thread(&self.inner);
    }

    pub(crate) fn update_coding_agent_hotkey_binding(&self) {
        update_coding_agent_hotkey_binding_now(&self.inner);
    }

    pub fn stop_qa_hotkey_listener(&self) {
        // QaHotkeyMonitor::drop 在 macOS 底层是 Carbon RemoveEventHotKey，要求主线程。
        // RunEvent::Exit 回调不保证在 AppKit 主线程跑，drop 漏到 tokio worker 上会
        // 触发 macOS dispatch_assert_queue_fail SIGTRAP。包到 run_on_main_thread 让
        // drop 在主线程发生；AppHandle 已 None 时直接 drop（最坏 crash 也是退出时刻）。
        // 详见 issue #169。
        let inner = Arc::clone(&self.inner);
        if self
            .inner
            .host
            .run_on_main_thread(move || {
                inner.qa_hotkey.lock().take();
            })
            .is_err()
        {
            self.inner.qa_hotkey.lock().take();
        }
    }

    #[cfg(not(mobile))]
    pub fn start_selection_polish_hotkey_listener(&self) {
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("openless-selection-polish-hotkey-supervisor".into())
            .spawn(move || selection_polish_hotkey_supervisor_loop(inner))
            .ok();
    }

    #[cfg(not(mobile))]
    pub fn stop_selection_polish_hotkey_listener(&self) {
        take_selection_polish_hotkey_on_main_thread(&self.inner);
    }

    #[cfg(not(mobile))]
    pub(crate) fn try_update_selection_polish_hotkey_binding(&self) -> Result<(), String> {
        try_update_selection_polish_hotkey_binding(&self.inner)
    }

    #[cfg(not(mobile))]
    pub(crate) fn update_selection_polish_hotkey_binding(&self) {
        if let Err(error) = self.try_update_selection_polish_hotkey_binding() {
            log::warn!("[coord] update selection polish hotkey binding failed: {error}");
        }
    }

    /// 启动自定义组合键监听器。当 `prefs.hotkey.trigger == Custom` 时，
    /// 代替 modifier-only 的 hotkey monitor。
    pub fn start_combo_hotkey_listener(&self) {
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("openless-combo-hotkey-supervisor".into())
            .spawn(move || combo_hotkey_supervisor_loop(inner))
            .ok();
    }

    pub fn stop_combo_hotkey_listener(&self) {
        take_combo_hotkey_on_main_thread(&self.inner);
    }

    pub fn start_translation_hotkey_listener(&self) {
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("openless-translation-hotkey-supervisor".into())
            .spawn(move || translation_hotkey_supervisor_loop(inner))
            .ok();
    }

    pub fn stop_translation_hotkey_listener(&self) {
        take_translation_hotkey_on_main_thread(&self.inner);
    }

    pub fn start_switch_style_hotkey_listener(&self) {
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("openless-switch-style-hotkey-supervisor".into())
            .spawn(move || action_hotkey_supervisor_loop(inner, ActionHotkeyKind::SwitchStyle))
            .ok();
    }

    pub fn stop_switch_style_hotkey_listener(&self) {
        take_action_hotkey_on_main_thread(&self.inner, ActionHotkeyKind::SwitchStyle);
    }

    pub fn start_open_app_hotkey_listener(&self) {
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("openless-open-app-hotkey-supervisor".into())
            .spawn(move || action_hotkey_supervisor_loop(inner, ActionHotkeyKind::OpenApp))
            .ok();
    }

    pub fn stop_open_app_hotkey_listener(&self) {
        take_action_hotkey_on_main_thread(&self.inner, ActionHotkeyKind::OpenApp);
    }

    /// 启动风格包直达快捷键监听（issue #759）。supervisor 线程等 AppHandle 就绪后
    /// 按 prefs 全量注册，个别注册失败按 action hotkey 的节奏重试。
    pub fn start_style_pack_hotkey_listeners(&self) {
        let inner = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("openless-style-pack-hotkey-supervisor".into())
            .spawn(move || style_pack_hotkey_supervisor_loop(inner))
            .ok();
    }

    pub fn stop_style_pack_hotkey_listeners(&self) {
        clear_style_pack_hotkeys_on_main_thread(&self.inner);
    }

    /// 用户在设置里改了风格快捷键列表时调用：按最新 prefs 全量对齐注册状态。
    pub(crate) fn update_style_pack_hotkey_bindings(&self) {
        sync_style_pack_hotkeys_on_main_thread(&self.inner);
    }

    /// 事务式设置路径使用：等待主线程完成整表注册并返回精确失败原因。
    pub(crate) fn try_update_style_pack_hotkey_bindings(&self) -> Result<(), String> {
        try_sync_style_pack_hotkeys_on_main_thread(&self.inner)
    }

    /// 用户在设置里改了自定义组合键时调用。
    pub(crate) fn update_combo_hotkey_binding(&self) {
        let target = hotkey_runtime_target(&self.inner);
        if crate::shortcut_binding::legacy_modifier_trigger(&target.dictation).is_some() {
            take_combo_hotkey_on_main_thread(&self.inner);
            self.inner.side_aware_combo.lock().take();
            log::info!("[coord] combo hotkey 已关闭（modifier-only）");
            return;
        }
        let binding = target.dictation;
        if is_unconfigured_shortcut(&binding) {
            take_combo_hotkey_on_main_thread(&self.inner);
            self.inner.side_aware_combo.lock().take();
            log::info!("[coord] combo hotkey 已关闭（无绑定）");
            return;
        }

        if crate::shortcut_binding::binding_requires_side_aware_hook(&binding) {
            take_combo_hotkey_on_main_thread(&self.inner);
            self.inner.side_aware_combo.lock().take();
            let (tx, rx) = mpsc::channel::<ComboHotkeyEvent>();
            match crate::side_aware_combo::SideAwareComboMonitor::start(binding, tx) {
                Ok(monitor) => {
                    *self.inner.side_aware_combo.lock() = Some(monitor);
                    let bridge_inner = Arc::clone(&self.inner);
                    std::thread::Builder::new()
                        .name("openless-side-combo-bridge".into())
                        .spawn(move || combo_hotkey_bridge_loop(bridge_inner, rx))
                        .ok();
                    log::info!("[coord] side-aware combo hotkey listener installed (via update)");
                }
                Err(e) => {
                    log::warn!("[coord] update side-aware combo binding 失败: {e}");
                }
            }
            return;
        }

        self.inner.side_aware_combo.lock().take();
        let inner_clone = Arc::clone(&self.inner);
        let binding_for_main = binding.clone();
        if self
            .inner
            .host
            .run_on_main_thread(move || {
                if let Some(monitor) = inner_clone.combo_hotkey.lock().as_ref() {
                    if let Err(e) = monitor.update_binding(binding_for_main.clone()) {
                        log::warn!("[coord] update combo hotkey binding 失败: {e}");
                    }
                    return;
                }
                let (tx, rx) = mpsc::channel::<ComboHotkeyEvent>();
                match ComboHotkeyMonitor::start(binding_for_main, tx) {
                    Ok(monitor) => {
                        *inner_clone.combo_hotkey.lock() = Some(monitor);
                        log::info!(
                            "[coord] combo hotkey listener installed on main thread (via update)"
                        );
                        let bridge_inner = Arc::clone(&inner_clone);
                        std::thread::Builder::new()
                            .name("openless-combo-hotkey-bridge".into())
                            .spawn(move || combo_hotkey_bridge_loop(bridge_inner, rx))
                            .ok();
                        #[cfg(target_os = "linux")]
                        sync_custom_dictation_to_plugin(&inner_clone);
                    }
                    Err(e) => {
                        log::warn!("[coord] update combo hotkey binding 失败: {e}");
                    }
                }
            })
            .is_err()
        {
            log::warn!("[coord] update combo hotkey binding: AppHandle 未 bind，跳过");
        }
    }

    /// 用户在设置里改了 QA 组合键时调用。先持久化（由 prefs.set 完成），
    /// 然后通知活着的 monitor 重新注册；monitor 不存在时 supervisor 会自然
    /// 在下一次循环里读到新的 prefs。
    pub(crate) fn update_qa_hotkey_binding(&self) {
        let target = hotkey_runtime_target(&self.inner);
        let Some(binding) = target.qa else {
            // 用户把功能关了 → 直接 drop monitor。drop 也得在主线程，否则 Carbon
            // unregister 会失败/UB。
            let inner_clone = Arc::clone(&self.inner);
            if self
                .inner
                .host
                .run_on_main_thread(move || {
                    inner_clone.qa_hotkey.lock().take();
                })
                .is_err()
            {
                self.inner.qa_hotkey.lock().take();
            }
            log::info!("[coord] QA hotkey 已关闭");
            self.update_modifier_shortcut_bindings();
            return;
        };
        if crate::shortcut_binding::legacy_modifier_trigger(&binding).is_some() {
            let inner_clone = Arc::clone(&self.inner);
            if self
                .inner
                .host
                .run_on_main_thread(move || {
                    inner_clone.qa_hotkey.lock().take();
                })
                .is_err()
            {
                self.inner.qa_hotkey.lock().take();
            }
            self.update_modifier_shortcut_bindings();
            log::info!("[coord] QA hotkey uses modifier-only listener");
            return;
        }
        self.update_modifier_shortcut_bindings();
        // global-hotkey crate 的 manager.register/unregister 必须主线程跑。
        // 没在主线程会让 Carbon 句柄注册看似成功但事件不派发。
        let inner_clone = Arc::clone(&self.inner);
        let binding_for_main = binding.clone();
        if self
            .inner
            .host
            .run_on_main_thread(move || {
                // 路径 1：当前已有 monitor → 在主线程换绑定。
                if let Some(monitor) = inner_clone.qa_hotkey.lock().as_ref() {
                    if let Err(e) = monitor.update_binding(binding_for_main.clone()) {
                        log::warn!("[coord] update QA hotkey binding 失败: {e}");
                    }
                    return;
                }
                // 路径 2：之前还没装上 → 主线程上重装一次（supervisor 也会重试，
                // 但用户体感更快：set_qa_hotkey 命令一返回，hotkey 立即生效）。
                let (tx, rx) = mpsc::channel::<QaHotkeyEvent>();
                match QaHotkeyMonitor::start(binding_for_main, tx) {
                    Ok(monitor) => {
                        *inner_clone.qa_hotkey.lock() = Some(monitor);
                        log::info!(
                            "[coord] QA hotkey listener installed on main thread (via update)"
                        );
                        let bridge_inner = Arc::clone(&inner_clone);
                        std::thread::Builder::new()
                            .name("openless-qa-hotkey-bridge".into())
                            .spawn(move || qa_hotkey_bridge_loop(bridge_inner, rx))
                            .ok();
                    }
                    Err(e) => {
                        log::warn!("[coord] update QA hotkey binding 失败: {e}");
                    }
                }
            })
            .is_err()
        {
            log::warn!("[coord] update QA hotkey binding: AppHandle 未 bind，跳过");
        }
    }

    pub(crate) fn update_translation_hotkey_binding(&self) {
        if let Err(e) = self.try_update_translation_hotkey_binding() {
            log::warn!("[coord] update translation hotkey binding 失败: {e}");
        }
    }

    pub(crate) fn try_update_translation_hotkey_binding(&self) -> Result<(), String> {
        let target = hotkey_runtime_target(&self.inner);
        if is_builtin_translation_shift(&target.translation)
            || crate::shortcut_binding::legacy_modifier_trigger(&target.translation).is_some()
        {
            take_translation_hotkey_on_main_thread(&self.inner);
            self.update_modifier_shortcut_bindings();
            log::info!("[coord] translation hotkey uses modifier-only listener");
            return Ok(());
        }
        self.update_modifier_shortcut_bindings();
        let inner_clone = Arc::clone(&self.inner);
        let binding_for_main = target.translation;
        let (result_tx, result_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        self.inner.host.run_on_main_thread(move || {
            let result = update_translation_hotkey_on_main_thread(inner_clone, binding_for_main);
            let _ = result_tx.send(result.map_err(|e| e.to_string()));
        })?;
        match result_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(result) => result,
            Err(_) => Err("注册翻译快捷键超时".into()),
        }
    }

    pub(crate) fn update_switch_style_hotkey_binding(&self) {
        self.update_action_hotkey_binding(ActionHotkeyKind::SwitchStyle);
    }

    pub(crate) fn update_open_app_hotkey_binding(&self) {
        self.update_action_hotkey_binding(ActionHotkeyKind::OpenApp);
    }

    fn update_action_hotkey_binding(&self, kind: ActionHotkeyKind) {
        // None = 用户主动停用：反注册全局键，立即生效。
        let Some(binding) = action_hotkey_binding(&self.inner, kind) else {
            take_action_hotkey_on_main_thread(&self.inner, kind);
            log::info!("[coord] action hotkey {kind:?} 已停用（用户清空）");
            return;
        };
        if is_modifier_only_shortcut(&binding) {
            take_action_hotkey_on_main_thread(&self.inner, kind);
            log::warn!("[coord] action hotkey {kind:?} 使用了不支持的 modifier-only 绑定，已关闭");
            return;
        }

        let inner_clone = Arc::clone(&self.inner);
        if self
            .inner
            .host
            .run_on_main_thread(move || {
                if let Some(monitor) = action_hotkey_slot(&inner_clone, kind).lock().as_ref() {
                    if let Err(e) = monitor.update_binding(binding.clone()) {
                        log::warn!("[coord] update action hotkey {kind:?} binding 失败: {e}");
                    }
                    return;
                }
                let (tx, rx) = mpsc::channel::<ComboHotkeyEvent>();
                match ComboHotkeyMonitor::start(binding, tx) {
                    Ok(monitor) => {
                        *action_hotkey_slot(&inner_clone, kind).lock() = Some(monitor);
                        let bridge_inner = Arc::clone(&inner_clone);
                        std::thread::Builder::new()
                            .name(action_hotkey_bridge_thread_name(kind).into())
                            .spawn(move || action_hotkey_bridge_loop(bridge_inner, rx, kind))
                            .ok();
                    }
                    Err(e) => log::warn!("[coord] update action hotkey {kind:?} binding 失败: {e}"),
                }
            })
            .is_err()
        {
            log::warn!("[coord] update action hotkey binding: AppHandle 未 bind，跳过");
        }
    }

    /// 给前端 Settings 渲染当前 QA 快捷键 label（如 "Cmd+Shift+;"）。
    /// `qa_hotkey == None` 时返回空串，UI 据此显示「未启用」。
    pub fn qa_hotkey_label(&self) -> String {
        self.inner
            .backend
            .get_preferences()
            .qa_hotkey
            .as_ref()
            .map(|b| b.display_label())
            .unwrap_or_default()
    }

    /// 设置保存后立即把胶囊样式同步进 Tauri Host 缓存。
    /// emit_capsule 的 ~30Hz 主线程闭包本来也会同步，但入场帧的 payload 是在闭包
    /// 同步之前克隆的（会带一帧旧样式），且 Windows 上主线程拥塞时闭包可能延迟
    /// 执行——用户反馈「切换成默认风格后仍显示流光 Siri」。在保存路径直接同步后，
    /// 任何平台的下一次录音从入场帧起就携带最新样式，不再依赖 emit 闭包的时序。
    pub fn sync_capsule_style_from_preferences(&self) {
        self.inner
            .host
            .cache_capsule_style(self.inner.backend.get_preferences().capsule_style);
    }
    pub fn sync_active_asr_provider_from_preferences(&self) -> Result<(), String> {
        let provider = self.inner.backend.get_preferences().active_asr_provider;
        self.sync_active_asr_provider_to_vault(&provider)
    }
    pub fn sync_active_asr_provider_to_vault(&self, provider: &str) -> Result<(), String> {
        if CredentialsVault::get_active_asr() == provider {
            return Ok(());
        }
        CredentialsVault::set_active_asr_provider(provider).map_err(|e| e.to_string())
    }

    /// Apply only the Tauri presentation side of the Core-owned vocabulary
    /// suggestion state. Commands mutate the Core collection first, then pass
    /// this narrow boolean to the host so Coordinator never owns or rewrites
    /// the suggestion business state.
    pub(crate) fn refresh_vocab_suggestion_presentation(&self, has_pending: bool) {
        if has_pending {
            show_vocab_suggestion_card(&self.inner);
        } else {
            hide_vocab_suggestion_card(&self.inner);
        }
    }

    /// 落字失败兜底卡片自己关掉了（用户点关闭 / TTL 到时）。
    pub fn dismiss_insert_fallback_card(&self) {
        hide_insert_fallback_card(&self.inner);
    }

    pub fn report_insert_fallback_card_height(
        &self,
        presentation_id: u64,
        height: f64,
    ) -> Result<(), String> {
        report_insert_fallback_card_height(&self.inner, presentation_id, height)
    }

    /// 用户关掉了「光标上下文」开关 —— 立刻停掉一切还在跑的观察，别等它自己超时。
    ///
    /// 置空即解除：`EditWatcher` 的 `Drop` 会把停止 flag 置位，观察线程在下一次
    /// runloop 轮转（≤1s）时退出并反注册 AXObserver。同时把还挂着的建议卡片收掉 ——
    /// 那些建议是这条链路的产物，开关关了就不该再让用户看见。
    pub fn disarm_edit_watch(&self) {
        disarm_edit_watch(&self.inner);
        hide_vocab_suggestion_card(&self.inner);
        log::info!("[cursor-context] edit watch disarmed: feature switched off");
    }

    pub(crate) fn update_hotkey_binding(&self) {
        let target = hotkey_runtime_target(&self.inner);
        let dictation_trigger =
            crate::shortcut_binding::legacy_modifier_trigger(&target.dictation);
        let binding = crate::types::HotkeyBinding {
            trigger: dictation_trigger.unwrap_or(crate::types::HotkeyTrigger::Custom),
            mode: target.dictation_mode,
            keys: None,
        };
        if dictation_trigger.is_some() {
            take_combo_hotkey_on_main_thread(&self.inner);
        } else {
            self.update_combo_hotkey_binding();
        }
        self.ensure_modifier_hotkey_monitor(binding);
        self.update_modifier_shortcut_bindings();
    }

    fn ensure_modifier_hotkey_monitor(&self, binding: crate::types::HotkeyBinding) {
        if let Some(monitor) = self.inner.hotkey.lock().as_ref() {
            #[cfg(target_os = "linux")]
            let plugin_binding = binding.clone();
            monitor.update_binding(binding);
            #[cfg(target_os = "linux")]
            if plugin_binding.trigger == crate::types::HotkeyTrigger::Custom {
                sync_custom_dictation_to_plugin(&self.inner);
            } else {
                crate::linux_fcitx::sync_binding_to_plugin(&plugin_binding);
            }
            return;
        }
        let (tx, rx) = mpsc::channel::<HotkeyEvent>();
        #[cfg(target_os = "linux")]
        let (fcitx_tx, fcitx_binding) = (tx.clone(), binding.clone());
        let cancel_tx = spawn_esc_cancel_bridge(&self.inner);
        let combo_tx = spawn_combo_abort_bridge(&self.inner, handle_trigger_combined);
        #[cfg(target_os = "linux")]
        let combo_tx_for_fcitx = combo_tx.clone();
        match HotkeyMonitor::start(binding, tx, cancel_tx, combo_tx) {
            Ok(monitor) => {
                let adapter = monitor.kind();
                *self.inner.hotkey.lock() = Some(monitor);
                *self.inner.hotkey_status.lock() = HotkeyStatus {
                    adapter,
                    state: HotkeyStatusState::Installed,
                    message: Some(format!("{} 已安装", adapter.display_name())),
                    last_error: None,
                };
                let inner_clone = Arc::clone(&self.inner);
                std::thread::Builder::new()
                    .name("openless-hotkey-bridge".into())
                    .spawn(move || hotkey_bridge_loop(inner_clone, rx))
                    .ok();
                // Linux: 启动 fcitx5 插件信号监听作为热键源。
                #[cfg(target_os = "linux")]
                {
                    let (qa_trigger, selection_polish_trigger, translation_trigger) =
                        modifier_shortcut_triggers(&self.inner);
                    let custom_key = custom_dictation_key_string(&self.inner);
                    crate::linux_fcitx::start_dictation_signal_listener(
                        fcitx_tx,
                        combo_tx_for_fcitx,
                        fcitx_binding.clone(),
                        qa_trigger,
                        selection_polish_trigger,
                        translation_trigger,
                        custom_key,
                    );
                    if fcitx_binding.trigger == crate::types::HotkeyTrigger::Custom {
                        sync_custom_dictation_to_plugin(&self.inner);
                    } else {
                        crate::linux_fcitx::sync_binding_to_plugin(&fcitx_binding);
                    }
                }
            }
            Err(e) => {
                *self.inner.hotkey_status.lock() = HotkeyStatus {
                    adapter: HotkeyMonitor::capability().adapter,
                    state: HotkeyStatusState::Failed,
                    message: Some(e.message.clone()),
                    last_error: Some(e),
                };
            }
        }
    }

    pub fn update_modifier_shortcut_bindings(&self) {
        if let Some(monitor) = self.inner.hotkey.lock().as_ref() {
            let (qa_trigger, selection_polish_trigger, translation_trigger) =
                modifier_shortcut_triggers(&self.inner);
            monitor.update_modifier_shortcuts(
                qa_trigger,
                selection_polish_trigger,
                translation_trigger,
            );
        }
    }

    /// 将 Core 已校验并完成冲突协调的显式目标应用到宿主监听器。
    ///
    /// 本方法不会读取 preferences；失败时 target 保持为 `next`，由 Core 根据
    /// receipt 调用反向 change 恢复，从而让部分安装也能收敛回旧状态。
    pub(crate) fn apply_hotkey_runtime_change(
        &self,
        change: &openless_core::SettingsValueChange<openless_core::HotkeyRuntimeTarget>,
    ) -> Result<(), String> {
        let previous = &change.previous;
        let next = &change.next;
        *self.inner.hotkey_runtime_target.lock() = next.clone();

        if previous.translation != next.translation {
            self.try_update_translation_hotkey_binding()?;
        }
        #[cfg(not(mobile))]
        if previous.selection_polish != next.selection_polish {
            self.try_update_selection_polish_hotkey_binding()?;
        }
        if previous.style_packs != next.style_packs {
            self.try_update_style_pack_hotkey_bindings()?;
        }
        if previous.dictation != next.dictation || previous.dictation_mode != next.dictation_mode {
            self.update_hotkey_binding();
        }
        if previous.dictation != next.dictation {
            self.update_combo_hotkey_binding();
        }
        if previous.qa != next.qa {
            self.update_qa_hotkey_binding();
        }
        if previous.switch_style != next.switch_style {
            self.update_switch_style_hotkey_binding();
        }
        if previous.open_app != next.open_app {
            self.update_open_app_hotkey_binding();
        }
        if previous.coding_agent_enabled != next.coding_agent_enabled
            || previous.coding_agent_voice != next.coding_agent_voice
        {
            self.update_coding_agent_hotkey_binding();
        }
        Ok(())
    }

    pub(crate) fn lock_settings_host(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.inner.settings_host_gate.lock()
    }

    pub fn hotkey_capability(&self) -> HotkeyCapability {
        HotkeyMonitor::capability()
    }

    pub fn switch_to_previous_style_pack(&self) {
        switch_to_previous_style(&self.inner);
    }

    pub async fn open_qa_from_overlay(&self) -> Result<(), String> {
        log::info!("[coord] overlay QA open requested");
        self.inner
            .backend
            .services()
            .qa
            .show()
            .await
            .map_err(|error| error.message)?;
        self.inner
            .backend
            .services()
            .qa
            .toggle_recording()
            .await
            .map_err(|error| error.message)
    }

    pub async fn finalize_qa_from_overlay(&self) -> Result<(), String> {
        log::info!("[coord] overlay QA finalize requested");
        self.inner
            .backend
            .services()
            .qa
            .toggle_recording()
            .await
            .map_err(|error| error.message)
    }

    /// 返回当前听写阶段（read-only 快照），供 CLI 入口在 dispatch toggle 时决策。
    /// 与原热键边沿走的 `handle_pressed` 分支完全相同的判定逻辑：Idle → start，
    /// Listening → stop。可用于桌面快捷键 → CLI 转发的备用触发路径。
    pub fn dictation_phase_for_cli(&self) -> SessionPhase {
        self.inner.state.lock().phase
    }

    /// CLI 入口的 QA toggle：直接复用 modifier-only QA 热键边沿的处理函数。
    /// 与 `handle_qa_hotkey_pressed` 同语义 — Idle → 开浮窗 / Recording → 收尾 /
    /// Processing → 忽略。桌面快捷键 → CLI 转发的备用进入点。
    pub async fn cli_toggle_qa_panel(&self) {
        handle_qa_hotkey_pressed(&self.inner).await;
    }

    pub fn set_shortcut_recording_active(&self, active: bool) {
        self.inner
            .shortcut_recording_active
            .store(active, Ordering::SeqCst);
        // 同步给热键监听器：录制态激活时 CGEventTap 上报 Fn 按下边沿，
        // 供前端 ShortcutRecorder 提交 Fn 绑定（浏览器不向网页层下发 Fn keydown）。
        #[cfg(not(mobile))]
        let sync_ok = self.inner.hotkey.lock().as_ref().map(|m| {
            m.set_recording_active(active);
            true
        });
        #[cfg(mobile)]
        let sync_ok = None;
        if active {
            reset_shortcut_held_state(&self.inner);
        }
        log::info!(
            "[coord] shortcut recording active={active} (synced_to_hotkey={})",
            sync_ok.unwrap_or(false)
        );
    }

    pub async fn handle_window_hotkey_event(
        &self,
        event_type: String,
        key: String,
        code: String,
        repeat: bool,
    ) -> Result<(), String> {
        handle_window_hotkey_event(&self.inner, event_type, key, code, repeat).await
    }

    #[cfg(any(debug_assertions, test))]
    pub async fn inject_hotkey_click_for_dev(&self) -> Result<(), String> {
        log::info!("[coord] dev hotkey injection started");
        handle_pressed(&self.inner, std::time::Instant::now(), 0).await;
        handle_released(&self.inner, std::time::Instant::now()).await;
        cancel_session(&self.inner);
        Ok(())
    }

    pub fn preview_style_pack_runtime(
        &self,
        style_pack: &crate::types::StylePack,
    ) -> crate::types::StylePackRuntimeDiagnostics {
        let prefs = self.inner.backend.get_preferences();
        let hotwords = enabled_phrases(&self.inner);
        let single_turn = crate::polish::assemble_polish_system_prompt(
            &style_pack.prompt,
            &hotwords,
            &prefs.working_languages,
            prefs.chinese_script_preference,
            prefs.output_language_preference,
            None,
            // front_app 一样传 None：这是脱离运行时的静态预览，前台 app 和光标上下文
            // 都要等真正听写时才有值。
            None,
            false,
        );
        let multi_turn = crate::polish::assemble_polish_system_prompt(
            &style_pack.prompt,
            &hotwords,
            &prefs.working_languages,
            prefs.chinese_script_preference,
            prefs.output_language_preference,
            None,
            None,
            true,
        );
        crate::types::StylePackRuntimeDiagnostics {
            pack_id: style_pack.id.clone(),
            pack_name: style_pack.name.clone(),
            pack_prompt: style_pack.prompt.clone(),
            pack_prompt_chars: style_pack.prompt.chars().count(),
            context_premise: single_turn.context_premise.clone(),
            context_premise_chars: single_turn.context_premise.chars().count(),
            hotword_block: single_turn.hotword_block.clone(),
            hotword_block_chars: single_turn.hotword_block.chars().count(),
            history_instruction: multi_turn.history_instruction.clone(),
            history_instruction_chars: multi_turn.history_instruction.chars().count(),
            single_turn_prompt: single_turn.effective_system_prompt.clone(),
            single_turn_prompt_chars: single_turn.effective_system_prompt.chars().count(),
            multi_turn_prompt: multi_turn.effective_system_prompt.clone(),
            multi_turn_prompt_chars: multi_turn.effective_system_prompt.chars().count(),
            working_languages: prefs.working_languages,
            hotwords,
            context_window_minutes: prefs.polish_context_window_minutes,
            includes_context_premise: single_turn.includes_context_premise,
            includes_hotword_block: single_turn.includes_hotword_block,
            includes_history_instruction: multi_turn.includes_history_instruction,
            preview_omits_front_app: true,
        }
    }
}

fn raw_style_pack_uses_llm(pack: &crate::types::StylePack) -> bool {
    !(pack.kind == crate::types::StylePackKind::Builtin
        && pack.id == crate::types::BUILTIN_STYLE_PACK_RAW_ID
        && pack.prompt == crate::types::StyleSystemPrompts::default().raw)
}

fn raw_mode_uses_llm(style_system_prompt: &str) -> bool {
    style_system_prompt != crate::types::StyleSystemPrompts::default().raw
}

// ─────────────────────────── session lifecycle ───────────────────────────

#[cfg(target_os = "windows")]
fn store_prepared_windows_ime_session(
    slots: &mut Vec<PreparedWindowsImeSessionSlot>,
    session_id: SessionId,
    prepared: PreparedWindowsImeSession,
) {
    slots.retain(|slot| slot.session_id != session_id);
    slots.push(PreparedWindowsImeSessionSlot {
        session_id,
        prepared,
    });
}

#[cfg(target_os = "windows")]
fn take_matching_prepared_windows_ime_session(
    slots: &mut Vec<PreparedWindowsImeSessionSlot>,
    session_id: SessionId,
) -> Option<PreparedWindowsImeSession> {
    let index = slots
        .iter()
        .position(|slot| slot.session_id == session_id)?;
    Some(slots.remove(index).prepared)
}

#[cfg(target_os = "windows")]
fn take_current_prepared_windows_ime_session_for_restore(
    slots: &mut Vec<PreparedWindowsImeSessionSlot>,
    session_id: SessionId,
    current_session_id: SessionId,
) -> Option<PreparedWindowsImeSession> {
    let prepared = take_matching_prepared_windows_ime_session(slots, session_id)?;
    if current_session_id == session_id {
        Some(prepared)
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn restore_prepared_windows_ime_session(inner: &Arc<Inner>, session_id: SessionId) {
    let state = inner.state.lock();
    let prepared = {
        let mut slot = inner.prepared_windows_ime_session.lock();
        take_current_prepared_windows_ime_session_for_restore(
            &mut slot,
            session_id,
            state.session_id,
        )
    };
    if let Some(prepared) = prepared {
        inner.windows_ime.restore_session(prepared);
    }
}

#[cfg(not(target_os = "windows"))]
fn restore_prepared_windows_ime_session(_inner: &Arc<Inner>, _session_id: SessionId) {}

#[cfg(target_os = "windows")]
async fn insert_with_windows_ime_first(
    inner: &Arc<Inner>,
    session_id: SessionId,
    polished: &str,
    restore_clipboard: bool,
    allow_non_tsf_insertion_fallback: bool,
    paste_shortcut: PasteShortcut,
    ime_target: Option<ImeSubmitTarget>,
) -> InsertStatus {
    let prepared = {
        let mut slot = inner.prepared_windows_ime_session.lock();
        take_matching_prepared_windows_ime_session(&mut slot, session_id)
    };
    let Some(prepared) = prepared else {
        log::warn!("[windows-ime] no prepared TSF session for this dictation");
        if should_try_non_tsf_insertion_fallback(
            allow_non_tsf_insertion_fallback,
            InsertStatus::Failed,
        ) {
            return insert_via_non_tsf_fallback(inner, polished, restore_clipboard, paste_shortcut);
        }
        log::warn!("[windows-ime] non-TSF insertion fallback is disabled; failing insert");
        return InsertStatus::Failed;
    };

    let request = crate::windows_ime_ipc::ImeSubmitRequest {
        session_id: Uuid::new_v4().to_string(),
        text: polished.to_string(),
        created_at: Utc::now().to_rfc3339(),
        target: ime_target,
    };

    let ime_status = match inner.windows_ime.submit_prepared(&prepared, request).await {
        Ok(status) => status,
        Err(error) if error.is_outcome_unknown() => {
            log::warn!(
                "[windows-ime] TSF submit outcome is unknown; suppressing non-TSF fallback: {error}"
            );
            InsertStatus::PasteSent
        }
        Err(error) => {
            log::warn!("[windows-ime] TSF submit failed: {error}");
            InsertStatus::Failed
        }
    };
    inner.windows_ime.restore_session(prepared);

    if matches!(ime_status, InsertStatus::Inserted | InsertStatus::PasteSent) {
        ime_status
    } else if should_try_non_tsf_insertion_fallback(allow_non_tsf_insertion_fallback, ime_status) {
        insert_via_non_tsf_fallback(inner, polished, restore_clipboard, paste_shortcut)
    } else {
        log::warn!("[windows-ime] TSF did not insert; non-TSF insertion fallback is disabled");
        InsertStatus::Failed
    }
}

#[cfg(target_os = "windows")]
fn should_try_non_tsf_insertion_fallback(
    allow_non_tsf_insertion_fallback: bool,
    ime_status: InsertStatus,
) -> bool {
    allow_non_tsf_insertion_fallback
        && matches!(
            ime_status,
            InsertStatus::CopiedFallback | InsertStatus::Failed
        )
}

#[cfg(target_os = "windows")]
fn insert_via_non_tsf_fallback(
    inner: &Arc<Inner>,
    polished: &str,
    _restore_clipboard: bool,
    _paste_shortcut: PasteShortcut,
) -> InsertStatus {
    let prefs = inner.backend.get_preferences();
    let sendinput_options = dictation::windows_sendinput_options_from_prefs(&prefs);
    let status = finish_non_tsf_insertion_fallback(
        || {
            inner
                .inserter
                .insert_via_unicode_keystrokes(polished, sendinput_options)
        },
        || inner.inserter.copy_fallback(polished),
    );

    match status {
        InsertStatus::Inserted => {
            log::warn!(
                "[windows-ime] TSF unavailable; inserted via paced Unicode SendInput fallback"
            );
        }
        InsertStatus::CopiedFallback => {
            log::warn!(
                "[windows-ime] TSF unavailable; Unicode SendInput failed, left text on clipboard"
            );
        }
        InsertStatus::PasteSent | InsertStatus::Failed => {
            log::warn!(
                "[windows-ime] TSF unavailable; Unicode SendInput fallback failed and copy fallback failed"
            );
        }
    }

    status
}

#[cfg(any(target_os = "windows", test))]
fn finish_non_tsf_insertion_fallback<U, C>(
    mut unicode_fallback: U,
    mut copy_only_fallback: C,
) -> InsertStatus
where
    U: FnMut() -> InsertStatus,
    C: FnMut() -> InsertStatus,
{
    match unicode_fallback() {
        InsertStatus::Inserted => InsertStatus::Inserted,
        InsertStatus::PasteSent | InsertStatus::CopiedFallback | InsertStatus::Failed => {
            match copy_only_fallback() {
                InsertStatus::CopiedFallback => InsertStatus::CopiedFallback,
                // TextInserter::copy_fallback is copy-only: success is CopiedFallback.
                // Treat any other status as failure so this helper never invents an insert.
                InsertStatus::Inserted | InsertStatus::PasteSent | InsertStatus::Failed => {
                    InsertStatus::Failed
                }
            }
        }
    }
}

#[cfg(test)]
mod non_tsf_fallback_tests {
    use super::finish_non_tsf_insertion_fallback;
    use crate::types::InsertStatus;

    #[test]
    fn unicode_fallback_runs_before_copy_fallback() {
        let mut copy_called = false;
        let status = finish_non_tsf_insertion_fallback(
            || InsertStatus::Inserted,
            || {
                copy_called = true;
                InsertStatus::CopiedFallback
            },
        );

        assert_eq!(status, InsertStatus::Inserted);
        assert!(!copy_called);
    }

    #[test]
    fn copy_fallback_runs_after_unicode_failure() {
        let mut copy_called = false;
        let status = finish_non_tsf_insertion_fallback(
            || InsertStatus::Failed,
            || {
                copy_called = true;
                InsertStatus::CopiedFallback
            },
        );

        assert_eq!(status, InsertStatus::CopiedFallback);
        assert!(copy_called);
    }

    #[test]
    fn double_failure_does_not_pretend_text_was_copied() {
        let mut copy_called = false;
        let status = finish_non_tsf_insertion_fallback(
            || InsertStatus::Failed,
            || {
                copy_called = true;
                InsertStatus::Failed
            },
        );

        assert_eq!(status, InsertStatus::Failed);
        assert!(copy_called);
    }
}

// ─────────────────────────── helpers ───────────────────────────

fn read_whisper_credentials() -> (String, String, String) {
    let api_key = CredentialsVault::get(CredentialAccount::AsrApiKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let active_asr = CredentialsVault::get_active_asr();
    let (default_endpoint, default_model) = whisper_credential_defaults(&active_asr);
    let base_url = CredentialsVault::get(CredentialAccount::AsrEndpoint)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(default_endpoint);
    let model = CredentialsVault::get(CredentialAccount::AsrModel)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(default_model);
    (api_key, base_url, model)
}

/// whisper 兼容系 provider 的「空槽默认值」。zenmux 有厂商默认端点/模型
/// （与前端 preset 一致）；其余预设沿用空 endpoint + `whisper-1`（默认值由
/// 前端切换 provider 时写入 vault）。纯函数，便于单测。
fn whisper_credential_defaults(provider_id: &str) -> (String, String) {
    if provider_id == ZENMUX_ASR_PROVIDER_ID {
        (
            crate::asr::whisper::ZENMUX_DEFAULT_ENDPOINT.to_string(),
            crate::asr::whisper::ZENMUX_DEFAULT_MODEL.to_string(),
        )
    } else {
        (String::new(), "whisper-1".to_string())
    }
}

fn read_mimo_credentials() -> (String, String, String) {
    let api_key = CredentialsVault::get(CredentialAccount::AsrApiKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let base_url = CredentialsVault::get(CredentialAccount::AsrEndpoint)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::mimo::DEFAULT_ENDPOINT.to_string());
    let model = CredentialsVault::get(CredentialAccount::AsrModel)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::mimo::DEFAULT_MODEL.to_string());
    (api_key, base_url, model)
}

fn read_elevenlabs_credentials() -> (String, String, String) {
    let api_key = CredentialsVault::get(CredentialAccount::AsrApiKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let base_url = CredentialsVault::get(CredentialAccount::AsrEndpoint)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::elevenlabs::DEFAULT_ENDPOINT.to_string());
    let model = CredentialsVault::get(CredentialAccount::AsrModel)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::elevenlabs::DEFAULT_MODEL.to_string());
    (api_key, base_url, model)
}

fn read_dashscope_multimodal_credentials() -> (String, String, String) {
    let api_key = CredentialsVault::get(CredentialAccount::AsrApiKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let model = CredentialsVault::get(CredentialAccount::AsrModel)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::dashscope_multimodal::DEFAULT_MODEL.to_string());
    let base_url = if unified_bailian_is_active() {
        let endpoint = read_asr_endpoint(crate::asr::bailian::DEFAULT_ENDPOINT);
        let protocol = match crate::asr::dashscope_multimodal::protocol_for_model(&model) {
            Some(crate::asr::dashscope_multimodal::DashScopeBatchProtocol::AsyncTranscription) => {
                BailianEndpointProtocol::AsyncTranscription
            }
            _ => BailianEndpointProtocol::Multimodal,
        };
        derive_bailian_endpoint(&endpoint, protocol).unwrap_or(endpoint)
    } else {
        CredentialsVault::get(CredentialAccount::AsrEndpoint)
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| crate::asr::dashscope_multimodal::DEFAULT_ENDPOINT.to_string())
    };
    (api_key, base_url, model)
}

fn read_asr_vocabulary_id() -> Option<String> {
    CredentialsVault::get(CredentialAccount::AsrVocabularyId)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
}

fn read_bailian_credentials() -> BailianCredentials {
    let api_key = CredentialsVault::get(CredentialAccount::AsrApiKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let stored_endpoint = read_asr_endpoint(crate::asr::bailian::DEFAULT_ENDPOINT);
    let endpoint = if unified_bailian_is_active() {
        derive_bailian_endpoint(&stored_endpoint, BailianEndpointProtocol::ClassicRealtime)
            .unwrap_or(stored_endpoint)
    } else {
        stored_endpoint
    };
    let model = CredentialsVault::get(CredentialAccount::AsrModel)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::bailian::DEFAULT_MODEL.to_string());
    let vocabulary_id = read_asr_vocabulary_id();
    BailianCredentials {
        api_key,
        endpoint,
        model,
        vocabulary_id,
    }
}

fn read_asr_endpoint(default_endpoint: &str) -> String {
    CredentialsVault::get(CredentialAccount::AsrEndpoint)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| default_endpoint.to_string())
}

/// 统一「阿里云百炼」入口的三条协议共用同一个 `bailian` 凭据条目。存储 endpoint
/// 提供区域/工作空间主机，运行时再按所选模型推导各协议的 scheme 与 path。
/// 老用户停在别名 id（`bailian-qwen3-realtime` / `bailian-fun-asr-flash`）上时不触发，
/// 仍读自己条目里存的 endpoint。
pub(crate) fn unified_bailian_is_active() -> bool {
    CredentialsVault::get_active_asr() == crate::asr::bailian::PROVIDER_ID
}

fn read_qwen3_realtime_credentials() -> Qwen3RealtimeCredentials {
    let api_key = CredentialsVault::get(CredentialAccount::AsrApiKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let endpoint = if unified_bailian_is_active() {
        let endpoint = read_asr_endpoint(crate::asr::bailian::DEFAULT_ENDPOINT);
        derive_bailian_endpoint(&endpoint, BailianEndpointProtocol::QwenRealtime)
            .unwrap_or(endpoint)
    } else {
        CredentialsVault::get(CredentialAccount::AsrEndpoint)
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| crate::asr::qwen_realtime::DEFAULT_ENDPOINT.to_string())
    };
    let model = CredentialsVault::get(CredentialAccount::AsrModel)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::qwen_realtime::DEFAULT_MODEL.to_string());
    Qwen3RealtimeCredentials {
        api_key,
        endpoint,
        model,
    }
}

/// StepFun 实时凭据与批式共用同一组槽位（一把 key、同一个 https base、
/// 模型名区分协议）；wss URL 由 client 的 `connect_url()` 从 base 派生。
/// `prompt` 由调用方按用户词典填充（实时协议接受 prompt、批式只认 hotwords）。
fn read_stepfun_realtime_credentials(
    prompt: Option<String>,
) -> crate::asr::StepfunRealtimeCredentials {
    let api_key = CredentialsVault::get(CredentialAccount::AsrApiKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let endpoint = CredentialsVault::get(CredentialAccount::AsrEndpoint)
        .ok()
        .flatten()
        .unwrap_or_default();
    let model = CredentialsVault::get(CredentialAccount::AsrModel)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::asr::stepfun_realtime::DEFAULT_MODEL.to_string());
    crate::asr::StepfunRealtimeCredentials {
        api_key,
        endpoint,
        model,
        prompt,
    }
}

fn read_volc_credentials() -> VolcengineCredentials {
    use crate::asr::volcengine::VolcengineAuthMode;
    let app_id = CredentialsVault::get(CredentialAccount::VolcengineAppKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    let auth_mode = CredentialsVault::get(CredentialAccount::VolcengineAuthMode)
        .ok()
        .flatten()
        .map(|s| VolcengineAuthMode::parse(&s))
        .unwrap_or(VolcengineAuthMode::AppIdToken);
    // 密钥槽位随鉴权模式：AppIdToken 读旧版 Access Token，ApiKey 读独立的方舟 API Key，
    // 两者互不污染，切换模式不会把旧模式的凭据带进新模式的握手。
    let secret = match auth_mode {
        VolcengineAuthMode::AppIdToken => {
            CredentialsVault::get(CredentialAccount::VolcengineAccessKey)
                .ok()
                .flatten()
                .unwrap_or_default()
        }
        VolcengineAuthMode::ApiKey => CredentialsVault::get(CredentialAccount::VolcengineApiKey)
            .ok()
            .flatten()
            .unwrap_or_default(),
    };
    let resource_id = VolcengineCredentials::resolve_resource_id(
        CredentialsVault::get(CredentialAccount::VolcengineResourceId)
            .ok()
            .flatten(),
    );
    VolcengineCredentials {
        auth_mode,
        app_id,
        access_token: secret,
        resource_id,
    }
}

fn read_xfyun_credentials() -> crate::asr::XfyunCredentials {
    let app_id = CredentialsVault::get(CredentialAccount::XfyunAppId)
        .ok()
        .flatten()
        .unwrap_or_default();
    let api_key = CredentialsVault::get(CredentialAccount::XfyunApiKey)
        .ok()
        .flatten()
        .unwrap_or_default();
    crate::asr::XfyunCredentials { app_id, api_key }
}

fn enabled_hotwords(inner: &Arc<Inner>) -> Vec<DictionaryHotword> {
    inner
        .backend
        .list_vocabulary()
        .unwrap_or_default()
        .into_iter()
        .map(|e| DictionaryHotword {
            phrase: e.phrase,
            enabled: e.enabled,
        })
        .collect()
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
fn read_gemini_credentials() -> anyhow::Result<(String, String, String)> {
    let api_key = CredentialsVault::get(CredentialAccount::ArkApiKey)?.unwrap_or_default();
    let model = CredentialsVault::get(CredentialAccount::ArkModelId)?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "gemini-2.5-flash".to_string());
    let base_url = CredentialsVault::get(CredentialAccount::ArkEndpoint)?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string());
    if api_key.trim().is_empty() {
        anyhow::bail!("API Key 为空");
    }
    let base_url = base_url.trim_end_matches('/').to_string();
    Ok((api_key, model, base_url))
}

/// 构建 ASR 客户端那一刻捕获的 (provider, model) 快照。共享类型同时供主听写、
/// 历史重转录和 Linux host 使用，避免宿主分别定义归因结构。
pub type AsrCallLabel = openless_core::AsrCallLabel;

fn build_active_llm_provider(llm_thinking_enabled: bool) -> anyhow::Result<ActiveLLMProvider> {
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
    let temperature = openai_compatible_temperature_for_provider(
        &active,
        CredentialsVault::get_active_llm_temperature(),
    );
    let config = OpenAICompatibleConfig::new(active, "OpenLess LLM", base_url, api_key, model)
        .with_thinking_enabled(llm_thinking_enabled)
        .with_temperature(temperature)
        .with_extra_headers(CredentialsVault::get_active_llm_extra_headers());
    Ok(ActiveLLMProvider::OpenAI(OpenAICompatibleLLMProvider::new(
        config,
    )))
}

/// 是否启用多模态识别管线：实验开关 + 模式切换都满足才生效。
pub(crate) fn pipeline_multimodal_enabled(prefs: &crate::types::UserPreferences) -> bool {
    prefs.multimodal_pipeline_enabled
        && prefs.pipeline_mode == crate::types::PipelineMode::Multimodal
}

/// 多模态（Omni）模型通道的凭据预检（友好错误信息，供录音前拦截）。
pub(crate) fn ensure_omni_credentials() -> Result<(), String> {
    let api_key = CredentialsVault::get(CredentialAccount::OmniApiKey)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let model = CredentialsVault::get(CredentialAccount::OmniModel)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let base_url = CredentialsVault::get(CredentialAccount::OmniEndpoint)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    if api_key.trim().is_empty() {
        return Err("多模态模型 API Key 为空：请在 服务 → AI 提供商 → 多模态模型 中配置".into());
    }
    if model.trim().is_empty() {
        return Err("多模态模型 id 为空：请在 服务 → AI 提供商 → 多模态模型 中配置".into());
    }
    let active = CredentialsVault::get_active_omni();
    if active != crate::omni::OMNI_GEMINI_PROVIDER_ID && base_url.trim().is_empty() {
        return Err("多模态模型 Base URL 为空：请在 服务 → AI 提供商 → 多模态模型 中配置".into());
    }
    Ok(())
}

fn omni_default_base_url(provider: &str) -> &'static str {
    match provider {
        "openai" => "https://api.openai.com/v1",
        crate::omni::OMNI_GEMINI_PROVIDER_ID => "https://generativelanguage.googleapis.com/v1beta",
        "dashscope-omni" => "https://dashscope.aliyuncs.com/compatible-mode/v1",
        _ => "",
    }
}

/// 读取 omni 命名空间凭据并构建多模态模型通道（与 build_active_llm_provider
/// 平行的唯一构建点）。Gemini 按 provider id / base_url 路由到原生通道。
pub(crate) fn build_active_omni_provider(
    thinking_enabled: bool,
) -> anyhow::Result<crate::omni::OmniProvider> {
    let active = CredentialsVault::get_active_omni();
    let api_key = CredentialsVault::get(CredentialAccount::OmniApiKey)?.unwrap_or_default();
    let model = CredentialsVault::get(CredentialAccount::OmniModel)?.unwrap_or_default();
    let base_url = CredentialsVault::get(CredentialAccount::OmniEndpoint)?.unwrap_or_default();
    if api_key.trim().is_empty() {
        anyhow::bail!("多模态模型 API Key 为空");
    }
    if model.trim().is_empty() {
        anyhow::bail!("多模态模型 id 为空");
    }
    let base_url = if base_url.trim().is_empty() {
        omni_default_base_url(&active).to_string()
    } else {
        base_url.trim().to_string()
    };
    if base_url.is_empty() {
        anyhow::bail!("多模态模型 Base URL 为空");
    }
    // 与 LLM / ASR 通道一致：拒绝指向内网/回环/元数据服务的地址（SSRF 防线）。
    crate::endpoint_security::validate_http_endpoint(&base_url)
        .map_err(|_| anyhow::anyhow!("endpointInvalid"))?;
    let config = crate::omni::OmniConfig {
        provider_id: active.clone(),
        base_url,
        api_key,
        model,
        extra_headers: CredentialsVault::get_active_omni_extra_headers(),
        temperature: crate::polish::openai_compatible_temperature_for_provider(
            &active,
            CredentialsVault::get_active_omni_temperature(),
        ),
        thinking_enabled,
    };
    Ok(crate::omni::OmniProvider::new(config))
}

fn resolve_ark_endpoint(api_key: &str) -> anyhow::Result<String> {
    let endpoint = CredentialsVault::get(CredentialAccount::ArkEndpoint)?.filter(|s| !s.is_empty());
    resolve_ark_endpoint_with_policy(api_key, endpoint)
}

fn resolve_ark_endpoint_with_policy(
    api_key: &str,
    endpoint: Option<String>,
) -> anyhow::Result<String> {
    if api_key.trim().is_empty() && endpoint.is_none() {
        anyhow::bail!("API Key 为空");
    }
    let resolved = endpoint
        .unwrap_or_else(|| "https://ark.cn-beijing.volces.com/api/v3/chat/completions".to_string());
    // 与 validate_provider_credentials / list_provider_models 同一校验函数：仅保证是
    // 合法 http(s) URL，地址不设限制（用户显式配置，前端有 http 风险提示）。
    crate::endpoint_security::validate_http_endpoint(&resolved)?;
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    #[test]
    fn fallback_card_height_report_rejects_non_finite_values() {
        assert!(super::validated_fallback_card_height(Some(7), 7, f64::NAN).is_err());
        assert!(super::validated_fallback_card_height(Some(7), 7, f64::INFINITY).is_err());
    }

    #[test]
    fn fallback_card_height_report_ignores_stale_presentations() {
        assert_eq!(
            super::validated_fallback_card_height(Some(8), 7, 180.0).unwrap(),
            None
        );
        assert_eq!(
            super::validated_fallback_card_height(None, 7, 180.0).unwrap(),
            None
        );
    }

    #[test]
    fn fallback_card_height_report_clamps_to_native_safety_bounds() {
        assert_eq!(
            super::validated_fallback_card_height(Some(7), 7, 40.0).unwrap(),
            Some(96.0)
        );
        assert_eq!(
            super::validated_fallback_card_height(Some(7), 7, 500.0).unwrap(),
            Some(320.0)
        );
        assert_eq!(
            super::validated_fallback_card_height(Some(7), 7, 181.2).unwrap(),
            Some(182.0)
        );
    }

    /// 造一条词典条目。传给 `prioritize_vocab_for_asr` 时必须是词典的原始顺序
    /// （最近添加在前）。
    fn vocab_entry(phrase: &str, hits: u64) -> crate::types::DictionaryEntry {
        crate::types::DictionaryEntry {
            id: phrase.to_string(),
            phrase: phrase.to_string(),
            note: None,
            enabled: true,
            hits,
            created_at: String::new(),
        }
    }

    fn learned_vocab_entry(phrase: &str, hits: u64) -> crate::types::DictionaryEntry {
        let mut entry = vocab_entry(phrase, hits);
        entry.note = Some(crate::types::LEARNED_VOCAB_NOTE.to_string());
        entry
    }

    /// 真机复现：刚添加的碎片排在词典最前，把命中 18 次的 `hermes`、7 次的
    /// `win-shukong` 挤出了 240 字符的 ASR 预算。保底席位之后必须按命中排。
    #[test]
    fn asr_vocab_orders_by_hits_once_past_the_fresh_seats() {
        let mut entries: Vec<_> = (0..super::FRESH_VOCAB_SEATS)
            .map(|i| vocab_entry(&format!("fresh{i}"), 0))
            .collect();
        entries.push(vocab_entry("scrap", 1));
        entries.push(vocab_entry("hermes", 18));
        entries.push(vocab_entry("win-shukong", 7));

        let ordered = super::prioritize_vocab_for_asr(entries);

        let pos = |p: &str| ordered.iter().position(|x| x == p).expect("phrase kept");
        assert!(
            pos("hermes") < pos("scrap"),
            "命中多的必须排在刚收进来的碎片前面"
        );
        assert!(pos("win-shukong") < pos("scrap"));
        assert!(pos("hermes") < pos("win-shukong"), "命中多的在前");
    }

    /// 纯按命中排会让刚添加的词永远进不去预算——而用户刚加它，多半就是因为刚
    /// 被它坑过。最近添加的若干条要有保底席位。
    #[test]
    fn asr_vocab_reserves_seats_for_freshly_added_phrases() {
        let mut entries = vec![vocab_entry("Pathwyze", 0)];
        entries.extend((0..30).map(|i| vocab_entry(&format!("old{i}"), 100 + i)));

        let ordered = super::prioritize_vocab_for_asr(entries);

        assert_eq!(
            ordered.first().map(String::as_str),
            Some("Pathwyze"),
            "命中为 0 的新词也要占住最前的保底席位"
        );
    }

    /// 同词异形一起进词表既浪费预算，又让模型无所适从。留命中多的那个写法——
    /// 位置取最靠前那次，但内容不能被刚收进来、命中为 0 的变体顶掉。
    #[test]
    fn asr_vocab_dedupes_case_insensitively_keeping_the_most_hit_spelling() {
        let entries = vec![
            vocab_entry("claude", 0),
            vocab_entry("mac-mini", 27),
            vocab_entry("Claude", 33),
        ];

        let ordered = super::prioritize_vocab_for_asr(entries);

        assert_eq!(
            ordered,
            vec!["Claude".to_string(), "mac-mini".to_string()],
            "保留 Claude 的写法，但沿用 claude 那次更靠前的位置"
        );
    }

    #[test]
    fn learned_vocab_does_not_consume_fresh_manual_seats() {
        let mut entries = Vec::new();
        for i in 0..super::FRESH_VOCAB_SEATS {
            entries.push(learned_vocab_entry(
                &format!("learned{i}"),
                1_000 - i as u64,
            ));
            entries.push(vocab_entry(&format!("manual{i}"), 0));
        }

        let ordered = super::prioritize_vocab_for_asr(entries);
        let expected_manual: Vec<String> = (0..super::FRESH_VOCAB_SEATS)
            .map(|i| format!("manual{i}"))
            .collect();

        assert_eq!(
            &ordered[..super::FRESH_VOCAB_SEATS],
            expected_manual.as_slice(),
            "学习词条即使排在词典前面，也不能占用手动新增的保底席位"
        );
    }

    #[test]
    fn learned_vocab_does_not_backfill_unused_manual_seats() {
        let entries = vec![
            learned_vocab_entry("learned-low", 1),
            vocab_entry("only-manual", 0),
            learned_vocab_entry("learned-high", 20),
        ];

        let ordered = super::prioritize_vocab_for_asr(entries);

        assert_eq!(ordered, vec!["only-manual", "learned-high", "learned-low"]);
    }

    #[test]
    fn all_learned_vocab_is_ranked_by_hits() {
        let entries = vec![
            learned_vocab_entry("cold", 0),
            learned_vocab_entry("hot", 12),
            learned_vocab_entry("warm", 5),
        ];

        let ordered = super::prioritize_vocab_for_asr(entries);

        assert_eq!(ordered, vec!["hot", "warm", "cold"]);
    }

    #[test]
    fn asr_vocab_dedupes_across_manual_and_learned_sources() {
        let entries = vec![
            vocab_entry("claude", 0),
            learned_vocab_entry("Claude", 33),
            learned_vocab_entry("other", 10),
        ];

        let ordered = super::prioritize_vocab_for_asr(entries);

        assert_eq!(ordered, vec!["Claude", "other"]);
    }

    #[test]
    fn volc_resource_history_label_allows_volc_namespace_ids() {
        // issue #373 场景的两个真实 resource id 必须放行。
        assert_eq!(
            super::volc_resource_history_label("volc.seedasr.sauc.duration").as_deref(),
            Some("volc.seedasr.sauc.duration")
        );
        assert_eq!(
            super::volc_resource_history_label(" volc.bigasr.sauc.duration ").as_deref(),
            Some("volc.bigasr.sauc.duration"),
            "首尾空白应被 trim"
        );
    }

    #[test]
    fn volc_resource_history_label_rejects_non_allowlisted_values() {
        // 非 volc. 命名空间 / 含异常字符 / 超长的值可能携带租户信息，一律不落历史。
        assert_eq!(super::volc_resource_history_label(""), None);
        assert_eq!(super::volc_resource_history_label("my-secret-tenant"), None);
        assert_eq!(
            super::volc_resource_history_label("volc.a b"),
            None,
            "空格不在字符集"
        );
        assert_eq!(
            super::volc_resource_history_label("volc.引擎"),
            None,
            "非 ASCII 拒绝"
        );
        let too_long = format!("volc.{}", "x".repeat(64));
        assert_eq!(super::volc_resource_history_label(&too_long), None);
    }

    use super::dictation::abort_recording_with_error;
    use super::dictation::{
        handle_pressed_edge, handle_released_edge, handle_trigger_combined_async,
    };
    use super::*;
    use crate::types::{HotkeyMode, HotkeyTrigger};
    use once_cell::sync::Lazy;

    static ENV_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

    fn session_id(n: u128) -> SessionId {
        Uuid::from_u128(n)
    }

    #[test]
    fn pipeline_multimodal_enabled_requires_both_flag_and_mode() {
        let mut prefs = crate::types::UserPreferences::default();
        assert!(!super::pipeline_multimodal_enabled(&prefs));
        prefs.multimodal_pipeline_enabled = true;
        assert!(
            !super::pipeline_multimodal_enabled(&prefs),
            "只开实验开关但模式还是 traditional 时不得启用"
        );
        prefs.pipeline_mode = crate::types::PipelineMode::Multimodal;
        assert!(super::pipeline_multimodal_enabled(&prefs));
        prefs.multimodal_pipeline_enabled = false;
        assert!(
            !super::pipeline_multimodal_enabled(&prefs),
            "实验开关关闭时即使模式为 multimodal 也不得启用"
        );
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
        let out =
            format!("{POLISH_TRANSLATE_SRC_MARKER}\n你好。\n{POLISH_TRANSLATE_TGT_MARKER}\n   ");
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
    fn translation_prompt_inherits_active_style_and_preserves_structure() {
        let style_prompt = "# STYLE_PACK_976\n按主题整理为编号列表。\n\n{{HOTWORDS}}";
        let combined = build_polish_translate_system_prompt(style_prompt, "English");
        let (system_prompt, _) = crate::polish::compose_polish_prompts(
            "原始转写",
            PolishMode::Structured,
            &["OpenLess".to_string()],
            &combined,
            &["简体中文".to_string(), "English".to_string()],
            ChineseScriptPreference::Auto,
            crate::types::OutputLanguagePreference::Auto,
            Some("GitHub"),
            Some("已有上下文<|OPENLESS_CURSOR|>"),
            true,
        );

        assert!(system_prompt.contains("STYLE_PACK_976"));
        assert!(system_prompt.contains("OpenLess"));
        assert!(!system_prompt.contains("{{HOTWORDS}}"));
        assert!(system_prompt.contains("English"));
        assert!(system_prompt.contains(POLISH_TRANSLATE_SRC_MARKER));
        assert!(system_prompt.contains(POLISH_TRANSLATE_TGT_MARKER));
        assert!(system_prompt.contains("# ASR 纠错"));
        assert!(system_prompt.contains("Token"));
        assert!(!system_prompt.contains("只输出最终英文译文"));
        assert!(!system_prompt.contains("不得输出中文"));
        assert!(system_prompt.contains("列表、编号、段落和 Markdown 结构"));
        assert!(system_prompt.contains("<cursor_context>"));
        assert!(system_prompt.contains("# 多轮上下文使用规则"));
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
            let mut prefs = coordinator.inner.backend.get_preferences();
            prefs.coding_agent_enabled = true;
            *coordinator.inner.hotkey_runtime_target.lock() = (&prefs).into();
            crate::set_backend_preferences_for_test(&coordinator.inner.backend, prefs);
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

    #[test]
    fn sync_capsule_style_from_preferences_updates_atomic_immediately() {
        // 设置保存路径会调 sync_capsule_style_from_preferences：原子缓存必须立即反映
        // 用户选择，让下一次录音的入场帧就携带新样式——不依赖 emit_capsule 主线程
        // 闭包的 ~30Hz 同步（Windows 主线程拥塞时闭包延迟 → 整场显示旧样式）。
        let coordinator = Coordinator::new();
        coordinator.sync_capsule_style_from_preferences();
        assert_eq!(
            coordinator.inner.host.cached_capsule_style(),
            CapsuleStyle::Siri
        );

        {
            let mut prefs = coordinator.inner.backend.get_preferences();
            prefs.capsule_style = CapsuleStyle::Classic;
            crate::set_backend_preferences_for_test(&coordinator.inner.backend, prefs);
        }
        coordinator.sync_capsule_style_from_preferences();
        assert_eq!(
            coordinator.inner.host.cached_capsule_style(),
            CapsuleStyle::Classic
        );

        {
            let mut prefs = coordinator.inner.backend.get_preferences();
            prefs.capsule_style = CapsuleStyle::Siri;
            crate::set_backend_preferences_for_test(&coordinator.inner.backend, prefs);
        }
        coordinator.sync_capsule_style_from_preferences();
        assert_eq!(
            coordinator.inner.host.cached_capsule_style(),
            CapsuleStyle::Siri
        );
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

        begin_session(&coordinator.inner).await.unwrap();

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

        begin_session(&coordinator.inner).await.unwrap();

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
    fn verbose_json_enabled_only_for_whisper_family() {
        // verbose_json + 幻听过滤只对返回完整 Whisper 指标的 provider 开启。
        assert!(whisper_supports_verbose_json("whisper"));
        assert!(whisper_supports_verbose_json("groq"));
        // SiliconFlow(SenseVoice/TeleSpeech) / Zhipu(GLM-ASR) 保持旧的 json 行为。
        assert!(!whisper_supports_verbose_json("siliconflow"));
        assert!(!whisper_supports_verbose_json("zhipu"));
    }

    #[test]
    fn openai_compatible_preset_is_whisper_compatible_and_conservative_by_default() {
        use crate::asr::whisper::AsrRequestFormat;

        assert!(is_whisper_compatible_provider(
            OPENAI_COMPATIBLE_ASR_PROVIDER_ID
        ));
        assert_eq!(
            active_asr_provider_kind(OPENAI_COMPATIBLE_ASR_PROVIDER_ID),
            ActiveAsrProviderKind::WhisperCompatible
        );
        assert_eq!(
            whisper_request_format(OPENAI_COMPATIBLE_ASR_PROVIDER_ID),
            AsrRequestFormat::Multipart
        );
        assert!(!whisper_uses_hotwords(OPENAI_COMPATIBLE_ASR_PROVIDER_ID));
        // 默认最保守：无 verbose_json、不分片。
        assert_eq!(
            advanced_asr_config_for(OPENAI_COMPATIBLE_ASR_PROVIDER_ID, None),
            AdvancedAsrConfig::default()
        );
    }

    #[test]
    fn openai_compatible_advanced_config_controls_whisper_switches() {
        assert_eq!(
            advanced_asr_config_for(
                OPENAI_COMPATIBLE_ASR_PROVIDER_ID,
                Some(r#"{"verboseJson":true,"chunkDurationMs":30000}"#),
            ),
            AdvancedAsrConfig {
                verbose_json: true,
                chunk_duration_ms: Some(30_000),
                enable_itn: true,
            }
        );
        // 命名厂商忽略该配置，保持硬编码行为。
        assert_eq!(
            advanced_asr_config_for(
                "siliconflow",
                Some(r#"{"verboseJson":true,"chunkDurationMs":30000}"#),
            ),
            AdvancedAsrConfig::default()
        );
        assert!(whisper_supports_verbose_json("whisper"));
        assert!(!whisper_supports_verbose_json("siliconflow"));
        assert_eq!(batch_asr_chunk_limit_ms("siliconflow"), None);
        assert_eq!(
            batch_asr_chunk_limit_ms(OPENAI_COMPATIBLE_ASR_PROVIDER_ID),
            None
        );
    }

    #[test]
    fn parse_advanced_asr_config_falls_back_on_missing_or_invalid_json() {
        assert_eq!(
            parse_advanced_asr_config(None),
            AdvancedAsrConfig::default()
        );
        assert_eq!(
            parse_advanced_asr_config(Some("not-json")),
            AdvancedAsrConfig::default()
        );
        assert_eq!(
            parse_advanced_asr_config(Some(r#"{"verboseJson":true}"#)),
            AdvancedAsrConfig {
                verbose_json: true,
                chunk_duration_ms: None,
                enable_itn: true,
            }
        );
        // 分片时长 0 或缺失 = 不分片。
        assert_eq!(
            parse_advanced_asr_config(Some(r#"{"chunkDurationMs":0}"#)),
            AdvancedAsrConfig::default()
        );
        assert_eq!(
            parse_advanced_asr_config(Some(r#"{"verboseJson":false,"chunkDurationMs":30000}"#)),
            AdvancedAsrConfig {
                verbose_json: false,
                chunk_duration_ms: Some(30_000),
                enable_itn: true,
            }
        );
        // 浮点分片时长与前端一致向下取整：30000.9 → 30000。
        assert_eq!(
            parse_advanced_asr_config(Some(r#"{"chunkDurationMs":30000.9}"#)),
            AdvancedAsrConfig {
                verbose_json: false,
                chunk_duration_ms: Some(30_000),
                enable_itn: true,
            }
        );
        // 负数 / 字符串分片时长 → 不分片。
        assert_eq!(
            parse_advanced_asr_config(Some(r#"{"chunkDurationMs":-1}"#)),
            AdvancedAsrConfig::default()
        );
        assert_eq!(
            parse_advanced_asr_config(Some(r#"{"chunkDurationMs":"abc"}"#)),
            AdvancedAsrConfig::default()
        );
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
    fn zenmux_credential_defaults_and_advanced_config() {
        use crate::asr::whisper::{ZENMUX_DEFAULT_ENDPOINT, ZENMUX_DEFAULT_MODEL};
        // base64 进 JSON body 体积膨胀，与 OpenRouter 同按 30s 切分。
        assert_eq!(batch_asr_chunk_limit_ms("zenmux"), Some(30_000));
        // 空槽默认值：zenmux 回落厂商默认端点/模型（与前端 preset 一致）；
        // 其余 whisper 兼容预设沿用空 endpoint + whisper-1。
        assert_eq!(
            whisper_credential_defaults("zenmux"),
            (
                ZENMUX_DEFAULT_ENDPOINT.to_string(),
                ZENMUX_DEFAULT_MODEL.to_string()
            )
        );
        assert_eq!(
            whisper_credential_defaults("whisper"),
            (String::new(), "whisper-1".to_string())
        );
        assert_eq!(
            whisper_credential_defaults("openrouter"),
            (String::new(), "whisper-1".to_string())
        );

        // enable_itn 默认 true；用户配置显式 false 可覆盖；仅 openai-compatible /
        // zenmux 读用户配置，其余命名厂商忽略（保持硬编码行为）。
        assert!(advanced_asr_config_for("zenmux", None).enable_itn);
        assert!(advanced_asr_config_for("zenmux", Some(r#"{"verboseJson":true}"#)).enable_itn);
        assert!(!advanced_asr_config_for("zenmux", Some(r#"{"enableItn":false}"#)).enable_itn);
        assert!(advanced_asr_config_for("whisper", Some(r#"{"enableItn":false}"#)).enable_itn);
        assert!(!advanced_asr_config_for("zenmux", Some(r#"{"enableItn":false}"#)).verbose_json);
    }

    #[test]
    fn stepfun_is_whisper_compatible_with_hotwords_vocab() {
        use crate::asr::whisper::AsrRequestFormat;
        // StepFun /audio/transcriptions 是标准 multipart（实测 2026-07），
        // response_format 只认 json/text → verbose_json 关闭；100MB 上限
        // （约 54 分钟 16k WAV）→ 无需切分。
        assert!(is_whisper_compatible_provider("stepfun"));
        assert_eq!(
            active_asr_provider_kind("stepfun"),
            ActiveAsrProviderKind::WhisperCompatible
        );
        assert_eq!(
            whisper_request_format("stepfun"),
            AsrRequestFormat::Multipart
        );
        assert!(!whisper_supports_verbose_json("stepfun"));
        assert_eq!(batch_asr_chunk_limit_ms("stepfun"), None);

        // 一入口双协议：`*-stream` 模型路由到实时 WS 客户端，其余留在批式。
        assert_eq!(
            resolve_effective_asr_provider("stepfun", "stepaudio-2.5-asr").unwrap(),
            "stepfun"
        );
        assert_eq!(
            resolve_effective_asr_provider("stepfun", "").unwrap(),
            "stepfun"
        );
        assert_eq!(
            resolve_effective_asr_provider("stepfun", "stepaudio-2.5-asr-stream").unwrap(),
            crate::asr::stepfun_realtime::PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider("stepfun", "step-asr-1.1-stream").unwrap(),
            crate::asr::stepfun_realtime::PROVIDER_ID
        );
        assert_eq!(
            active_asr_provider_kind(crate::asr::stepfun_realtime::PROVIDER_ID),
            ActiveAsrProviderKind::StepfunRealtime
        );

        // 词典路由：StepFun 批式忽略 prompt，走一等 hotwords；其余厂商维持 prompt。
        assert!(whisper_uses_hotwords("stepfun"));
        assert!(!whisper_uses_hotwords("whisper"));
        let phrases = vec!["阶跃星辰".to_string()];
        let (prompt, hotwords) = whisper_vocab_for_provider("stepfun", phrases.clone());
        assert_eq!(prompt, None);
        assert_eq!(hotwords, phrases);
        let (prompt, hotwords) = whisper_vocab_for_provider("groq", phrases);
        assert_eq!(prompt.as_deref(), Some("阶跃星辰."));
        assert!(hotwords.is_empty());
    }

    #[test]
    fn qa_asr_provider_kind_tracks_active_provider() {
        assert_eq!(
            active_asr_provider_kind(crate::asr::bailian::PROVIDER_ID),
            ActiveAsrProviderKind::Bailian
        );
        assert_eq!(
            active_asr_provider_kind(crate::asr::qwen_realtime::PROVIDER_ID),
            ActiveAsrProviderKind::Qwen3Realtime
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
            active_asr_provider_kind(crate::asr::dashscope_multimodal::PROVIDER_ID),
            ActiveAsrProviderKind::DashScopeMultimodal
        );
        assert_eq!(
            active_asr_provider_kind(crate::asr::elevenlabs::PROVIDER_ID),
            ActiveAsrProviderKind::ElevenLabs
        );
        assert_eq!(
            active_asr_provider_kind("volcengine"),
            ActiveAsrProviderKind::Volcengine
        );
        // 未知 id 落到 Volcengine（与构建/凭据分发的兜底一致）。
        assert_eq!(
            active_asr_provider_kind("some-unknown-provider"),
            ActiveAsrProviderKind::Volcengine
        );
    }

    // 锁定分类枚举派生的凭据语义：重构把 ensure_asr_credentials /
    // asr_configured_for_provider 从「字符串白名单 + 静默 else」改成对这两个方法的
    // 穷尽 match，这里逐 kind 钉死映射，防止未来悄悄改动某个 provider 的凭据形态。
    #[test]
    fn preflight_credential_maps_every_kind() {
        use ActiveAsrProviderKind::*;
        use AsrPreflightCredential::*;
        assert_eq!(Bailian.preflight_credential(), AsrApiKey);
        assert_eq!(Qwen3Realtime.preflight_credential(), AsrApiKey);
        assert_eq!(Mimo.preflight_credential(), AsrApiKey);
        assert_eq!(DashScopeMultimodal.preflight_credential(), AsrApiKey);
        assert_eq!(ElevenLabs.preflight_credential(), AsrApiKey);
        assert_eq!(WhisperCompatible.preflight_credential(), AsrApiKey);
        assert_eq!(Volcengine.preflight_credential(), VolcAppKey);
        assert_eq!(Xfyun.preflight_credential(), XfyunAppKey);
    }

    #[test]
    fn resolve_effective_asr_provider_routes_bailian_by_model() {
        let bailian = crate::asr::bailian::PROVIDER_ID;
        // 统一百炼:按模型名路由到底层协议 id。
        assert_eq!(
            resolve_effective_asr_provider(bailian, "fun-asr-realtime").unwrap(),
            crate::asr::bailian::PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider(bailian, "qwen3-asr-flash-realtime").unwrap(),
            crate::asr::qwen_realtime::PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider(bailian, "qwen3-asr-flash-realtime-2026-02-10").unwrap(),
            crate::asr::qwen_realtime::PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider(bailian, "fun-asr-flash-2026-06-15").unwrap(),
            crate::asr::dashscope_multimodal::PROVIDER_ID
        );
        for model in [
            "fun-asr-flash-2026-09-01",
            "qwen3-asr-flash",
            "qwen3-asr-flash-2026-02-10",
            "fun-asr",
            "fun-asr-mtl-2025-08-25",
            "paraformer-v2",
        ] {
            assert_eq!(
                resolve_effective_asr_provider(bailian, model).unwrap(),
                crate::asr::dashscope_multimodal::PROVIDER_ID,
                "unexpected route for {model}"
            );
        }
        assert_eq!(
            resolve_effective_asr_provider(bailian, "fun-asr-flash-8k-realtime").unwrap(),
            crate::asr::bailian::PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider(bailian, "qwen-audio-3.0-asr-flash").unwrap(),
            crate::asr::dashscope_multimodal::PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider(bailian, "paraformer-realtime-v2").unwrap(),
            crate::asr::bailian::PROVIDER_ID
        );
        // 8k 实时变体同样走经典 WebSocket（降采样 + sample_rate=8000），
        // 与 bailian.rs::model_is_8k 的命名空间保持一致。
        assert_eq!(
            resolve_effective_asr_provider(bailian, "paraformer-8k-realtime-v2").unwrap(),
            crate::asr::bailian::PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider(bailian, "sensevoice-8k-realtime-v1").unwrap(),
            crate::asr::bailian::PROVIDER_ID
        );
        // 空模型 → 经典实时（百炼默认）；未知模型应被拒绝。
        assert_eq!(
            resolve_effective_asr_provider(bailian, "").unwrap(),
            crate::asr::bailian::PROVIDER_ID
        );
        // 非百炼 provider 原样返回（隐藏别名与其它厂商各走各的旧路径）。
        assert_eq!(
            resolve_effective_asr_provider(crate::asr::qwen_realtime::PROVIDER_ID, "anything")
                .unwrap(),
            crate::asr::qwen_realtime::PROVIDER_ID
        );
        assert_eq!(
            resolve_effective_asr_provider("whisper", "whisper-1").unwrap(),
            "whisper"
        );
    }

    #[test]
    fn resolve_effective_asr_provider_rejects_unsupported_bailian_model() {
        let error = resolve_effective_asr_provider(crate::asr::bailian::PROVIDER_ID, "unknown-asr")
            .unwrap_err();
        assert!(error.contains("不支持的百炼 ASR 模型"));
        // qwen3-asr-flash-filetrans 仅接受公网 URL，与本地录音链路不兼容，同样拒绝。
        let error = resolve_effective_asr_provider(
            crate::asr::bailian::PROVIDER_ID,
            "qwen3-asr-flash-filetrans",
        )
        .unwrap_err();
        assert!(error.contains("不支持的百炼 ASR 模型"));
    }

    #[test]
    fn validates_only_supported_dashscope_multimodal_models() {
        assert!(validate_dashscope_multimodal_model("").is_ok());
        assert!(validate_dashscope_multimodal_model("fun-asr-flash-2026-06-15").is_ok());
        assert!(validate_dashscope_multimodal_model("qwen-audio-3.0-asr-flash").is_ok());
        assert!(validate_dashscope_multimodal_model("qwen-audio-3.0-asr-flash-streaming").is_err());
    }

    #[test]
    fn derive_bailian_endpoint_preserves_region_host_and_selects_protocol_path() {
        let endpoint = "https://workspace.ap-southeast-1.maas.aliyuncs.com/custom?x=1";
        assert_eq!(
            derive_bailian_endpoint(endpoint, BailianEndpointProtocol::ClassicRealtime).unwrap(),
            "wss://workspace.ap-southeast-1.maas.aliyuncs.com/api-ws/v1/inference/"
        );
        assert_eq!(
            derive_bailian_endpoint(endpoint, BailianEndpointProtocol::QwenRealtime).unwrap(),
            "wss://workspace.ap-southeast-1.maas.aliyuncs.com/api-ws/v1/realtime"
        );
        assert_eq!(
            derive_bailian_endpoint(endpoint, BailianEndpointProtocol::Multimodal).unwrap(),
            "https://workspace.ap-southeast-1.maas.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation"
        );
        assert_eq!(
            derive_bailian_endpoint(endpoint, BailianEndpointProtocol::AsyncTranscription)
                .unwrap(),
            "https://workspace.ap-southeast-1.maas.aliyuncs.com/api/v1/services/audio/asr/transcription"
        );
    }

    #[test]
    fn derive_bailian_endpoint_uses_protocol_default_for_empty_value() {
        assert_eq!(
            derive_bailian_endpoint("", BailianEndpointProtocol::QwenRealtime).unwrap(),
            crate::asr::qwen_realtime::DEFAULT_ENDPOINT
        );
    }

    #[test]
    fn configured_fields_maps_every_kind() {
        use ActiveAsrProviderKind::*;
        use AsrConfiguredFields::*;
        assert_eq!(Bailian.configured_fields(), ApiKeyOnly);
        assert_eq!(Qwen3Realtime.configured_fields(), ApiKeyOnly);
        assert_eq!(Mimo.configured_fields(), ApiKeyEndpointModel);
        assert_eq!(DashScopeMultimodal.configured_fields(), ApiKeyEndpointModel);
        assert_eq!(ElevenLabs.configured_fields(), ApiKeyOnly);
        assert_eq!(WhisperCompatible.configured_fields(), EndpointModelOnly);
        assert_eq!(Volcengine.configured_fields(), VolcAppKey);
        assert_eq!(Xfyun.configured_fields(), XfyunAppKey);
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

    #[test]
    fn windows_local_asr_timeout_floors_at_global_timeout_for_short_audio() {
        assert_eq!(
            windows_local_asr_transcribe_timeout(5.0),
            std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn windows_local_asr_timeout_scales_with_audio_duration() {
        // 65s 录音：65 × 1.0 = 65，+20 = 85s。长音频不再撞 30s 墙。
        assert_eq!(
            windows_local_asr_transcribe_timeout(65.0),
            std::time::Duration::from_secs(85)
        );
    }

    #[test]
    fn local_qwen_timeout_floors_at_global_timeout_for_short_audio() {
        // 5s 录音：5 × 0.6 = 3, +10 = 13, max(30) = 30。短录音兜底。
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
        // 10.1s 录音：10.1 × 0.6 = 6.06, ceil = 7, +10 = 17, max(30) = 30。
        // COORDINATOR_GLOBAL_TIMEOUT_SECS 提升到 30 后，短音频统一被兜底值覆盖。
        assert_eq!(
            local_qwen_transcribe_timeout(10.1),
            std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn local_qwen_timeout_handles_zero_duration() {
        // 0 时长（空 buffer 边界）：0 × 0.6 = 0, +10 = 10, max(30) = 30。
        assert_eq!(
            local_qwen_transcribe_timeout(0.0),
            std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn whisper_timeout_floors_at_global_timeout_for_short_audio() {
        // 10s 录音：10 × 0.5 = 5, +20 = 25, max(30) = 30。短音频兜底。
        assert_eq!(
            whisper_transcribe_timeout(10.0),
            std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn whisper_timeout_scales_with_audio_duration() {
        // 60s 录音：60 × 0.5 = 30, +20 = 50。覆盖多分片 HTTP 请求。
        assert_eq!(
            whisper_transcribe_timeout(60.0),
            std::time::Duration::from_secs(50)
        );
    }

    #[test]
    fn whisper_timeout_ceils_partial_seconds() {
        // 45.3s 录音：45.3 × 0.5 = 22.65, ceil = 23, +20 = 43, max(30) = 43。
        assert_eq!(
            whisper_transcribe_timeout(45.3),
            std::time::Duration::from_secs(43)
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn foundry_release_uses_foundry_keep_loaded_preference() {
        let runtime = Arc::new(crate::asr::local::FoundryLocalRuntime::new());
        let coordinator = Coordinator::new_with_foundry_runtime(runtime);
        let mut prefs = coordinator.inner.backend.get_preferences();
        prefs.local_asr_keep_loaded_secs = 3;
        prefs.foundry_local_asr_keep_loaded_secs = 7;
        crate::set_backend_preferences_for_test(&coordinator.inner.backend, prefs);

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
    fn local_asr_release_guard_rejects_stale_selection_voice_session() {
        let runtime = Arc::new(crate::asr::local::FoundryLocalRuntime::new());
        let coordinator = Coordinator::new_with_foundry_runtime(runtime);
        let old_session_id = new_session_id();
        selection_voice_session::set_selection_voice_recorder_session_for_test(
            &coordinator.inner,
            Some(old_session_id),
        );

        assert!(asr_release_session_is_current(
            &coordinator.inner,
            AsrReleaseSession::SelectionVoice(old_session_id)
        ));

        selection_voice_session::set_selection_voice_recorder_session_for_test(
            &coordinator.inner,
            Some(new_session_id()),
        );

        assert!(!asr_release_session_is_current(
            &coordinator.inner,
            AsrReleaseSession::SelectionVoice(old_session_id)
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

    #[test]
    fn resolve_ark_endpoint_allows_any_custom_endpoint() {
        // 地址选择权完全交给用户：http 域名、局域网 IP、元数据地址均放行，
        // 前端对 http:// 输入展示明文风险提示。
        let endpoint = resolve_ark_endpoint_with_policy(
            "",
            Some("http://example.com:12345/v1/chat/completions".to_string()),
        )
        .expect("custom LLM HTTP hostname with a custom port must remain usable");
        assert_eq!(endpoint, "http://example.com:12345/v1/chat/completions");

        resolve_ark_endpoint_with_policy(
            "",
            Some("http://192.168.1.50:12345/v1/chat/completions".to_string()),
        )
        .expect("custom LLM LAN HTTP endpoint must remain usable");

        resolve_ark_endpoint_with_policy(
            "",
            Some("http://169.254.169.254/latest/meta-data/".to_string()),
        )
        .expect("user-explicitly-configured endpoint must be allowed (user decides)");
    }

    #[test]
    fn resolve_ark_endpoint_rejects_malformed_endpoint() {
        let error = resolve_ark_endpoint_with_policy(
            "",
            Some("ftp://example.com/v1/chat/completions".to_string()),
        )
        .expect_err("non-http(s) scheme must be rejected");
        assert!(error.to_string().contains("http 或 https"));
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

        end_session(&coordinator.inner).await.unwrap();

        let state = coordinator.inner.state.lock();
        assert_eq!(state.phase, SessionPhase::Starting);
        assert!(state.pending_stop);
    }

    #[tokio::test]
    async fn stop_dictation_from_listening_without_asr_returns_idle_and_hides_capsule() {
        let coordinator = Coordinator::new();
        {
            let mut state = coordinator.inner.state.lock();
            state.phase = SessionPhase::Listening;
            state.session_id = session_id(123);
        }

        end_session(&coordinator.inner).await.unwrap();

        assert_eq!(coordinator.inner.state.lock().phase, SessionPhase::Idle);
        tokio::time::sleep(std::time::Duration::from_millis(
            CAPSULE_AUTO_HIDE_DELAY_MS + 100,
        ))
        .await;
        assert_eq!(
            coordinator
                .inner
                .last_capsule_state
                .lock()
                .as_ref()
                .copied(),
            Some(CapsuleState::Idle),
            "无 ASR 句柄的停止路径也必须调度胶囊隐藏"
        );
    }

    #[tokio::test]
    async fn stale_capsule_idle_schedule_does_not_hide_newer_state() {
        let coordinator = Coordinator::new();
        // 旧 schedule 触发时若期间有更新的 emit，应跳过隐藏（voice agent 取消双 emit 竞争）。
        emit_capsule(&coordinator.inner, CapsuleState::Done, 0.0, 0, None, None);
        schedule_capsule_idle(&coordinator.inner, 30);
        emit_capsule(
            &coordinator.inner,
            CapsuleState::Cancelled,
            0.0,
            0,
            None,
            None,
        );
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert_eq!(
            coordinator
                .inner
                .last_capsule_state
                .lock()
                .as_ref()
                .copied(),
            Some(CapsuleState::Cancelled),
            "旧 schedule 不应把更新的 Cancelled 状态提前隐藏"
        );
    }

    #[tokio::test]
    async fn capsule_idle_schedule_hides_when_no_newer_state() {
        let coordinator = Coordinator::new();
        emit_capsule(&coordinator.inner, CapsuleState::Done, 0.0, 0, None, None);
        schedule_capsule_idle(&coordinator.inner, 30);
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert_eq!(
            coordinator
                .inner
                .last_capsule_state
                .lock()
                .as_ref()
                .copied(),
            Some(CapsuleState::Idle),
            "无新 emit 时 schedule 应隐藏胶囊"
        );
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

            cancel_session(&coordinator.inner);

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

        handle_pressed_edge(&coordinator.inner, std::time::Instant::now(), 1).await;

        let state = coordinator.inner.state.lock();
        assert_eq!(state.phase, SessionPhase::Inserting);
        assert_eq!(state.session_id, session_id(41));
    }

    // #856：识别中按下热键想录下一条的 Pressed 会在会话收尾后被串行 bridge 取出（落在
    // 冷却期内）—— 现在一律静默丢弃，不再像「排队接力」那样放行开录下一条（无反馈排队 +
    // 延迟开录的惊吓成本大于省下的等待时间；Esc 取消后也不会因此再弹出一条新录音）。
    #[tokio::test]
    async fn toggle_press_within_cooldown_is_dropped() {
        let coordinator = Coordinator::new();
        // Coordinator::new() 读取真实持久化偏好；测试必须固定自己的模式，不能让本机
        // 当前设置（例如 Hold/Auto）改变该用例验证的 Toggle 冷却语义。
        crate::set_backend_preferences_for_test(
            &coordinator.inner.backend,
            crate::types::UserPreferences {
                hotkey: crate::types::HotkeyBinding {
                    trigger: HotkeyTrigger::RightControl,
                    mode: HotkeyMode::Toggle,
                    keys: None,
                },
                ..Default::default()
            },
        );
        // Idle + 冷却未过期：模拟「识别中按下 → 会话收尾 → bridge 取出该 Pressed」的时刻。
        *coordinator.inner.session_cooldown_until.lock() = Some(
            std::time::Instant::now() + std::time::Duration::from_millis(POST_SESSION_COOLDOWN_MS),
        );

        handle_pressed_edge(&coordinator.inner, std::time::Instant::now(), 1).await;

        // 静默丢弃：没有开录下一条（phase 仍是 Idle）。
        assert_eq!(coordinator.inner.state.lock().phase, SessionPhase::Idle);
    }

    #[tokio::test]
    async fn repeated_pressed_edge_during_hold_session_does_not_restart() {
        let coordinator = Coordinator::new();
        crate::set_backend_preferences_for_test(
            &coordinator.inner.backend,
            crate::types::UserPreferences {
                hotkey: crate::types::HotkeyBinding {
                    trigger: HotkeyTrigger::RightControl,
                    mode: HotkeyMode::Hold,
                    keys: None,
                },
                ..Default::default()
            },
        );
        coordinator.inner.state.lock().phase = SessionPhase::Listening;
        coordinator
            .inner
            .hotkey_trigger_held
            .store(true, Ordering::SeqCst);

        handle_pressed_edge(&coordinator.inner, std::time::Instant::now(), 1).await;

        assert_eq!(
            coordinator.inner.state.lock().phase,
            SessionPhase::Listening
        );
        assert!(coordinator.inner.hotkey_trigger_held.load(Ordering::SeqCst));
    }

    fn set_auto_mode(coordinator: &Coordinator) {
        crate::set_backend_preferences_for_test(
            &coordinator.inner.backend,
            crate::types::UserPreferences {
                hotkey: crate::types::HotkeyBinding {
                    trigger: HotkeyTrigger::RightControl,
                    mode: HotkeyMode::Auto,
                    keys: None,
                },
                ..Default::default()
            },
        );
    }

    // Auto 模式短按：松手时按住时长 < 阈值 → 锁存为切换态，保持 Listening（不结束会话）。
    #[tokio::test]
    async fn auto_short_tap_release_latches_recording() {
        let coordinator = Coordinator::new();
        set_auto_mode(&coordinator);
        coordinator.inner.state.lock().phase = SessionPhase::Listening;
        // 刚按下（elapsed ≈ 0 < 350ms）→ 短按。
        let pressed_at = std::time::Instant::now();
        *coordinator.inner.hotkey_press_at.lock() = Some(pressed_at);
        coordinator
            .inner
            .hotkey_trigger_held
            .store(true, Ordering::SeqCst);

        handle_released_edge(
            &coordinator.inner,
            pressed_at + std::time::Duration::from_millis(100),
        )
        .await;

        // 短按松手不结束录音，等下一次按下再停。
        assert_eq!(
            coordinator.inner.state.lock().phase,
            SessionPhase::Listening
        );
    }

    #[tokio::test]
    async fn auto_short_tap_stays_latched_when_bridge_handles_release_late() {
        let coordinator = Coordinator::new();
        set_auto_mode(&coordinator);
        coordinator.inner.state.lock().phase = SessionPhase::Listening;
        let pressed_at = std::time::Instant::now();
        *coordinator.inner.hotkey_press_at.lock() = Some(pressed_at);
        coordinator
            .inner
            .hotkey_trigger_held
            .store(true, Ordering::SeqCst);

        // 模拟上一条会话阻塞 bridge：处理发生在物理松手很久之后。
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        handle_released_edge(
            &coordinator.inner,
            pressed_at + std::time::Duration::from_millis(100),
        )
        .await;

        assert_eq!(
            coordinator.inner.state.lock().phase,
            SessionPhase::Listening
        );
        assert!(coordinator.inner.hotkey_press_at.lock().is_none());
    }

    // 迁移边界：热键业务相位由共享 Core 独占；旧 Coordinator phase 不得驱动 Auto 收尾。
    #[tokio::test]
    async fn auto_release_ignores_legacy_only_session_state() {
        let coordinator = Coordinator::new();
        set_auto_mode(&coordinator);
        coordinator.inner.state.lock().phase = SessionPhase::Listening;
        // 按住已超过阈值 → 长按。
        let pressed_at = std::time::Instant::now();
        *coordinator.inner.hotkey_press_at.lock() = Some(pressed_at);
        coordinator
            .inner
            .hotkey_trigger_held
            .store(true, Ordering::SeqCst);

        handle_released_edge(
            &coordinator.inner,
            pressed_at + std::time::Duration::from_millis(500),
        )
        .await;

        assert_eq!(
            coordinator.inner.state.lock().phase,
            SessionPhase::Listening
        );
        assert!(coordinator.inner.hotkey_press_at.lock().is_none());
    }

    // Option+任意字母/数字键：这次按下开出来的会话必须被撤销，且随后的松手边沿不能再被当成
    // Auto 短按锁存（否则录音一直开着，正是用户报的「按 Option+其他键唤起听写」）。
    #[tokio::test]
    async fn trigger_combined_clears_press_bookkeeping_without_mutating_legacy_state() {
        let coordinator = Coordinator::new();
        set_auto_mode(&coordinator);
        coordinator.inner.state.lock().phase = SessionPhase::Listening;
        let pressed_at = std::time::Instant::now();
        *coordinator.inner.hotkey_press_at.lock() = Some(pressed_at);
        coordinator
            .inner
            .hotkey_trigger_held
            .store(true, Ordering::SeqCst);
        coordinator
            .inner
            .hotkey_press_generation
            .store(1, Ordering::SeqCst);
        coordinator
            .inner
            .hotkey_press_began_session
            .store(1, Ordering::SeqCst);

        handle_trigger_combined_async(&coordinator.inner, 1).await;

        assert_eq!(
            coordinator.inner.state.lock().phase,
            SessionPhase::Listening
        );
        assert!(!coordinator.inner.hotkey_trigger_held.load(Ordering::SeqCst));
        assert!(coordinator.inner.hotkey_press_at.lock().is_none());
        // 组合键误触不算「刚用完一次听写」：不留冷却，否则紧接着真想说话的按下被吞。
        assert!(coordinator.inner.session_cooldown_until.lock().is_none());

        handle_released_edge(
            &coordinator.inner,
            pressed_at + std::time::Duration::from_millis(80),
        )
        .await;

        assert_eq!(
            coordinator.inner.state.lock().phase,
            SessionPhase::Listening
        );
    }

    // 这次按下是 toggle 停止（没开出会话）时，组合键撤销不能顺手取消正在跑的会话 ——
    // 那条录音是上一次按下锁存的，取消 = 用户白说一段。
    #[tokio::test]
    async fn trigger_combined_leaves_session_it_did_not_start() {
        let coordinator = Coordinator::new();
        set_auto_mode(&coordinator);
        coordinator.inner.state.lock().phase = SessionPhase::Listening;
        coordinator
            .inner
            .hotkey_trigger_held
            .store(true, Ordering::SeqCst);
        coordinator
            .inner
            .hotkey_press_generation
            .store(1, Ordering::SeqCst);
        coordinator
            .inner
            .hotkey_press_began_session
            .store(0, Ordering::SeqCst);

        handle_trigger_combined_async(&coordinator.inner, 1).await;

        assert_eq!(
            coordinator.inner.state.lock().phase,
            SessionPhase::Listening
        );
        assert!(!coordinator.inner.hotkey_trigger_held.load(Ordering::SeqCst));
    }

    // 组合键撤销通道独立于 Released；若正常松手已经把会话收尾到 Idle，迟到的撤销
    // 不能清掉正常会话的冷却/防抖，否则下一次三连按会绕过 #545 的保护。
    #[tokio::test]
    async fn late_trigger_combined_does_not_clear_completed_session_guards() {
        let coordinator = Coordinator::new();
        set_auto_mode(&coordinator);
        let now = std::time::Instant::now();
        *coordinator.inner.session_cooldown_until.lock() =
            Some(now + std::time::Duration::from_secs(1));
        *coordinator.inner.last_hotkey_dispatch_at.lock() = Some(now);
        coordinator
            .inner
            .hotkey_press_generation
            .store(1, Ordering::SeqCst);
        coordinator
            .inner
            .hotkey_press_began_session
            .store(1, Ordering::SeqCst);

        handle_trigger_combined_async(&coordinator.inner, 1).await;

        assert_eq!(coordinator.inner.state.lock().phase, SessionPhase::Idle);
        assert!(coordinator.inner.session_cooldown_until.lock().is_some());
        assert!(coordinator.inner.last_hotkey_dispatch_at.lock().is_some());
    }

    // 撤销走独立线程后，它与 Pressed/Released 那条串行 bridge 之间没有先后保证。
    // 万一 Released 抢先跑完（把按住态清了、Auto 还锁存成了切换态），撤销仍然必须认出
    // 这条会话是自己那次按下开的并取消掉 —— 否则组合键会留下一条停不下来的录音，
    // 正是本 PR 要修的老毛病换个形式复发。
    #[tokio::test]
    async fn trigger_combined_clears_bookkeeping_after_legacy_release_race() {
        let coordinator = Coordinator::new();
        set_auto_mode(&coordinator);
        coordinator.inner.state.lock().phase = SessionPhase::Listening;
        let pressed_at = std::time::Instant::now();
        *coordinator.inner.hotkey_press_at.lock() = Some(pressed_at);
        coordinator
            .inner
            .hotkey_trigger_held
            .store(true, Ordering::SeqCst);
        coordinator
            .inner
            .hotkey_press_generation
            .store(1, Ordering::SeqCst);
        coordinator
            .inner
            .hotkey_press_began_session
            .store(1, Ordering::SeqCst);

        // 先跑 Released（短按 → Auto 锁存成切换态，录音继续），撤销后到。
        handle_released_edge(
            &coordinator.inner,
            pressed_at + std::time::Duration::from_millis(80),
        )
        .await;
        assert_eq!(
            coordinator.inner.state.lock().phase,
            SessionPhase::Listening
        );

        handle_trigger_combined_async(&coordinator.inner, 1).await;

        assert_eq!(
            coordinator.inner.state.lock().phase,
            SessionPhase::Listening
        );
        assert!(coordinator.inner.session_cooldown_until.lock().is_none());
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
            true,
            InsertStatus::PasteSent
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
            dictation_error_code(
                InsertStatus::Failed,
                false,
                false,
                false,
                crate::types::WindowsInsertionMode::Tsf,
            ),
            Some("focusRestoreFailed")
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn tsf_required_failure_keeps_tsf_error_when_focus_was_ready() {
        assert_eq!(
            dictation_error_code(
                InsertStatus::Failed,
                false,
                true,
                false,
                crate::types::WindowsInsertionMode::Tsf,
            ),
            Some("windowsImeTsfRequired")
        );
    }

    #[test]
    fn sendinput_only_mode_skips_tsf_required_error() {
        assert_eq!(
            dictation_error_code(
                InsertStatus::Failed,
                false,
                true,
                false,
                crate::types::WindowsInsertionMode::SendInput,
            ),
            None
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

    #[test]
    fn selection_polish_capsule_epoch_rejects_stale_auto_hide() {
        let coordinator = Coordinator::new();
        let terminal_epoch =
            emit_selection_polish_capsule(&coordinator.inner, CapsuleState::Done, "已替换");
        assert!(selection_polish_capsule_epoch_is_current(
            &coordinator.inner,
            terminal_epoch
        ));

        let next_epoch = emit_selection_polish_capsule(
            &coordinator.inner,
            CapsuleState::Polishing,
            "正在润色...",
        );
        assert_ne!(terminal_epoch, next_epoch);
        assert!(
            !selection_polish_capsule_epoch_is_current(&coordinator.inner, terminal_epoch),
            "上一轮的终态 timer 不能收起下一轮处理中提示"
        );
        assert!(selection_polish_capsule_epoch_is_current(
            &coordinator.inner,
            next_epoch
        ));

        emit_capsule(
            &coordinator.inner,
            CapsuleState::Recording,
            0.0,
            0,
            None,
            None,
        );
        assert!(
            !selection_polish_capsule_epoch_is_current(&coordinator.inner, next_epoch),
            "选区终态 timer 不能在新的语音状态上调用 Idle"
        );
    }
}

fn enabled_phrases(inner: &Arc<Inner>) -> Vec<String> {
    inner
        .backend
        .list_vocabulary()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.enabled)
        .map(|e| e.phrase)
        .collect()
}

/// 词典启用词条，**按送进 ASR 词汇偏置的优先级排好序**。
///
/// LLM 侧的热词块没有名额限制（[`enabled_phrases`] 直接用词典顺序就行），ASR 侧
/// 有：`whisper::PROMPT_CHAR_BUDGET` 只给 240 个字符，装不下的词条被直接丢弃。
/// 于是「送进去的顺序」就等于「谁能被听见」。
///
/// 而词典本身的顺序是**最近添加的在最前**（[`DictionaryStore::add`] 用
/// `insert(0)`，为的是词汇表页面把刚加的词排在上面）。两个各自都合理的决定撞在
/// 一起，结果是预算永远优先喂给最新的词，最老的先掉出去——而最老的那批恰恰是
/// 攒了最多命中的常用词。真机上的表现：一份 40 条的词典里，命中 18 次、7 次、
/// 10 次的三个专有名词全部排在预算外，从来没送到过 ASR；用户在词汇表里看得见
/// 它们、以为在生效，实际上一次都没生效过。
///
/// 排序规则：
/// 1. 最近手动添加的前 [`FRESH_VOCAB_SEATS`] 条保底——刚加的词还没机会攒命中，纯按
///    命中排会让它永远进不去，而用户刚加它多半就是因为刚被它坑过。手改学习词条不占
///    这些席位；它们本来就可能是半截词，必须靠真实命中自己爬进预算。
/// 2. 其余按命中次数降序。
/// 3. 同词异形（`claude` / `Claude`）只留命中多的那个写法。
fn asr_vocab_phrases(inner: &Arc<Inner>) -> Vec<String> {
    let entries: Vec<crate::types::DictionaryEntry> = inner
        .backend
        .list_vocabulary()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.enabled)
        .collect();
    prioritize_vocab_for_asr(entries)
}

/// 最近添加的词条无条件占住的名额，见 [`asr_vocab_phrases`]。
const FRESH_VOCAB_SEATS: usize = 5;

/// [`asr_vocab_phrases`] 的纯函数部分，方便直接测排序规则。
///
/// `entries` 必须是词典的原始顺序（最近添加在前）——保底席位靠它取「最近」，
/// 不去解析 `created_at` 字符串（历史文件由 Swift 版写入，格式不保证一致）。
fn prioritize_vocab_for_asr(entries: Vec<crate::types::DictionaryEntry>) -> Vec<String> {
    let mut fresh_manual = Vec::with_capacity(FRESH_VOCAB_SEATS.min(entries.len()));
    let mut ranked = Vec::with_capacity(entries.len());
    for entry in entries {
        let learned = entry.note.as_deref() == Some(crate::types::LEARNED_VOCAB_NOTE);
        if !learned && fresh_manual.len() < FRESH_VOCAB_SEATS {
            fresh_manual.push(entry);
        } else {
            ranked.push(entry);
        }
    }
    // 保底席位之外的全部词条按命中降序；`sort_by_key` 是稳定排序，同命中次数的保持
    // 词典原顺序（最近添加在前）。学习词条也在这里，不会被拿来填空缺的手动保底席位。
    ranked.sort_by_key(|e| std::cmp::Reverse(e.hits));
    fresh_manual.extend(ranked);
    let ordered = fresh_manual;

    // 同一个词的不同写法（`claude` / `Claude`）只留一个：既省预算，也免得两种
    // 写法一起进词表让模型无所适从。留**命中多**的那个写法，但位置取最靠前那次
    // ——否则一个刚被收进来、命中为 0 的小写变体会把攒了几十次命中的正确写法顶掉。
    let mut best: std::collections::HashMap<String, (usize, crate::types::DictionaryEntry)> =
        std::collections::HashMap::new();
    for (index, entry) in ordered.into_iter().enumerate() {
        let key = entry.phrase.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        match best.entry(key) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert((index, entry));
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if entry.hits > slot.get().1.hits {
                    let position = slot.get().0;
                    slot.insert((position, entry));
                }
            }
        }
    }

    let mut picked: Vec<(usize, String)> = best
        .into_values()
        .map(|(index, entry)| (index, entry.phrase))
        .collect();
    picked.sort_by_key(|(index, _)| *index);
    picked.into_iter().map(|(_, phrase)| phrase).collect()
}

/// 终止态（Done / Error）后延迟 N ms 把胶囊改回 Idle，让浮窗自动消失。
/// 点 ✓ / 中途出错走这里，保留 2 秒让用户看清结果 / 错误提示。
const CAPSULE_AUTO_HIDE_DELAY_MS: u64 = 2000;

/// 用户主动取消（Esc / 点 ✕）时的收起延迟。取消是明确的「我不要了」意图，
/// 不需要像 Done/Error 那样停留 2 秒给用户读——立刻回 Idle，由前端 capsule-out
/// 淡出动画（520ms）负责优雅收尾，观感上「按下即消失」（对齐 Typeless）。
const CAPSULE_CANCEL_HIDE_DELAY_MS: u64 = 0;

/// Toggle 模式下，end_session 将 phase 设为 Idle 后在此时间内禁止新的 begin_session。
/// 避免用户三连按时第 3 次按下误激活新听写（此时胶囊仍在离场动画周期内）。
/// 值取 capsule EXIT_ANIM_MS (360ms) + 余量 ≈ 600ms。
const POST_SESSION_COOLDOWN_MS: u64 = 600;

/// Coordinator 全局超时保护：防止 ASR await_final_result() 永远挂起。
/// 设置为 30 秒，为云端 batch ASR（OpenRouter Whisper 等）提供足够的
/// 网络超时预算；只在 ASR 自身超时机制失效时作为最后的防线触发。
const COORDINATOR_GLOBAL_TIMEOUT_SECS: u64 = 30;

/// Windows 本地 batch ASR 的动态转写超时。Foundry 与 sherpa-onnx 当前使用
/// 同一预算：短音频至少 30s，长音频按整段时长向上取整后增加 20s 余量。
fn windows_local_asr_transcribe_timeout(audio_secs: f64) -> std::time::Duration {
    let secs = (audio_secs.ceil() as u64)
        .saturating_add(20)
        .max(COORDINATOR_GLOBAL_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

/// 本地 Qwen3-ASR 的动态转写超时。固定 15 秒在长录音（≥ 30s）+ 慢机器
/// （RTF ≈ 0.3–0.5）上必然超时把整段内容丢掉。改用 max(15, ceil(audio_s
/// × 0.6) + 10)：基础保留 15s 兜住短录音；长录音按音频长度的 0.6 倍 +
/// 10s 余量，覆盖 RTF ≤ 0.5 的机器。
fn local_qwen_transcribe_timeout(audio_secs: f64) -> std::time::Duration {
    let secs = ((audio_secs * 0.6).ceil() as u64)
        .saturating_add(10)
        .max(COORDINATOR_GLOBAL_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

fn local_whisper_transcribe_timeout(audio_secs: f64) -> std::time::Duration {
    let secs = ((audio_secs * 0.5).ceil() as u64)
        .saturating_add(10)
        .max(15);
    std::time::Duration::from_secs(secs)
}

/// 检查 begin_session 的 await 间隙是否被 cancel_session 打断。
/// 必须在持有 state lock 的瞬间读，结果一拿就过期，所以用 helper 名字提醒只在
/// 「准备做下一步副作用前」用。
fn startup_race_status_for_starting(
    inner: &Arc<Inner>,
    captured_session_id: SessionId,
) -> StartupRaceStatus {
    let state = inner.state.lock();
    startup_race_status(&state, captured_session_id)
}

fn set_phase_idle_if_session_matches(inner: &Arc<Inner>, session_id: SessionId) {
    let mut state = inner.state.lock();
    if state.session_id == session_id {
        state.phase = SessionPhase::Idle;
    }
}

fn schedule_capsule_idle(inner: &Arc<Inner>, delay_ms: u64) {
    // 记录触发时胶囊显示的状态；到点时若期间有更新的 emit（last_capsule_state 已变），
    // 说明本次状态已被后续 emit 取代，隐藏交给那次 emit 自己的 schedule——避免旧
    // schedule 把新状态提前隐藏（如 voice agent 取消路径 cancel_session 与收尾双 emit）。
    let expect = inner.last_capsule_state.lock().as_ref().copied();
    let inner_clone = Arc::clone(inner);
    inner.host.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        if inner_clone.last_capsule_state.lock().as_ref().copied() != expect {
            return;
        }
        // 必须 dictation **和** QA 同时空闲才能隐藏胶囊。否则旧 dictation Done timer
        // 的尾巴会在新 QA 录音/思考中把胶囊意外收掉（issue #118 v2 复现）。
        // 选区润色进行中或出现新 payload 时，函数内部依据 capsule epoch 放弃隐藏。
        hide_capsule_if_all_sessions_idle(&inner_clone);
    });
}

/// 选区润色终态的短暂展示。旧的 timer 只能收起自己那一代的 payload；若用户已经
/// 触发了下一轮 selection，或在此期间开始语音/QA，会直接放弃，不碰当前 capsule。
#[cfg(not(mobile))]
fn schedule_selection_polish_capsule_idle(inner: &Arc<Inner>, event_epoch: u64, delay_ms: u64) {
    let inner_clone = Arc::clone(inner);
    inner.host.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        hide_selection_polish_capsule_if_current(&inner_clone, event_epoch);
    });
}

// ─────────────────────────── audio bridge ───────────────────────────

struct DeferredAsrBridge {
    state: Mutex<DeferredAsrState>,
}

struct DeferredAsrState {
    target: Option<Arc<dyn crate::asr::AudioConsumer>>,
    pending_audio: Vec<u8>,
    attaching: bool,
}

impl DeferredAsrBridge {
    fn new() -> Self {
        Self {
            state: Mutex::new(DeferredAsrState {
                target: None,
                pending_audio: Vec::new(),
                attaching: false,
            }),
        }
    }

    fn attach(&self, target: Arc<dyn crate::asr::AudioConsumer>) -> usize {
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
