//! Streaming ASR providers.
//!
//! Mirrors the Swift `OpenLessASR` library. The Volcengine SAUC bigmodel
//! client is the reference implementation; the wire protocol lives in
//! `frame.rs` (binary frame codec) and the session lifecycle in
//! `volcengine.rs`.

pub mod local;

pub use openless_core::asr::{
    bailian, dashscope_multimodal, elevenlabs, mimo, pcm, qwen_realtime, stepfun_realtime,
    volcengine, wav, whisper, xfyun, AudioConsumer, BailianCredentials, BailianRealtimeASR,
    DashScopeMultimodalASR, DictionaryHotword, ElevenLabsBatchASR, MimoBatchASR,
    Qwen3RealtimeASR, Qwen3RealtimeCredentials, RawTranscript, StepfunRealtimeASR,
    StepfunRealtimeCredentials, VolcengineCredentials, VolcengineStreamingASR, WhisperBatchASR,
    XfyunCredentials, XfyunStreamingASR,
};
