//! Apple Speech 本地 ASR 适配器（macOS，issue #574）。
//!
//! 把 Apple 的 `SFSpeechRecognizer` 当作第 4 个本地 provider，接入链路与
//! `LocalQwenAsr` 完全同形：实现 `crate::recorder::AudioConsumer` 把 PCM
//! 累进缓冲，`transcribe()` 返回 `RawTranscript{text, duration_ms}`。
//!
//! **首版批处理**：把缓冲的 16k/mono/16-bit PCM 用 `encode_wav_16k_mono`
//! 写成临时 wav，喂给 `SFSpeechURLRecognitionRequest`。这样避开
//! `AVAudioPCMBuffer` / `AVAudioFormat` 的 objc2 桥接，换取实现确定性。
//! 实时 partial 流式列为后续增量，不在本次范围。
//!
//! 权限走 `SFSpeechRecognizer.requestAuthorization:`（completion handler
//! block），范式照抄 `permissions.rs` 的 `requestAccessForMediaType:`。
//! 未授权时 `transcribe()` 返回清晰错误。
//!
//! 非 macOS 平台不编译本模块（见 `mod.rs` 的 cfg 门控）。

#![cfg(target_os = "macos")]

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use parking_lot::Mutex;

use crate::asr::wav::encode_wav_16k_mono;
use crate::asr::RawTranscript;

/// `SFSpeechRecognizerAuthorizationStatus`（NS_ENUM(NSInteger)）。
const SF_AUTH_NOT_DETERMINED: i64 = 0;
const SF_AUTH_DENIED: i64 = 1;
const SF_AUTH_RESTRICTED: i64 = 2;
const SF_AUTH_AUTHORIZED: i64 = 3;

/// 等待识别 / 授权回调的兜底超时。识别本身另有 coordinator 侧动态超时；
/// 这里只防 block 永不回调导致线程永久阻塞。
const RECOGNITION_WAIT: Duration = Duration::from_secs(60);
const AUTHORIZATION_WAIT: Duration = Duration::from_secs(30);
/// 识别引擎就绪（isAvailable）的轮询等待：刚 init 的 recognizer 常瞬时不可用（异步
/// 加载语言资源），稍等即就绪。等满仍不可用才报错——修「有时用不了」的竞态。
const AVAILABILITY_WAIT: Duration = Duration::from_secs(3);
const AVAILABILITY_POLL: Duration = Duration::from_millis(100);

pub struct AppleSpeechAsr {
    /// 16-bit LE PCM 字节缓冲（recorder 推什么我们存什么）。与 LocalQwenAsr 同形。
    buffer: Mutex<Vec<u8>>,
    /// 识别 locale（Apple 标识符，如 "zh-CN"）。None = 用系统默认。由用户工作语言映射
    /// 而来 —— SFSpeechRecognizer 一个实例只认一种语言，不显式指定就落到系统首选语言
    /// （常是英文），中文语音会被英文引擎识别成英文且理解错误（用户报告的根因）。
    locale: Option<String>,
}

impl AppleSpeechAsr {
    pub fn new(locale: Option<String>) -> Self {
        Self {
            buffer: Mutex::new(Vec::new()),
            locale,
        }
    }

    /// 当前缓冲音频时长（毫秒）。与 LocalQwenAsr::buffer_duration_ms 对齐，
    /// coordinator 用它给本地 provider 计算动态超时。不消费缓冲。
    pub fn buffer_duration_ms(&self) -> u64 {
        (self.buffer.lock().len() as u64 / 2) * 1000 / 16_000
    }

