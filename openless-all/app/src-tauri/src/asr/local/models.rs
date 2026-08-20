//! 本地 Qwen3-ASR 模型注册表（仅 id / 仓库名 / 显示名）。
//!
//! **文件清单与尺寸不再硬编码** —— 由 `download.rs` 在下载时从
//! `huggingface.co/api/models/<repo>/tree/main` 拉真实清单和大小。
//! 增加新模型 = 这里加一条枚举 + 仓库名。

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use crate::persistence;

/// 下载完成后落在模型目录里的哨兵文件名；存在 = 完整、可加载。
pub(super) const READY_SENTINEL: &str = ".openless-asr-ready";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelId {
    Small06b,
    Large17b,
    WhisperBase,
    WhisperSmall,
    WhisperMedium,
    WhisperLargeV3,
    WhisperLargeV3Turbo,
    WhisperLargeV3TurboQ5,
}

impl ModelId {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelId::Small06b => "qwen3-asr-0.6b",
            ModelId::Large17b => "qwen3-asr-1.7b",
            ModelId::WhisperBase => "whisper-base",
            ModelId::WhisperSmall => "whisper-small",
            ModelId::WhisperMedium => "whisper-medium",
            ModelId::WhisperLargeV3 => "whisper-large-v3",
            ModelId::WhisperLargeV3Turbo => "whisper-large-v3-turbo",
            ModelId::WhisperLargeV3TurboQ5 => "whisper-large-v3-turbo-q5",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "qwen3-asr-0.6b" => Some(ModelId::Small06b),
            "qwen3-asr-1.7b" => Some(ModelId::Large17b),
            "whisper-base" => Some(ModelId::WhisperBase),
            "whisper-small" => Some(ModelId::WhisperSmall),
            "whisper-medium" => Some(ModelId::WhisperMedium),
            "whisper-large-v3" => Some(ModelId::WhisperLargeV3),
            "whisper-large-v3-turbo" => Some(ModelId::WhisperLargeV3Turbo),
            "whisper-large-v3-turbo-q5" => Some(ModelId::WhisperLargeV3TurboQ5),
            _ => None,
        }
    }

    pub fn all() -> &'static [ModelId] {
        &[
            ModelId::Small06b,
            ModelId::Large17b,
            ModelId::WhisperBase,
            ModelId::WhisperSmall,
            ModelId::WhisperMedium,
            ModelId::WhisperLargeV3,
            ModelId::WhisperLargeV3Turbo,
            ModelId::WhisperLargeV3TurboQ5,
        ]
    }

    /// HuggingFace repo id（用于拼 API + 下载 URL）。
    pub fn hf_repo(self) -> &'static str {
        match self {
            ModelId::Small06b => "Qwen/Qwen3-ASR-0.6B",
            ModelId::Large17b => "Qwen/Qwen3-ASR-1.7B",
            ModelId::WhisperBase
            | ModelId::WhisperSmall
            | ModelId::WhisperMedium
            | ModelId::WhisperLargeV3
            | ModelId::WhisperLargeV3Turbo
            | ModelId::WhisperLargeV3TurboQ5 => "ggerganov/whisper.cpp",
        }
    }

    pub fn is_whisper(self) -> bool {
        matches!(
            self,
            ModelId::WhisperBase
                | ModelId::WhisperSmall
                | ModelId::WhisperMedium
                | ModelId::WhisperLargeV3
                | ModelId::WhisperLargeV3Turbo
                | ModelId::WhisperLargeV3TurboQ5
        )
    }

    pub fn is_qwen(self) -> bool {
        matches!(self, ModelId::Small06b | ModelId::Large17b)
    }

    pub fn file_name(self) -> Option<&'static str> {
        match self {
            ModelId::WhisperBase => Some("ggml-base.bin"),
            ModelId::WhisperSmall => Some("ggml-small.bin"),
            ModelId::WhisperMedium => Some("ggml-medium.bin"),
            ModelId::WhisperLargeV3 => Some("ggml-large-v3.bin"),
            ModelId::WhisperLargeV3Turbo => Some("ggml-large-v3-turbo.bin"),
            ModelId::WhisperLargeV3TurboQ5 => Some("ggml-large-v3-turbo-q5_0.bin"),
            _ => None,
        }
    }
}

/// Turbo 全精度与 Q5 量化共用同一目录（见 `model_dir`）：返回共享目录的
/// "伙伴"文件名。完整 Turbo 可回退到 Q5，Q5 不反向认完整 Turbo。
fn shared_dir_peer_file(id: ModelId) -> Option<&'static str> {
    match id {
        ModelId::WhisperLargeV3Turbo => Some("ggml-large-v3-turbo-q5_0.bin"),
        _ => None,
    }
}

/// 模型在本地的根目录（可能不存在）。
pub fn model_dir(id: ModelId) -> Result<PathBuf> {
    if id.is_whisper() {
        // Whisper 与 Qwen 共用模型根目录，但各自独立子目录；Turbo 的全精度与
        // Q5 量化文件放同一目录，兼容之前手动迁移的 q5_0 文件。
        let dir_name = if matches!(id, ModelId::WhisperLargeV3TurboQ5) {
            ModelId::WhisperLargeV3Turbo.as_str()
        } else {
            id.as_str()
        };
        Ok(persistence::models_root()?.join(dir_name))
    } else {
        Ok(persistence::local_models_root()?.join(id.as_str()))
    }
}

