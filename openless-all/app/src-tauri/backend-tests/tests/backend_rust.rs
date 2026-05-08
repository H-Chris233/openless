//! Rust-only backend unit harness.
//!
//! 这个测试 crate 只把纯 Rust 后端模块按源码路径编进来，不链接完整 Tauri
//! `openless_lib`，避免 Windows CI 在 test harness 启动前被桌面运行时 DLL 拦截。

#![allow(dead_code, unused_variables)]

mod asr {
    pub mod local {
        pub mod foundry {
            pub const DEFAULT_MODEL_ALIAS: &str = "whisper-large-v3-turbo";
            pub const PROVIDER_ID: &str = "foundry-local-whisper";
        }

        pub mod foundry_native {
            pub fn normalize_runtime_source_str(value: &str) -> String {
                match value.trim() {
                    "nuget" | "ort-nightly" => value.trim().to_string(),
                    _ => "auto".to_string(),
                }
            }
        }
    }
}

#[path = "../../src/coordinator_state.rs"]
mod coordinator_state;
#[path = "../../src/hotkey.rs"]
mod hotkey;
#[cfg(not(target_os = "macos"))]
#[path = "../../src/insertion.rs"]
mod insertion;
#[path = "../../src/recorder.rs"]
mod recorder;
#[path = "../../src/shortcut_binding.rs"]
mod shortcut_binding;
#[path = "../../src/types.rs"]
mod types;