    /// stop 时调用：把缓冲编码成临时 wav，喂给 `SFSpeechURLRecognitionRequest`，
    /// 把异步结果同步化后返回。
    ///
    /// 失败时**保留** buffer（与 WhisperBatchASR / LocalQwenAsr 一致）：凭据无关，
    /// 但权限被拒 / 识别失败时不该把用户录音直接丢掉。仅成功路径清缓冲。
    pub async fn transcribe(&self) -> Result<RawTranscript> {
        // clone 而非 take：会话末调用一次，几 MB 可接受；失败时缓冲仍在。
        let pcm = self.buffer.lock().clone();
        if pcm.is_empty() {
            return Ok(RawTranscript {
                text: String::new(),
                duration_ms: 0,
            });
        }
        let duration_ms = (pcm.len() as u64 / 2) * 1000 / 16_000;
        let locale = self.locale.clone();

        // SFSpeechRecognizer 是阻塞且基于 objc runloop 的同步桥接；放到
        // spawn_blocking 不占 tokio runtime。与 LocalQwenAsr 走同一个 Tauri
        // 持有的 runtime handle。
        let result = tauri::async_runtime::spawn_blocking(move || {
            transcribe_pcm_blocking(&pcm, duration_ms, locale.as_deref())
        })
        .await
        .context("apple-speech transcribe spawn_blocking join 失败")?;

        if result.is_ok() {
            self.buffer.lock().clear();
        }
        result
    }

    pub fn cancel(&self) {
        self.buffer.lock().clear();
    }
}

impl Default for AppleSpeechAsr {
    fn default() -> Self {
        Self::new(None)
    }
}

impl crate::recorder::AudioConsumer for AppleSpeechAsr {
    fn consume_pcm_chunk(&self, pcm: &[u8]) {
        self.buffer.lock().extend_from_slice(pcm);
    }
}

/// 把 PCM 写成临时 wav，确保授权，跑批处理识别，删临时文件，返回结果。
/// 在 spawn_blocking 线程内同步执行。
fn transcribe_pcm_blocking(
    pcm: &[u8],
    duration_ms: u64,
    locale: Option<&str>,
) -> Result<RawTranscript> {
    ensure_authorized()?;

    let samples: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    let wav = encode_wav_16k_mono(&samples);

    // 临时 wav：唯一文件名避免并发会话碰撞；用完即删（RAII guard）。
    let path = std::env::temp_dir().join(format!(
        "openless-apple-speech-{}-{}.wav",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::write(&path, &wav).with_context(|| format!("写临时 wav 失败: {}", path.display()))?;
    let _cleanup = TempFileGuard(&path);

    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("临时 wav 路径含非 UTF-8 字符: {}", path.display()))?;
    let text = recognize_file(path_str, locale)?;

    Ok(RawTranscript { text, duration_ms })
}

/// 当前授权未确定时弹系统授权框并等待；最终非 authorized 一律返回清晰错误。
fn ensure_authorized() -> Result<()> {
    let cls = speech_recognizer_class()?;

    // SFSpeechRecognizer.authorizationStatus（类方法）。
    // SAFETY: `cls` 是已查到的 `SFSpeechRecognizer` 类对象；`authorizationStatus`
    // 是无参类方法，返回 NSInteger（i64）。
    let status: i64 = unsafe { msg_send![cls, authorizationStatus] };
    if status == SF_AUTH_AUTHORIZED {
        return Ok(());
    }
    if status == SF_AUTH_DENIED {
        bail!("语音识别权限被拒绝，请在 系统设置 → 隐私与安全性 → 语音识别 中允许 OpenLess");
    }
    if status == SF_AUTH_RESTRICTED {
        bail!("此设备的语音识别功能受限（可能由家长控制或 MDM 策略禁用）");
    }
    if status != SF_AUTH_NOT_DETERMINED {
        bail!("语音识别授权状态未知: {status}");
    }

    // NotDetermined：弹系统授权框并同步等待回调。block 范式照抄 permissions.rs。
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |granted_status: i64| {
        let _ = tx.send(granted_status);
    });
    log::info!("[apple-speech] requesting SFSpeechRecognizer authorization");
    // SAFETY: `requestAuthorization:` 接收一个 `void(^)(SFSpeechRecognizerAuthorizationStatus)`
    // block，回调参数是 NSInteger（i64）。`&*block` 是 block2 的稳定指针，block 本体
    // 由 `block` 持有到本作用域结束 —— 回调在系统弹框被用户应答后触发，发生在
    // `rx.recv_timeout` 返回之前，因此 block 生命周期足够覆盖回调。
    let _: () = unsafe { msg_send![cls, requestAuthorization: &*block] };

    let granted = match rx.recv_timeout(AUTHORIZATION_WAIT) {
        Ok(s) => s,
        Err(err) => bail!("等待语音识别授权超时或失败: {err}"),
    };
    match granted {
        SF_AUTH_AUTHORIZED => Ok(()),
        SF_AUTH_DENIED => {
            bail!("语音识别权限被拒绝，请在 系统设置 → 隐私与安全性 → 语音识别 中允许 OpenLess")
        }
        SF_AUTH_RESTRICTED => bail!("此设备的语音识别功能受限"),
        other => bail!("语音识别未获授权（状态 {other}）"),
    }
}