/// 判断模型是否完整且可加载：Whisper 看目标文件，Qwen 看完成哨兵。
/// 比"枚举所有应有文件"稳：HF 仓库改文件名 / 加新文件时不会误报缺失。
pub fn is_downloaded(id: ModelId) -> bool {
    let dir = match model_dir(id) {
        Ok(d) => d,
        Err(_) => return false,
    };
    is_downloaded_in_dir(id, &dir)
}

fn is_downloaded_in_dir(id: ModelId, dir: &std::path::Path) -> bool {
    if let Some(file_name) = id.file_name() {
        // 完整 Turbo 可使用同目录下的 Q5 回退；Q5 只能认自己的文件。
        return dir.join(file_name).is_file()
            || shared_dir_peer_file(id)
                .map(|peer| dir.join(peer).is_file())
                .unwrap_or(false);
    }
    dir.join(READY_SENTINEL).exists()
}

/// 已落盘的字节数（walk_dir 求和）。下载中也能显示真实进度。
pub fn downloaded_bytes(id: ModelId) -> u64 {
    let dir = match model_dir(id) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    downloaded_bytes_in_dir(id, &dir)
}

fn downloaded_bytes_in_dir(id: ModelId, dir: &std::path::Path) -> u64 {
    if let Some(file_name) = id.file_name() {
        let dest = dir.join(file_name);
        if let Ok(meta) = std::fs::metadata(&dest) {
            return meta.len();
        }
        // 完整 Turbo 目标文件缺失时按 Q5 文件计（同 is_downloaded）。
        // 下载进度不跨认账：.partial 只按各自目标文件算。
        if let Some(peer) = shared_dir_peer_file(id) {
            if let Ok(meta) = std::fs::metadata(dir.join(peer)) {
                return meta.len();
            }
        }
        return super::download::partial_actual_size(&dest.with_extension("partial"));
    }
    let mut total: u64 = 0;
    walk_files(&dir, &mut |size| total += size);
    total
}

fn walk_files<F: FnMut(u64)>(dir: &std::path::Path, on_size: &mut F) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        if name == READY_SENTINEL {
            continue;
        }
        // .partial.idx 是 chunk 完成索引，不算下载字节
        if name.ends_with(".partial.idx") {
            continue;
        }
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk_files(&path, on_size),
            Ok(ft) if ft.is_file() => {
                // .partial 在 chunked 模式下是 sparse 全长，meta.len() 不是真实字节
                if name.ends_with(".partial") {
                    on_size(super::download::partial_actual_size(&path));
                } else if let Ok(meta) = entry.metadata() {
                    on_size(meta.len());
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub id: String,
    pub hf_repo: String,
    pub downloaded_bytes: u64,
    pub is_downloaded: bool,
}

pub fn list_status() -> Vec<ModelStatus> {
    ModelId::all()
        .iter()
        .map(|&id| ModelStatus {
            id: id.as_str().to_string(),
            hf_repo: id.hf_repo().to_string(),
            downloaded_bytes: downloaded_bytes(id),
            is_downloaded: is_downloaded(id),
        })
        .collect()
}

/// 删除本地模型目录（用户在 UI 主动删）。
pub fn delete_model(id: ModelId) -> Result<()> {
    let dir = model_dir(id)?;
    delete_model_files(id, &dir)
}

fn delete_model_files(id: ModelId, dir: &std::path::Path) -> Result<()> {
    if let Some(file_name) = id.file_name() {
        let dest = dir.join(file_name);
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_file(dest.with_extension("partial"));
        let _ = std::fs::remove_file(dest.with_extension("partial.idx"));
        if dir.exists() && dir.read_dir()?.next().is_none() {
            let _ = std::fs::remove_dir(dir);
        }
        return Ok(());
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{delete_model_files, downloaded_bytes_in_dir, is_downloaded_in_dir, ModelId};

    #[test]
    fn deleting_one_shared_whisper_file_keeps_the_other() {
        let dir = std::env::temp_dir().join(format!(
            "openless-asr-models-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let turbo = dir.join("ggml-large-v3-turbo.bin");
        let q5 = dir.join("ggml-large-v3-turbo-q5_0.bin");
        std::fs::write(&turbo, b"turbo").unwrap();
        std::fs::write(&q5, b"q5").unwrap();

        delete_model_files(ModelId::WhisperLargeV3TurboQ5, &dir).unwrap();

        assert!(turbo.is_file());
        assert!(!q5.exists());

        std::fs::write(&q5, b"q5").unwrap();
        delete_model_files(ModelId::WhisperLargeV3Turbo, &dir).unwrap();

        assert!(!turbo.exists());
        assert!(q5.is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn q5_is_not_marked_downloaded_by_full_precision_turbo() {
        let dir = std::env::temp_dir().join(format!(
            "openless-asr-models-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ggml-large-v3-turbo.bin"), b"turbo").unwrap();

        assert!(is_downloaded_in_dir(ModelId::WhisperLargeV3Turbo, &dir));
        assert!(!is_downloaded_in_dir(ModelId::WhisperLargeV3TurboQ5, &dir));
        assert_eq!(
            downloaded_bytes_in_dir(ModelId::WhisperLargeV3Turbo, &dir),
            5
        );
        assert_eq!(
            downloaded_bytes_in_dir(ModelId::WhisperLargeV3TurboQ5, &dir),
            0
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
