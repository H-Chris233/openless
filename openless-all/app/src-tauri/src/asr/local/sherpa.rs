#![allow(dead_code, unused_imports, unused_variables)]
//! Windows sherpa-onnx 本地 ASR 的常量、catalog 与事件载荷。
//!
//! 当前 catalog 覆盖 Windows offline batch 模型和实验 online streaming 模型；
//! `sherpa_runtime.rs` 分别持有 `OfflineRecognizer` / `OnlineRecognizer`。

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

pub const PROVIDER_ID: &str = "sherpa-onnx-local";
pub const DEFAULT_MODEL_ALIAS: &str = "sense-voice-small-zh";
pub const DEFAULT_ONLINE_MODEL_ALIAS: &str = "zipformer-bilingual-zh-en-streaming";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub enum SherpaFamily {
    SenseVoice,
    Paraformer,
    Whisper,
    Qwen3Asr,
    Zipformer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub enum SherpaMode {
    /// 录音停止后整段 PCM 一次性识别。
    Offline,
    /// 边录边识别 partial / final segment。
    Online,
}

pub fn model_alias_is_known(alias: &str) -> bool {
    openless_core::LocalAsrTarget::parse(openless_core::LocalAsrRuntime::SherpaOnnx, alias).is_ok()
}

pub fn mode_for_alias(alias: &str) -> Result<SherpaMode> {
    if alias == DEFAULT_ONLINE_MODEL_ALIAS {
        Ok(SherpaMode::Online)
    } else if model_alias_is_known(alias) {
        Ok(SherpaMode::Offline)
    } else {
        anyhow::bail!("unknown sherpa-onnx model alias: {alias}")
    }
}

pub fn alias_is_online(alias: &str) -> bool {
    matches!(mode_for_alias(alias), Ok(SherpaMode::Online))
}

pub fn required_files_for_alias(alias: &str) -> Result<&'static [&'static str]> {
    match alias {
        "sense-voice-small-zh" => Ok(&["model.int8.onnx", "tokens.txt"]),
        "paraformer-zh" => Ok(&["model.int8.onnx", "tokens.txt"]),
        "whisper-small-multi" => Ok(&["encoder.int8.onnx", "decoder.int8.onnx", "tokens.txt"]),
        "whisper-large-v3-multi" => Ok(&["encoder.int8.onnx", "decoder.int8.onnx", "tokens.txt"]),
        "qwen3-asr-0.6b-int8" => Ok(&[
            "conv_frontend.onnx",
            "encoder.int8.onnx",
            "decoder.int8.onnx",
            "tokenizer",
        ]),
        DEFAULT_ONLINE_MODEL_ALIAS => Ok(&[
            "encoder-epoch-99-avg-1.int8.onnx",
            "decoder-epoch-99-avg-1.onnx",
            "joiner-epoch-99-avg-1.int8.onnx",
            "tokens.txt",
        ]),
        _ => anyhow::bail!("unknown sherpa-onnx model alias: {alias}"),
    }
}

pub fn required_path_is_valid(alias: &str, required: &str, path: &Path) -> bool {
    if required_path_is_dir(alias, required) {
        required_dir_is_valid(alias, required, path)
    } else {
        path.is_file()
    }
}

fn required_path_is_dir(alias: &str, required: &str) -> bool {
    matches!((alias, required), ("qwen3-asr-0.6b-int8", "tokenizer"))
}

fn required_dir_is_valid(alias: &str, required: &str, path: &Path) -> bool {
    match (alias, required) {
        ("qwen3-asr-0.6b-int8", "tokenizer") => {
            path.join("tokenizer.json").is_file()
                || (path.join("vocab.json").is_file() && path.join("merges.txt").is_file())
        }
        _ => false,
    }
}