/// 用 `SFSpeechURLRecognitionRequest` 对给定 wav 文件做一次批处理识别，
/// 把 `recognitionTaskWithRequest:resultHandler:` 的异步回调同步化。
fn recognize_file(wav_path: &str, locale: Option<&str>) -> Result<String> {
    let recognizer = create_recognizer(locale)?;

    // 识别引擎就绪等待（isAvailable 竞态）：SFSpeechRecognizer 刚 init 时引擎往往还没
    // 就绪（异步加载语言资源），isAvailable 瞬时为 false、稍等即 true。之前一见 false
    // 就 bail —— 这正是「有时用不了」的主因。改为轮询等待最多几秒再判定。
    wait_until_available(recognizer)?;

    let url = file_url(wav_path)?;
    let request = create_url_request(url)?;

    // on-device 优先：设备支持当前语言的设备端识别就强制 on-device —— 音频不出本机
    // （隐私）、离线可用、不受网络波动/限流影响（消除「有时连不上服务器」）。不支持的
    // 语言回退系统默认（可能走网络）以保底能用。
    configure_on_device(recognizer, request);

    let (tx, rx) = mpsc::channel::<RecognitionOutcome>();
    // resultHandler: void(^)(SFSpeechRecognitionResult *result, NSError *error)
    let block = RcBlock::new(move |result: *mut AnyObject, error: *mut AnyObject| {
        let outcome = build_outcome(result, error);
        // 只取第一个 final（或第一个 error）。后续重复回调忽略。
        if outcome.is_terminal() {
            let _ = tx.send(outcome);
        }
    });

    log::info!("[apple-speech] starting recognitionTaskWithRequest");
    // SAFETY: `recognizer` 有效；`request` 是有效的 `SFSpeechURLRecognitionRequest`；
    // `&*block` 是稳定 block 指针，block 本体被 `block` 持有至本作用域结束。
    // 返回的 `SFSpeechRecognitionTask` 我们不持有（自身被 recognizer 强引用直到完成）。
    let _task: *mut AnyObject = unsafe {
        msg_send![
            recognizer,
            recognitionTaskWithRequest: request,
            resultHandler: &*block
        ]
    };

    match rx.recv_timeout(RECOGNITION_WAIT) {
        Ok(RecognitionOutcome::Final(text)) => Ok(text),
        Ok(RecognitionOutcome::Failed(message)) => bail!("语音识别失败: {message}"),
        Ok(RecognitionOutcome::Pending) => unreachable!("Pending 不会被发送"),
        Err(err) => bail!("等待语音识别结果超时或失败: {err}"),
    }
}

/// 轮询等待识别引擎就绪。init 后 isAvailable 可能瞬时 false（异步加载资源），稍等
/// 即 true；等满 AVAILABILITY_WAIT 仍不可用才报错并引导。
fn wait_until_available(recognizer: *mut AnyObject) -> Result<()> {
    let deadline = std::time::Instant::now() + AVAILABILITY_WAIT;
    loop {
        // SAFETY: `recognizer` 有效；`isAvailable` 无参返回 BOOL。
        let available: Bool = unsafe { msg_send![recognizer, isAvailable] };
        if available.as_bool() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "当前语言的语音识别暂不可用：系统可能仍在准备识别资源，或需在 系统设置 → 键盘 → 听写 中下载对应语言。可稍后重试，或改用其它 ASR。"
            );
        }
        std::thread::sleep(AVAILABILITY_POLL);
    }
}

