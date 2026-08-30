//! Coding Agent 的跨宿主纯业务已迁入 `openless-core`。
//!
//! 本模块仅保留兼容重导出，让现有 Tauri 进程 Adapter 无需一次性改完所有导入路径。

pub use openless_core::coding_agent::{
    build_claude_args, resolve_coding_agent_model, CodingAgentPermissionMode, CodingAgentProvider,
    CodingAgentRequest,
};
