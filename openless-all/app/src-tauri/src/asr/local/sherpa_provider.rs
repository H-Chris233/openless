//! sherpa-onnx 本地 ASR provider（M1 骨架）。
//!
//! 形状与 `foundry_provider.rs` 对齐：
//! - 作为 `Recorder::AudioConsumer` 持续吃 PCM
//! - 录音结束后 `transcribe(timeout)` 返回 `RawTranscript`
//! - `cancel()` 让任何 in-flight transcription 提前结束（M1 桩，仅清 buffer）
//!
//! M1 阶段：
//! - `transcribe` 调 `SherpaOnnxRuntime::transcribe_pcm`（M1 返回空串）
//! - 让主链路在 Windows + `sherpa-onnx-local` provider 时能跑完
//!   begin_session → 录音 → end_session → polish → insert 的形态
//! - M1 空 transcript 会走现有 emptyTranscript 护栏；M2 接真实推理后复用同一收尾路径

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;

use crate::asr::RawTranscript;

use super::sherpa_runtime::SherpaOnnxRuntime;

pub struct SherpaOnnxAsr {
    runtime: Arc<SherpaOnnxRuntime>,
    model_alias: String,
    language_hint: Option<String>,
    buffer: Mutex<Vec<u8>>,
    cancel_generation: AtomicU64,
}

impl SherpaOnnxAsr {
    pub fn new(
        runtime: Arc<SherpaOnnxRuntime>,
        model_alias: String,
        language_hint: Option<String>,
    ) -> Self {
        Self {
            runtime,
            model_alias,
            language_hint: normalize_language_hint(language_hint),
            buffer: Mutex::new(Vec::new()),
            cancel_generation: AtomicU64::new(0),
        }
    }

    #[allow(dead_code)]
    pub fn model_alias(&self) -> &str {
        &self.model_alias
    }

    #[allow(dead_code)]
    pub fn language_hint(&self) -> Option<&str> {
        self.language_hint.as_deref()
    }

    pub async fn transcribe(&self, audio_timeout: Duration) -> Result<RawTranscript> {
        let cancel_generation = self.cancel_generation.load(Ordering::SeqCst);
        let pcm = self.buffer.lock().clone();
        if pcm.is_empty() {
            return Ok(RawTranscript {
                text: String::new(),
                duration_ms: 0,
            });
        }

        let duration_ms = pcm_duration_ms(&pcm);
        let result = self
            .runtime
            .transcribe_pcm(&self.model_alias, &pcm, self.language_hint(), audio_timeout)
            .await;

        if self.cancel_generation.load(Ordering::SeqCst) != cancel_generation {
            anyhow::bail!("sherpa-onnx transcription cancelled");
        }

        // 与 Foundry 行为对齐：进入推理后清 buffer，避免下一轮重复消费。
        self.buffer.lock().clear();

        let text = result?;
        Ok(RawTranscript {
            text: trim_transcript_text(&text),
            duration_ms,
        })
    }

    pub fn cancel(&self) {
        self.cancel_generation.fetch_add(1, Ordering::SeqCst);
        self.runtime.request_cancel_prepare();
        self.buffer.lock().clear();
    }
}

impl crate::recorder::AudioConsumer for SherpaOnnxAsr {
    fn consume_pcm_chunk(&self, pcm: &[u8]) {
        self.buffer.lock().extend_from_slice(pcm);
    }
}

fn pcm_duration_ms(pcm: &[u8]) -> u64 {
    (pcm.len() as u64 / 2) * 1000 / 16_000
}

fn trim_transcript_text(text: &str) -> String {
    text.trim().to_string()
}

fn normalize_language_hint(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::AudioConsumer;

    fn make_provider() -> SherpaOnnxAsr {
        SherpaOnnxAsr::new(
            Arc::new(SherpaOnnxRuntime::new()),
            "sense-voice-small-zh".into(),
            Some("  ZH  ".into()),
        )
    }

    #[test]
    fn normalize_language_hint_trims_and_lowercases() {
        let provider = make_provider();
        assert_eq!(provider.language_hint(), Some("zh"));
    }

    #[test]
    fn empty_language_hint_normalizes_to_none() {
        let provider = SherpaOnnxAsr::new(
            Arc::new(SherpaOnnxRuntime::new()),
            "paraformer-zh".into(),
            Some("   ".into()),
        );
        assert!(provider.language_hint().is_none());
    }

    #[test]
    fn consume_pcm_chunk_extends_buffer() {
        let provider = make_provider();
        provider.consume_pcm_chunk(&[1, 2, 3, 4]);
        provider.consume_pcm_chunk(&[5, 6]);
        assert_eq!(provider.buffer.lock().len(), 6);
    }

    #[tokio::test]
    async fn empty_buffer_transcribe_returns_empty_transcript() {
        let provider = make_provider();
        let result = provider.transcribe(Duration::from_secs(5)).await.unwrap();
        assert!(result.text.is_empty());
        assert_eq!(result.duration_ms, 0);
    }

    #[tokio::test]
    async fn transcribe_clears_buffer_on_runtime_error() {
        let provider = SherpaOnnxAsr::new(
            Arc::new(SherpaOnnxRuntime::new()),
            "unknown-sherpa-model".into(),
            None,
        );
        provider.consume_pcm_chunk(&vec![0u8; 32_000]);
        let result = provider.transcribe(Duration::from_secs(5)).await;
        assert!(result.is_err());
        assert!(provider.buffer.lock().is_empty());
    }

    #[test]
    fn cancel_clears_buffer_and_bumps_generation() {
        let provider = make_provider();
        provider.consume_pcm_chunk(&[1, 2, 3, 4]);
        let before = provider.cancel_generation.load(Ordering::SeqCst);
        provider.cancel();
        let after = provider.cancel_generation.load(Ordering::SeqCst);
        assert!(after > before);
        assert!(provider.buffer.lock().is_empty());
    }
}