/// 支持设备端识别的语言就把请求设成强制 on-device（音频不出本机、离线可用）；不支持
/// 的语言不设，回退系统默认（可能走网络）以保底能用。
fn configure_on_device(recognizer: *mut AnyObject, request: *mut AnyObject) {
    // SFSpeechRecognizer.supportsOnDeviceRecognition（macOS 10.15+，BOOL 属性）。
    // SAFETY: `recognizer` 有效；无参返回 BOOL。
    let supports: Bool = unsafe { msg_send![recognizer, supportsOnDeviceRecognition] };
    if supports.as_bool() {
        // SFSpeechRecognitionRequest.requiresOnDeviceRecognition = YES。
        // SAFETY: `request` 是 SFSpeechURLRecognitionRequest（父类
        // SFSpeechRecognitionRequest 提供该 setter）；参数 BOOL。
        let _: () = unsafe { msg_send![request, setRequiresOnDeviceRecognition: Bool::new(true)] };
        log::info!("[apple-speech] on-device recognition enabled");
    } else {
        log::info!(
            "[apple-speech] on-device unsupported for current locale; using default (may use network)"
        );
    }
}

/// 识别回调的归一化结果。
enum RecognitionOutcome {
    /// 还没拿到 final（partial），不发送。
    Pending,
    Final(String),
    Failed(String),
}

impl RecognitionOutcome {
    fn is_terminal(&self) -> bool {
        !matches!(self, RecognitionOutcome::Pending)
    }
}

/// 从 `(result, error)` 回调参数提取最终文本或错误。
fn build_outcome(result: *mut AnyObject, error: *mut AnyObject) -> RecognitionOutcome {
    if !error.is_null() {
        return RecognitionOutcome::Failed(ns_error_description(error));
    }
    if result.is_null() {
        return RecognitionOutcome::Failed("识别返回空结果".to_string());
    }
    // result.isFinal —— 只有 final 才取文本；partial 让上层继续等。
    // SAFETY: `result` 非空，是 `SFSpeechRecognitionResult`；`isFinal` 无参返回 BOOL。
    let is_final: Bool = unsafe { msg_send![result, isFinal] };
    if !is_final.as_bool() {
        return RecognitionOutcome::Pending;
    }
    // result.bestTranscription.formattedString → NSString → Rust String。
    // SAFETY: `result` 是 final 的 `SFSpeechRecognitionResult`，`bestTranscription`
    // 非空（final 结果保证有 transcription）；`formattedString` 返回 NSString。
    let transcription: *mut AnyObject = unsafe { msg_send![result, bestTranscription] };
    if transcription.is_null() {
        return RecognitionOutcome::Final(String::new());
    }
    let formatted: *mut AnyObject = unsafe { msg_send![transcription, formattedString] };
    RecognitionOutcome::Final(ns_string_to_rust(formatted))
}

fn speech_recognizer_class() -> Result<&'static AnyClass> {
    AnyClass::get("SFSpeechRecognizer").ok_or_else(|| {
        anyhow!("SFSpeechRecognizer 类不可用（需要 macOS 10.15+ 并链接 Speech.framework）")
    })
}

/// 创建 recognizer。有指定 locale 就 `initWithLocale:`（关键 —— 否则落到系统首选语言，
/// 中文语音会被英文引擎误识别）；无 locale 或 NSLocale 构造失败时回退 `init`（系统默认）。
fn create_recognizer(locale: Option<&str>) -> Result<*mut AnyObject> {
    let cls = speech_recognizer_class()?;
    let recognizer: *mut AnyObject = match locale.and_then(ns_locale) {
        Some(ns_loc) => {
            log::info!("[apple-speech] recognizer locale = {}", locale.unwrap_or(""));
            // SAFETY: `cls` 是 SFSpeechRecognizer 类；`alloc` 得未初始化实例，
            // `initWithLocale:` 用有效 NSLocale 初始化，返回实例移交调用方（ARC 管理）。
            unsafe {
                let alloc: *mut AnyObject = msg_send![cls, alloc];
                msg_send![alloc, initWithLocale: ns_loc]
            }
        }
        None => {
            // SAFETY: 同上；`init` 用系统默认 locale。
            unsafe {
                let alloc: *mut AnyObject = msg_send![cls, alloc];
                msg_send![alloc, init]
            }
        }
    };
    if recognizer.is_null() {
        bail!("无法创建 SFSpeechRecognizer（当前语言可能不支持语音识别）");
    }
    Ok(recognizer)
}