pub fn model_dir_for_alias(alias: &str) -> Result<PathBuf> {
    if !model_alias_is_known(alias) {
        anyhow::bail!("unknown sherpa-onnx model alias: {alias}");
    }
    #[cfg(target_os = "windows")]
    {
        Ok(crate::persistence::sherpa_onnx_models_root()?.join(alias))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(std::env::temp_dir()
            .join("openless-sherpa-onnx")
            .join(alias))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub enum SherpaPreparePhase {
    Runtime,
    Model,
    Load,
    Finished,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct SherpaPrepareProgressPayload {
    pub phase: SherpaPreparePhase,
    pub model_alias: String,
    pub label: String,
    pub percent: Option<f64>,
    pub error: Option<String>,
}

impl SherpaPrepareProgressPayload {
    #[allow(dead_code)]
    pub fn new(
        phase: SherpaPreparePhase,
        model_alias: impl Into<String>,
        label: impl Into<String>,
        percent: Option<f64>,
        error: Option<String>,
    ) -> Self {
        Self {
            phase,
            model_alias: model_alias.into(),
            label: label.into(),
            percent: percent.map(|value| value.clamp(0.0, 100.0)),
            error,
        }
    }

    #[allow(dead_code)]
    pub fn failed(
        model_alias: impl Into<String>,
        label: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self::new(
            SherpaPreparePhase::Failed,
            model_alias,
            label,
            None,
            Some(error.into()),
        )
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct SherpaRuntimeStatus {
    pub provider_id: String,
    /// 当前平台是否具备 sherpa-onnx 推理能力。Windows 为 true；其他平台保留
    /// provider 元数据但不提供本地 sherpa 推理。
    pub available: bool,
    /// 当前模型是否已加载到内存。
    pub runtime_ready: bool,
    pub active_model: String,
    pub loaded_model_id: Option<String>,
    pub error: Option<String>,
    /// 最近一次 prepare/load 耗时。缓存命中也会记录一次很小的耗时。
    pub last_prepare_ms: Option<u64>,
    /// 最近一次 batch decode 耗时，不含录音时间。
    pub last_transcribe_ms: Option<u64>,
    /// 最近一次送入 recognizer 的音频时长。
    pub last_audio_ms: Option<u64>,
    /// 最近一次 prepare/transcribe 错误，方便 UI 和日志定位可恢复失败。
    pub last_error: Option<String>,
}

impl SherpaRuntimeStatus {
    #[allow(dead_code)]
    pub fn unavailable(active_model: String, error: impl Into<String>) -> Self {
        let error = error.into();
        Self {
            provider_id: PROVIDER_ID.into(),
            available: false,
            runtime_ready: false,
            active_model,
            loaded_model_id: None,
            error: Some(error.clone()),
            last_prepare_ms: None,
            last_transcribe_ms: None,
            last_audio_ms: None,
            last_error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn online_zipformer_has_download_and_required_files() {
        assert_eq!(
            mode_for_alias(DEFAULT_ONLINE_MODEL_ALIAS).unwrap(),
            SherpaMode::Online
        );
        assert!(alias_is_online(DEFAULT_ONLINE_MODEL_ALIAS));
        assert_eq!(
            required_files_for_alias(DEFAULT_ONLINE_MODEL_ALIAS).unwrap(),
            &[
                "encoder-epoch-99-avg-1.int8.onnx",
                "decoder-epoch-99-avg-1.onnx",
                "joiner-epoch-99-avg-1.int8.onnx",
                "tokens.txt",
            ]
        );
    }

    #[test]
    fn qwen3_tokenizer_accepts_supported_layouts() {
        let dir = std::env::temp_dir().join(format!(
            "openless-sherpa-tokenizer-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("create tokenizer dir");

        assert!(!required_path_is_valid(
            "qwen3-asr-0.6b-int8",
            "tokenizer",
            &dir
        ));

        fs::write(dir.join("tokenizer.json"), b"{}").expect("write tokenizer json");
        assert!(required_path_is_valid(
            "qwen3-asr-0.6b-int8",
            "tokenizer",
            &dir
        ));

        fs::remove_file(dir.join("tokenizer.json")).expect("remove tokenizer json");
        fs::write(dir.join("vocab.json"), b"{}").expect("write vocab json");
        assert!(!required_path_is_valid(
            "qwen3-asr-0.6b-int8",
            "tokenizer",
            &dir
        ));

        fs::write(dir.join("merges.txt"), b"#version: 0.2").expect("write merges txt");
        assert!(required_path_is_valid(
            "qwen3-asr-0.6b-int8",
            "tokenizer",
            &dir
        ));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unavailable_status_uses_provider_id() {
        let status = SherpaRuntimeStatus::unavailable("paraformer-zh".into(), "not ready");
        assert_eq!(status.provider_id, PROVIDER_ID);
        assert!(!status.available);
        assert!(!status.runtime_ready);
        assert_eq!(status.active_model, "paraformer-zh");
        assert_eq!(status.error.as_deref(), Some("not ready"));
        assert_eq!(status.last_error.as_deref(), Some("not ready"));
    }

    #[test]
    fn prepare_progress_payload_uses_expected_event_shape() {
        let payload = SherpaPrepareProgressPayload::new(
            SherpaPreparePhase::Model,
            "sense-voice-small-zh",
            "download model",
            Some(42.4),
            None,
        );
        let value = serde_json::to_value(payload).unwrap();
        assert_eq!(value["phase"], "model");
        assert_eq!(value["modelAlias"], "sense-voice-small-zh");
        assert_eq!(value["label"], "download model");
        assert_eq!(value["percent"], 42.4);
        assert_eq!(value["error"], serde_json::Value::Null);
    }
}
