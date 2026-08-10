//! 本地 Qwen3-ASR 在 dictation 路径上的适配器。
//!
//! 与 `WhisperBatchASR` 形状对齐：实现 `AudioConsumer` 缓冲 PCM，stop 时
//! 调本地 Qwen3-ASR 的 batch 解码，不向前端发送中间 token。
//!
//! engine 现在由 `LocalAsrCache` 提供——Coordinator 在 build_local_qwen3 里
//! 取已缓存的引擎再传进来，避免每次会话都重加载 1.2GB+ 模型。

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::sync::Arc;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use super::LocalQwenEngine;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::asr::RawTranscript;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use anyhow::{Context, Result};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use parking_lot::Mutex;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub struct LocalQwenAsr {
    engine: Arc<LocalQwenEngine>,
    buffer: Mutex<Vec<u8>>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl LocalQwenAsr {
    pub fn new(engine: Arc<LocalQwenEngine>) -> Self {
        Self {
            engine,
            buffer: Mutex::new(Vec::new()),
        }
    }

    /// 当前缓冲音频时长（毫秒）。Coordinator 在 transcribe() 调用前读取，
    /// 用来给本地 Qwen ASR 计算动态超时（max(15, ceil(audio_s × 0.6) + 10)）。
    /// 不消费缓冲。
    pub fn buffer_duration_ms(&self) -> u64 {
        (self.buffer.lock().len() as u64 / 2) * 1000 / 16_000
    }

    /// stop 时调用：把 PCM 转 f32，整段执行一次 batch 解码。
    pub async fn transcribe(self: Arc<Self>) -> Result<RawTranscript> {
        let pcm_bytes = std::mem::take(&mut *self.buffer.lock());
        if pcm_bytes.is_empty() {
            return Ok(RawTranscript {
                text: String::new(),
                duration_ms: 0,
            });
        }
        let duration_ms = (pcm_bytes.len() as u64 / 2) * 1000 / 16_000;
        let samples_f32 = i16_le_bytes_to_f32(&pcm_bytes);
        let engine = Arc::clone(&self.engine);
        let text =
            tauri::async_runtime::spawn_blocking(move || engine.transcribe_pcm(&samples_f32))
                .await
                .context("transcribe spawn_blocking join 失败")?
                .context("本地 Qwen3-ASR batch 解码失败")?;

        Ok(RawTranscript { text, duration_ms })
    }

    pub fn cancel(&self) {
        self.buffer.lock().clear();
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl crate::recorder::AudioConsumer for LocalQwenAsr {
    fn consume_pcm_chunk(&self, pcm: &[u8]) {
        self.buffer.lock().extend_from_slice(pcm);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn i16_le_bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|c| {
            let v = i16::from_le_bytes([c[0], c[1]]);
            v as f32 / 32768.0
        })
        .collect()
}