/// `[NSLocale localeWithLocaleIdentifier:<id>]`。构造失败返回 None（调用方回退系统默认）。
fn ns_locale(identifier: &str) -> Option<*mut AnyObject> {
    let ns_id = ns_string_from_str(identifier).ok()?;
    let cls = AnyClass::get("NSLocale")?;
    // SAFETY: `cls` 是 NSLocale；`localeWithLocaleIdentifier:` 接收 NSString（`ns_id` 有效），
    // 返回 autoreleased NSLocale（在 spawn_blocking 线程的 autorelease 池存活）。
    let loc: *mut AnyObject = unsafe { msg_send![cls, localeWithLocaleIdentifier: ns_id] };
    if loc.is_null() {
        None
    } else {
        Some(loc)
    }
}

/// 用户工作语言（原生名，见前端 `SUPPORTED_LANGUAGES`）→ SFSpeechRecognizer 的 locale
/// 标识符。取 `working_languages` 主语言映射；未收录的语言返回 None（回退系统默认 locale）。
/// SFSpeechRecognizer 一个实例只认一种语言，中英混说时以主语言为准 —— 这是 Apple 的固有
/// 限制，云端 ASR 才能自由多语言混识。
pub fn native_name_to_apple_locale(native_name: &str) -> Option<String> {
    let locale = match native_name.trim() {
        "简体中文" => "zh-CN",
        "繁体中文" | "繁體中文" => "zh-TW",
        "English" => "en-US",
        "日本語" => "ja-JP",
        "한국어" => "ko-KR",
        "Français" => "fr-FR",
        "Deutsch" => "de-DE",
        "Español" => "es-ES",
        "Italiano" => "it-IT",
        "Português" => "pt-BR",
        "Русский" => "ru-RU",
        "العربية" => "ar-SA",
        "Tiếng Việt" => "vi-VN",
        "ไทย" => "th-TH",
        "हिन्दी" => "hi-IN",
        _ => return None,
    };
    Some(locale.to_string())
}

/// `[NSURL fileURLWithPath:<path>]`。
fn file_url(path: &str) -> Result<*mut AnyObject> {
    let ns_path = ns_string_from_str(path)?;
    let cls = AnyClass::get("NSURL").ok_or_else(|| anyhow!("NSURL 类不可用"))?;
    // SAFETY: `cls` 是 NSURL；`fileURLWithPath:` 接收 NSString（`ns_path` 有效），
    // 返回 autoreleased NSURL（在 spawn_blocking 线程的隐式 autorelease 池存活）。
    let url: *mut AnyObject = unsafe { msg_send![cls, fileURLWithPath: ns_path] };
    if url.is_null() {
        bail!("构造文件 URL 失败: {path}");
    }
    Ok(url)
}

/// `[[SFSpeechURLRecognitionRequest alloc] initWithURL:<url>]`。
fn create_url_request(url: *mut AnyObject) -> Result<*mut AnyObject> {
    let cls = AnyClass::get("SFSpeechURLRecognitionRequest")
        .ok_or_else(|| anyhow!("SFSpeechURLRecognitionRequest 类不可用"))?;
    // SAFETY: `cls` 是请求类；`alloc`+`initWithURL:` 用有效 `url` 初始化请求实例。
    let request: *mut AnyObject = unsafe {
        let alloc: *mut AnyObject = msg_send![cls, alloc];
        msg_send![alloc, initWithURL: url]
    };
    if request.is_null() {
        bail!("构造 SFSpeechURLRecognitionRequest 失败");
    }
    Ok(request)
}

