//! 本地 ASR 引擎入口。
//!
//! 当前本地引擎：
//! - **macOS**：`antirez/qwen-asr` 纯 C + Accelerate（`local_provider` / `qwen_engine`）
//! - **Windows**：Foundry Local Whisper（`foundry_*`），以及 sherpa-onnx-local
//!   实验 provider（`sherpa*`，offline batch + online streaming）

pub mod cache;
pub mod download;
pub mod foundry;
pub mod foundry_native;
pub mod foundry_provider;
pub mod foundry_runtime;
mod local_provider;
pub mod models;
pub mod sherpa;
pub mod sherpa_download;
pub mod sherpa_provider;
pub mod sherpa_runtime;
pub mod test_run;

pub use cache::LocalAsrCache;
#[allow(unused_imports)]
pub use foundry_provider::FoundryLocalWhisperAsr;
#[allow(unused_imports)]
pub use foundry_runtime::FoundryLocalRuntime;
#[allow(unused_imports)]
pub use sherpa_provider::SherpaOnnxAsr;
#[allow(unused_imports)]
pub use sherpa_runtime::SherpaOnnxRuntime;

#[cfg(target_os = "macos")]
mod qwen_engine;
#[cfg(target_os = "macos")]
mod qwen_ffi;

#[cfg(target_os = "macos")]
pub use local_provider::LocalQwenAsr;
#[cfg(target_os = "macos")]
pub use qwen_engine::QwenAsrEngine;

pub use download::{DownloadManager, Mirror};
pub use models::{ModelId, ModelStatus};

/// 本地 Qwen3-ASR 在 active_asr 字段里的标识；与前端 ASR_PRESETS 的 id 对齐。
pub const PROVIDER_ID: &str = "local-qwen3";

pub fn is_local_qwen3(id: &str) -> bool {
    id == PROVIDER_ID
}