/// `[NSString stringWithUTF8String:<bytes>]`。`s` 不能含内部 NUL。
fn ns_string_from_str(s: &str) -> Result<*mut AnyObject> {
    let c = std::ffi::CString::new(s).context("字符串含 NUL，无法构造 NSString")?;
    let cls = AnyClass::get("NSString").ok_or_else(|| anyhow!("NSString 类不可用"))?;
    // SAFETY: `cls` 是 NSString；`stringWithUTF8String:` 接收以 NUL 结尾的 C 字符串
    // （`c.as_ptr()` 在 `c` 存活期间有效，本调用同步完成，NSString 会拷贝内容）。
    let ns: *mut AnyObject = unsafe { msg_send![cls, stringWithUTF8String: c.as_ptr()] };
    if ns.is_null() {
        bail!("stringWithUTF8String 返回 nil");
    }
    Ok(ns)
}

/// NSString → Rust String（经 `UTF8String`）。nil 返回空串。
fn ns_string_to_rust(ns: *mut AnyObject) -> String {
    if ns.is_null() {
        return String::new();
    }
    // SAFETY: `ns` 非空，是 NSString；`UTF8String` 返回指向 NSString 内部、以 NUL
    // 结尾的 UTF-8 缓冲，在自动释放池存活期间有效。立即拷贝成 owned String。
    let ptr: *const std::os::raw::c_char = unsafe { msg_send![ns, UTF8String] };
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: `ptr` 是有效、以 NUL 结尾的 C 字符串（来自 NSString.UTF8String）。
    unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// NSError → 可读字符串（`localizedDescription`）。
fn ns_error_description(error: *mut AnyObject) -> String {
    if error.is_null() {
        return "未知错误".to_string();
    }
    // SAFETY: `error` 非空，是 NSError；`localizedDescription` 返回 NSString。
    let desc: *mut AnyObject = unsafe { msg_send![error, localizedDescription] };
    let message = ns_string_to_rust(desc);
    if message.is_empty() {
        "未知错误".to_string()
    } else {
        message
    }
}

/// 进程内单调递增后缀，避免同进程内并发临时 wav 文件名碰撞。
fn unique_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// 临时文件 RAII 清理：transcribe 返回（成功或失败）时删除 wav。
struct TempFileGuard<'a>(&'a std::path::Path);

impl Drop for TempFileGuard<'_> {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(self.0) {
            log::warn!(
                "[apple-speech] 删除临时 wav 失败 {}: {err}",
                self.0.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::AudioConsumer;

    #[test]
    fn buffer_duration_tracks_consumed_pcm() {
        let asr = AppleSpeechAsr::new(None);
        assert_eq!(asr.buffer_duration_ms(), 0);
        // 16k * 2 bytes/sample * 1s = 32000 bytes。
        asr.consume_pcm_chunk(&vec![0u8; 32_000]);
        assert_eq!(asr.buffer_duration_ms(), 1_000);
        asr.consume_pcm_chunk(&vec![0u8; 16_000]);
        assert_eq!(asr.buffer_duration_ms(), 1_500);
    }

    #[test]
    fn cancel_clears_buffer() {
        let asr = AppleSpeechAsr::new(None);
        asr.consume_pcm_chunk(&vec![0u8; 32_000]);
        asr.cancel();
        assert_eq!(asr.buffer_duration_ms(), 0);
    }

    #[tokio::test]
    async fn transcribe_empty_buffer_returns_empty() {
        let asr = AppleSpeechAsr::new(None);
        let transcript = asr.transcribe().await.unwrap();
        assert_eq!(transcript.text, "");
        assert_eq!(transcript.duration_ms, 0);
    }

    #[test]
    fn temp_file_guard_removes_file_on_drop() {
        let path = std::env::temp_dir().join(format!(
            "openless-apple-speech-test-{}.wav",
            unique_suffix()
        ));
        std::fs::write(&path, b"x").unwrap();
        assert!(path.exists());
        {
            let _guard = TempFileGuard(&path);
        }
        assert!(!path.exists());
    }

    #[test]
    fn unique_suffix_is_monotonic() {
        let a = unique_suffix();
        let b = unique_suffix();
        assert!(b > a);
    }
}
