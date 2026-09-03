//! Coding Agent 的跨宿主请求类型、参数归一化和纯业务规则。
//!
//! 进程创建、Git 快照、临时配置文件和事件转发属于宿主 Adapter；本模块只保留两个宿主
//! 必须共享的规则，避免 Tauri 与 Linux 各维护一份 provider/权限/模型语义。

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::coding_agent_guard::{deny_rule_for_pattern, HIGH_RISK_PATTERNS};
use crate::errors::{BackendError, BackendErrorCode};
use crate::events::{BackendEventKind, BackendEventPublisher, CodingAgentStreamEvent};

/// Coding Agent provider，对应持久化偏好中的稳定字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodingAgentProvider {
    #[serde(rename = "claude-code-cli")]
    ClaudeCodeCli,
    #[serde(rename = "opencode-cli")]
    OpenCodeCli,
    #[serde(rename = "codex-cli")]
    CodexCli,
    #[serde(rename = "dsh-cli")]
    DshCli,
}

impl CodingAgentProvider {
    pub fn from_pref(value: &str) -> Self {
        match value.trim() {
            "opencode-cli" => Self::OpenCodeCli,
            "codex-cli" => Self::CodexCli,
            "dsh-cli" => Self::DshCli,
            _ => Self::ClaudeCodeCli,
        }
    }

    pub fn as_pref(self) -> &'static str {
        match self {
            Self::ClaudeCodeCli => "claude-code-cli",
            Self::OpenCodeCli => "opencode-cli",
            Self::CodexCli => "codex-cli",
            Self::DshCli => "dsh-cli",
        }
    }

    pub fn supports_command_approval(self) -> bool {
        matches!(self, Self::ClaudeCodeCli | Self::OpenCodeCli)
    }

    pub fn default_exe(self) -> &'static str {
        match self {
            Self::ClaudeCodeCli => "claude",
            Self::OpenCodeCli => "opencode",
            Self::CodexCli => "codex",
            Self::DshCli => "dsh",
        }
    }

    pub fn max_budget_usd(self) -> Option<f64> {
        match self {
            Self::ClaudeCodeCli => Some(2.0),
            Self::OpenCodeCli | Self::CodexCli | Self::DshCli => None,
        }
    }
}

/// 按 provider 解析用户配置的模型。
pub fn resolve_coding_agent_model(
    provider: CodingAgentProvider,
    configured: Option<String>,
) -> Option<String> {
    let configured = configured
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty());
    match provider {
        CodingAgentProvider::ClaudeCodeCli => configured.or_else(|| Some("sonnet".to_string())),
        CodingAgentProvider::OpenCodeCli => configured.filter(|model| model.contains('/')),
        CodingAgentProvider::CodexCli => configured,
        CodingAgentProvider::DshCli => None,
    }
}

/// Coding Agent 权限模式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodingAgentPermissionMode {
    Plan,
    Default,
    #[default]
    AcceptEdits,
    BypassPermissions,
}

impl CodingAgentPermissionMode {
    pub fn as_cli_arg(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

/// Resolve the permission mode used by the unattended voice path. Wide legacy
/// values are deliberately reduced to a provider-supported safe mode.
pub fn normalize_less_computer_permission_mode(
    provider: CodingAgentProvider,
    configured: &str,
) -> CodingAgentPermissionMode {
    let mode = match configured.trim() {
        "plan" => CodingAgentPermissionMode::Plan,
        "default" => CodingAgentPermissionMode::Default,
        "bypassPermissions" => CodingAgentPermissionMode::BypassPermissions,
        _ => CodingAgentPermissionMode::AcceptEdits,
    };
    match provider {
        CodingAgentProvider::CodexCli | CodingAgentProvider::DshCli
            if matches!(
                mode,
                CodingAgentPermissionMode::Default | CodingAgentPermissionMode::BypassPermissions
            ) =>
        {
            CodingAgentPermissionMode::Plan
        }
        CodingAgentProvider::ClaudeCodeCli | CodingAgentProvider::OpenCodeCli
            if mode == CodingAgentPermissionMode::BypassPermissions =>
        {
            CodingAgentPermissionMode::AcceptEdits
        }
        _ => mode,
    }
}

/// Validate and resolve the configured Coding Agent working directory. The
/// fallback is supplied by the host through [`BackendConfig`](crate::BackendConfig).
pub fn normalize_coding_agent_workdir(
    configured: Option<String>,
    fallback: Option<PathBuf>,
) -> Result<Option<PathBuf>, BackendError> {
    let configured = configured
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let path = configured.map(PathBuf::from).or(fallback);
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.is_absolute() {
        return Err(invalid_argument("coding agent workdir must be absolute"));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(invalid_argument("coding agent workdir cannot contain '..'"));
    }
    Ok(Some(path))
}

/// 一次无头 Coding Agent 运行的归一化请求。
#[derive(Debug, Clone)]
pub struct CodingAgentRequest {
    pub session_id: String,
    pub provider: CodingAgentProvider,
    /// prompt 只能走 stdin/专用输入，不得放入 argv。
    pub prompt: String,
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    pub fallback_model: Option<String>,
    pub permission_mode: CodingAgentPermissionMode,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub max_budget_usd: Option<f64>,
    pub timeout_secs: u64,
    pub extra_system_prompt: Option<String>,
    pub settings_json_path: Option<PathBuf>,
    pub session_persistence: bool,
    pub continue_session: bool,
    pub continuation_context: Option<String>,
    /// Optional executable selected by the Core policy. Hosts may resolve the
    /// empty value to their platform default without changing the request.
    pub executable: Option<String>,
    /// Core-approved high-risk patterns. Runtime adapters use these values to
    /// construct provider-specific guard configuration.
    pub approved_patterns: Vec<String>,
}

impl CodingAgentRequest {
    pub fn new(session_id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            provider: CodingAgentProvider::ClaudeCodeCli,
            prompt: prompt.into(),
            cwd: None,
            model: None,
            fallback_model: None,
            permission_mode: CodingAgentPermissionMode::default(),
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            max_budget_usd: None,
            timeout_secs: 300,
            extra_system_prompt: None,
            settings_json_path: None,
            session_persistence: true,
            continue_session: false,
            continuation_context: None,
            executable: None,
            approved_patterns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CancellationToken(Arc<AtomicBool>);

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTemporaryFile {
    pub name: String,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptPayload {
    Stdin(String),
    Argv(String),
    PatchFile { main_file: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommand {
    pub executable: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub prompt: PromptPayload,
    pub temporary_files: Vec<AgentTemporaryFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutputLine {
    pub stream: ProcessStream,
    pub line: String,
}

pub trait ProcessOutputSink: Send + Sync {
    fn write(&self, line: ProcessOutputLine);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub success: bool,
}

pub trait CodingAgentProcessAdapter: Send + Sync {
    fn execute(
        &self,
        command: AgentCommand,
        output: Arc<dyn ProcessOutputSink>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<ProcessExit, BackendError>>;
}

pub fn temporary_path_token(name: &str) -> String {
    format!("__OPENLESS_TEMP_PATH_{name}__")
}

pub fn temporary_json_path_token(name: &str) -> String {
    format!("__OPENLESS_TEMP_JSON_PATH_{name}__")
}

/// Wrap a user task with the invariant one-shot instructions shared by every
/// Less Computer provider. The text is pure policy; process execution remains
/// in the host Adapter.
pub fn autonomous_prompt(task: &str) -> String {
    format!(
        "【自动化任务 · 一次性完成】这是一次无人值守的单次无头运行，没有多轮对话机会，\
你无法事后追问或补充。请把下面的需求当成一个必须在本次运行内彻底达成的目标（等价于先 /goal \
设定目标与完成标准，再自主执行直到达成）：\n\
- 先想清楚目标和「完成」的判定标准，再开始动手；\n\
- 自主、连续地一口气执行到完全完成，不要中途停下来提问或等待确认；遇到歧义按最合理的方式继续；\n\
- 不要只给计划、思路或半成品，也不要留「后续步骤」给别人——要交付最终可用的结果；\n\
- 任务较长也要想办法在这一次运行内拆解并跑完；\n\
- 全部完成后，只输出最终结果本身，不要解释过程、不要前后缀、不要引号。\n\n\
需求：\n{task}"
    )
}

/// 构造 Claude Code 无头流式参数；不含可执行文件和 prompt。
pub fn build_claude_args(request: &CodingAgentRequest) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--include-partial-messages".into(),
        "--permission-mode".into(),
        request.permission_mode.as_cli_arg().into(),
    ];
    if let Some(model) = &request.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(model) = &request.fallback_model {
        args.extend(["--fallback-model".into(), model.clone()]);
    }
    if let Some(cwd) = &request.cwd {
        args.extend(["--add-dir".into(), cwd.to_string_lossy().into_owned()]);
    }
    if !request.allowed_tools.is_empty() {
        args.extend(["--allowedTools".into(), request.allowed_tools.join(",")]);
    }
    if !request.disallowed_tools.is_empty() {
        args.extend([
            "--disallowedTools".into(),
            request.disallowed_tools.join(","),
        ]);
    }
    if let Some(budget) = request.max_budget_usd {
        args.extend(["--max-budget-usd".into(), budget.to_string()]);
    }
    if let Some(path) = &request.settings_json_path {
        args.extend(["--settings".into(), path.to_string_lossy().into_owned()]);
    }
    if let Some(prompt) = &request.extra_system_prompt {
        args.extend(["--append-system-prompt".into(), prompt.clone()]);
    }
    if !request.session_persistence {
        args.push("--no-session-persistence".into());
    }
    if request.continue_session {
        args.push("--continue".into());
    }
    args
}

/// Codex 的沙箱模式。权限只能收紧，遗留的宽权限值统一降级为只读。
pub fn codex_sandbox_mode(mode: CodingAgentPermissionMode) -> &'static str {
    match mode {
        CodingAgentPermissionMode::AcceptEdits => "workspace-write",
        CodingAgentPermissionMode::Plan
        | CodingAgentPermissionMode::Default
        | CodingAgentPermissionMode::BypassPermissions => "read-only",
    }
}

/// 构造 OpenCode 无头参数；prompt 必须由宿主追加在 `--` 之后。
pub fn build_opencode_args(request: &CodingAgentRequest) -> Vec<String> {
    let mut args = vec!["run".into(), "--format".into(), "json".into()];
    if let Some(model) = &request.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(cwd) = &request.cwd {
        args.extend(["--dir".into(), cwd.to_string_lossy().into_owned()]);
    }
    if request.permission_mode != CodingAgentPermissionMode::Plan {
        args.push("--auto".into());
    }
    if request.continue_session {
        args.push("--continue".into());
    }
    args.push("--".into());
    args
}

/// 构造 Codex 参数；prompt 由 stdin 提供，避免 argv 泄漏和注入。
pub fn build_codex_args(request: &CodingAgentRequest) -> Vec<String> {
    let mut args = vec![
        "exec".into(),
        "--json".into(),
        "--color".into(),
        "never".into(),
        "--skip-git-repo-check".into(),
        "--sandbox".into(),
        codex_sandbox_mode(request.permission_mode).into(),
        "-c".into(),
        "sandbox_workspace_write.exclude_tmpdir_env_var=true".into(),
        "-c".into(),
        "sandbox_workspace_write.exclude_slash_tmp=true".into(),
    ];
    if let Some(model) = &request.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(cwd) = &request.cwd {
        args.extend(["--cd".into(), cwd.to_string_lossy().into_owned()]);
    }
    if request.continue_session {
        args.extend(["resume".into(), "--last".into()]);
    }
    args.push("-".into());
    args
}

/// dsh 只允许通过 profile 启动；prompt 由宿主写入 stdin/patch。
pub fn build_dsh_args(request: &CodingAgentRequest) -> Vec<String> {
    let _ = request;
    vec!["--profile".into(), "headless".into()]
}

pub const DSH_TASK_PLACEHOLDER: &str = "openless-task";

pub fn build_dsh_args_with_patch(patch_path: &Path) -> Vec<String> {
    vec![
        "--profile".into(),
        "headless".into(),
        "--patch".into(),
        patch_path.to_string_lossy().into_owned(),
        DSH_TASK_PLACEHOLDER.into(),
    ]
}

pub fn build_dsh_patch_yaml(patch_path: &Path, prompt: &str) -> Result<String, BackendError> {
    let path = serde_json::to_string(&patch_path.to_string_lossy())
        .map_err(|error| BackendError::new(BackendErrorCode::Internal, error.to_string()))?;
    let task = serde_json::to_string(prompt)
        .map_err(|error| BackendError::new(BackendErrorCode::Internal, error.to_string()))?;
    Ok(format!(
        "# Generated by OpenLess. Applied as the last patch layer for one headless run.\n\
- insert:\n\
    - id: dsh-events\n\
      name: {path}\n\
- id: headless-runner\n\
  config:\n\
    task: {task}\n"
    ))
}

/// 解析 Claude stream-json 的共享事件。
pub fn parse_claude_stream_line(session_id: &str, line: &str) -> Option<CodingAgentStreamEvent> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match value.get("type")?.as_str()? {
        "stream_event" => {
            let event = value.get("event")?;
            if event.get("type")?.as_str()? != "content_block_delta"
                || event.get("delta")?.get("type")?.as_str()? != "text_delta"
            {
                return None;
            }
            Some(CodingAgentStreamEvent::Delta {
                session_id: session_id.into(),
                text: event.get("delta")?.get("text")?.as_str()?.into(),
            })
        }
        "assistant" => {
            let content = value.get("message")?.get("content")?.as_array()?;
            for block in content {
                if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                        return Some(CodingAgentStreamEvent::ToolUse {
                            session_id: session_id.into(),
                            name: name.into(),
                        });
                    }
                }
            }
            None
        }
        "system" if value.get("subtype").and_then(|v| v.as_str()) == Some("compact_boundary") => {
            Some(CodingAgentStreamEvent::Compaction {
                session_id: session_id.into(),
            })
        }
        "result" => {
            let text = value
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if value
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                Some(CodingAgentStreamEvent::Error {
                    session_id: session_id.into(),
                    message: if text.is_empty() {
                        "agent 返回错误".into()
                    } else {
                        text
                    },
                })
            } else {
                Some(CodingAgentStreamEvent::Completed {
                    session_id: session_id.into(),
                    text,
                    cost_usd: value.get("total_cost_usd").and_then(|v| v.as_f64()),
                    duration_ms: value.get("duration_ms").and_then(|v| v.as_u64()),
                })
            }
        }
        _ => None,
    }
}

pub fn parse_opencode_stream_line(session_id: &str, line: &str) -> Option<CodingAgentStreamEvent> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match value.get("type")?.as_str()? {
        "text" => {
            let text = value.get("part")?.get("text")?.as_str()?;
            (!text.is_empty()).then(|| CodingAgentStreamEvent::Delta {
                session_id: session_id.into(),
                text: text.into(),
            })
        }
        "tool_use" => Some(CodingAgentStreamEvent::ToolUse {
            session_id: session_id.into(),
            name: value.get("part")?.get("tool")?.as_str()?.into(),
        }),
        "error" => Some(CodingAgentStreamEvent::Error {
            session_id: session_id.into(),
            message: value
                .pointer("/error/data/message")
                .or_else(|| value.pointer("/error/message"))
                .or_else(|| value.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("OpenCode 返回了未知错误")
                .into(),
        }),
        _ => None,
    }
}

pub fn parse_codex_stream_line(session_id: &str, line: &str) -> Option<CodingAgentStreamEvent> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match value.get("type")?.as_str()? {
        "item.completed"
            if value.pointer("/item/type").and_then(|v| v.as_str()) == Some("agent_message") =>
        {
            let text = value.pointer("/item/text")?.as_str()?;
            if text.is_empty() {
                return None;
            }
            Some(CodingAgentStreamEvent::Delta {
                session_id: session_id.into(),
                text: text.into(),
            })
        }
        "item.started"
            if value.pointer("/item/type").and_then(|v| v.as_str())
                == Some("command_execution") =>
        {
            Some(CodingAgentStreamEvent::ToolUse {
                session_id: session_id.into(),
                name: value
                    .pointer("/item/command")?
                    .as_str()?
                    .split_whitespace()
                    .next()
                    .unwrap_or("command")
                    .into(),
            })
        }
        "turn.failed" | "error" => Some(CodingAgentStreamEvent::Error {
            session_id: session_id.into(),
            message: value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("Codex 协议错误")
                .into(),
        }),
        _ => None,
    }
}

pub fn parse_dsh_stream_line(session_id: &str, line: &str) -> Option<CodingAgentStreamEvent> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match value.get("type")?.as_str()? {
        "text.delta" => {
            let text = value.get("text")?.as_str()?;
            (!text.is_empty()).then(|| CodingAgentStreamEvent::Delta {
                session_id: session_id.into(),
                text: text.into(),
            })
        }
        "tool.call" => Some(CodingAgentStreamEvent::ToolUse {
            session_id: session_id.into(),
            name: value.get("name")?.as_str()?.into(),
        }),
        "turn.end" if value.get("ok").and_then(|v| v.as_bool()) == Some(false) => {
            Some(CodingAgentStreamEvent::Error {
                session_id: session_id.into(),
                message: value
                    .pointer("/error/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("dsh 本轮执行失败")
                    .into(),
            })
        }
        _ => None,
    }
}

pub fn parse_coding_agent_models(output: &str) -> Vec<String> {
    let mut clean = String::with_capacity(output.len());
    let mut chars = output.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            let _ = chars.next();
            for code in chars.by_ref() {
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
        } else {
            clean.push(ch);
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    clean
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty() && line.contains('/') && !line.chars().any(char::is_whitespace)
        })
        .filter(|line| seen.insert((*line).to_string()))
        .map(str::to_string)
        .collect()
}

pub fn assess_command_risk(command: &str) -> CommandRiskAssessment {
    let lowered = command.to_lowercase();
    let Some((pattern, reason)) = HIGH_RISK_PATTERNS
        .iter()
        .find(|(pattern, _)| lowered.contains(pattern))
    else {
        return CommandRiskAssessment {
            risk: CommandRisk::Safe,
            reason: None,
        };
    };
    CommandRiskAssessment {
        risk: if deny_rule_for_pattern(pattern).is_some() {
            CommandRisk::RequiresApproval
        } else {
            CommandRisk::Denied
        },
        reason: Some((*reason).into()),
    }
}

const DSH_EVENTS_PLUGIN: &[u8] = include_bytes!("vendor/dsh-events.js");

pub fn build_agent_command(request: &CodingAgentRequest) -> Result<AgentCommand, BackendError> {
    let executable =
        normalize_coding_agent_executable(request.provider, request.executable.clone())?;
    let mut env = BTreeMap::new();
    let mut temporary_files = Vec::new();
    let (argv, prompt) = match request.provider {
        CodingAgentProvider::ClaudeCodeCli => {
            let approved = request
                .approved_patterns
                .iter()
                .flat_map(|pattern| crate::risk_equivalent_patterns(pattern))
                .filter_map(crate::deny_rule_for_pattern)
                .collect::<std::collections::HashSet<_>>();
            let mut deny = crate::default_deny_rules();
            deny.retain(|rule| !approved.contains(rule.as_str()));
            let settings_name = "claude-settings.json";
            temporary_files.push(AgentTemporaryFile {
                name: settings_name.into(),
                contents: serde_json::to_vec_pretty(&serde_json::json!({
                    "permissions": {
                        "defaultMode": request.permission_mode.as_cli_arg(),
                        "deny": deny,
                    }
                }))
                .map_err(|error| {
                    BackendError::new(BackendErrorCode::Internal, error.to_string())
                })?,
            });
            let mut resolved = request.clone();
            resolved.settings_json_path = Some(PathBuf::from(temporary_path_token(settings_name)));
            (
                build_claude_args(&resolved),
                PromptPayload::Stdin(request.prompt.clone()),
            )
        }
        CodingAgentProvider::OpenCodeCli => {
            env.insert(
                "OPENCODE_CONFIG_CONTENT".into(),
                serde_json::to_string(&crate::build_opencode_guard_config(
                    &request.approved_patterns,
                ))
                .map_err(|error| {
                    BackendError::new(BackendErrorCode::Internal, error.to_string())
                })?,
            );
            (
                build_opencode_args(request),
                PromptPayload::Argv(request.prompt.clone()),
            )
        }
        CodingAgentProvider::CodexCli => (
            build_codex_args(request),
            PromptPayload::Stdin(request.prompt.clone()),
        ),
        CodingAgentProvider::DshCli => {
            let task = if request.continue_session {
                request
                    .continuation_context
                    .as_deref()
                    .map(|context| format!("{context}\n\n当前任务：\n{}", request.prompt))
                    .unwrap_or_else(|| request.prompt.clone())
            } else {
                request.prompt.clone()
            };
            let plugin_name = "dsh-events.mjs";
            let patch_name = "openless.patch.yml";
            let plugin_token = temporary_json_path_token(plugin_name);
            temporary_files.push(AgentTemporaryFile {
                name: plugin_name.into(),
                contents: DSH_EVENTS_PLUGIN.to_vec(),
            });
            temporary_files.push(AgentTemporaryFile {
                name: patch_name.into(),
                contents: build_dsh_patch_yaml(Path::new(&plugin_token), &task)?.into_bytes(),
            });
            env.insert(
                "DSH_PERMISSION_MODE".into(),
                if request.permission_mode == CodingAgentPermissionMode::AcceptEdits {
                    "workspace-write"
                } else {
                    "read-only"
                }
                .into(),
            );
            env.insert("DSH_EVENTS_OUT".into(), "stderr".into());
            (
                build_dsh_args_with_patch(Path::new(&temporary_path_token(patch_name))),
                PromptPayload::PatchFile {
                    main_file: patch_name.into(),
                },
            )
        }
    };
    Ok(AgentCommand {
        executable,
        argv,
        env,
        cwd: request.cwd.clone(),
        prompt,
        temporary_files,
    })
}

/// Core-owned Coding Agent runner. The host only implements process I/O through
/// [`CodingAgentProcessAdapter`]; this type owns stream
/// filtering, aggregation and the single terminal outcome.
pub struct CodingAgentRunner {
    process: Arc<dyn CodingAgentProcessAdapter>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CodingAgentRunOutcome {
    Completed {
        text: String,
        cost_usd: Option<f64>,
        duration_ms: Option<u64>,
    },
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodingAgentRunResult {
    pub session_id: String,
    pub outcome: CodingAgentRunOutcome,
}

impl CodingAgentRunner {
    pub fn new(process: Arc<dyn CodingAgentProcessAdapter>) -> Self {
        Self { process }
    }

    pub fn run(
        &self,
        request: CodingAgentRequest,
        cancel: Arc<AtomicBool>,
    ) -> BoxFuture<'static, Result<CodingAgentRunResult, BackendError>> {
        self.run_streaming(request, cancel, None)
    }

    pub fn run_streaming(
        &self,
        request: CodingAgentRequest,
        cancel: Arc<AtomicBool>,
        stream_events: Option<tokio::sync::mpsc::UnboundedSender<CodingAgentStreamEvent>>,
    ) -> BoxFuture<'static, Result<CodingAgentRunResult, BackendError>> {
        let process = Arc::clone(&self.process);
        Box::pin(async move {
            if request.session_id.trim().is_empty() {
                return Err(invalid_argument("coding agent session id cannot be empty"));
            }
            let expected = request.session_id.clone();
            let timeout_secs = request.timeout_secs.max(1);
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            let operation = run_process(process, request, sender, Arc::clone(&cancel));
            tokio::pin!(operation);
            let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
            tokio::pin!(timeout);
            let mut text = String::new();
            let mut cost_usd = None;
            let mut duration_ms = None;
            let mut terminal = None;
            loop {
                tokio::select! {
                    result = &mut operation => {
                        result?;
                        break;
                    }
                    _ = &mut timeout => {
                        cancel.store(true, std::sync::atomic::Ordering::Release);
                        terminal = Some(CodingAgentRunOutcome::Failed("coding agent timed out".into()));
                        break;
                    }
                    event = receiver.recv() => {
                        let Some(event) = event else { break; };
                        if let Some(stream_events) = &stream_events {
                            let _ = stream_events.send(event.clone());
                        }
                        consume_runner_event(&expected, event, &mut text, &mut cost_usd, &mut duration_ms, &mut terminal);
                    }
                }
            }
            if terminal.is_none() {
                while let Ok(event) = receiver.try_recv() {
                    if let Some(stream_events) = &stream_events {
                        let _ = stream_events.send(event.clone());
                    }
                    consume_runner_event(
                        &expected,
                        event,
                        &mut text,
                        &mut cost_usd,
                        &mut duration_ms,
                        &mut terminal,
                    );
                }
            }
            let outcome = terminal.unwrap_or_else(|| {
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    CodingAgentRunOutcome::Cancelled
                } else if text.trim().is_empty() {
                    CodingAgentRunOutcome::Failed("coding agent returned no result".into())
                } else {
                    CodingAgentRunOutcome::Completed {
                        text: text.trim().into(),
                        cost_usd,
                        duration_ms,
                    }
                }
            });
            Ok(CodingAgentRunResult {
                session_id: expected,
                outcome,
            })
        })
    }
}

struct ProcessLineSink(tokio::sync::mpsc::UnboundedSender<ProcessOutputLine>);

impl ProcessOutputSink for ProcessLineSink {
    fn write(&self, line: ProcessOutputLine) {
        let _ = self.0.send(line);
    }
}

async fn run_process(
    process: Arc<dyn CodingAgentProcessAdapter>,
    request: CodingAgentRequest,
    events: tokio::sync::mpsc::UnboundedSender<CodingAgentStreamEvent>,
    cancel: Arc<AtomicBool>,
) -> Result<(), BackendError> {
    let _ = events.send(CodingAgentStreamEvent::Started {
        session_id: request.session_id.clone(),
    });
    if let Some(cwd) = &request.cwd {
        if let Ok((exit, stdout, _)) = execute_capture(
            Arc::clone(&process),
            simple_command(
                "git".into(),
                vec![
                    "-C".into(),
                    cwd.to_string_lossy().into_owned(),
                    "stash".into(),
                    "create".into(),
                    "openless-agent-pre-run".into(),
                ],
            ),
            std::time::Duration::from_secs(15),
        )
        .await
        {
            if exit.success && !stdout.trim().is_empty() {
                log::info!(
                    "[coding-agent] created recoverable Git snapshot {}",
                    stdout.trim()
                );
            }
        }
    }
    let command = build_agent_command(&request)?;
    let (line_sender, mut lines) = tokio::sync::mpsc::unbounded_channel();
    let output: Arc<dyn ProcessOutputSink> = Arc::new(ProcessLineSink(line_sender));
    let result = process
        .execute(
            command,
            output,
            CancellationToken::from_flag(cancel.clone()),
        )
        .await;
    let mut accumulated = String::new();
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut terminal = false;
    while let Ok(line) = lines.try_recv() {
        match line.stream {
            ProcessStream::Stdout => {
                if !stdout.is_empty() {
                    stdout.push('\n');
                }
                stdout.push_str(&line.line);
            }
            ProcessStream::Stderr if stderr.len() < 16 * 1024 => {
                if !stderr.is_empty() {
                    stderr.push('\n');
                }
                stderr.push_str(&line.line);
            }
            ProcessStream::Stderr => {}
        }
        if terminal {
            continue;
        }
        let event = match (request.provider, line.stream) {
            (CodingAgentProvider::ClaudeCodeCli, ProcessStream::Stdout) => {
                parse_claude_stream_line(&request.session_id, &line.line)
            }
            (CodingAgentProvider::OpenCodeCli, ProcessStream::Stdout) => {
                parse_opencode_stream_line(&request.session_id, &line.line)
            }
            (CodingAgentProvider::CodexCli, ProcessStream::Stdout) => {
                parse_codex_stream_line(&request.session_id, &line.line)
            }
            (CodingAgentProvider::DshCli, ProcessStream::Stderr) => {
                parse_dsh_stream_line(&request.session_id, &line.line)
            }
            _ => None,
        };
        if let Some(event) = event {
            match &event {
                CodingAgentStreamEvent::Delta { text, .. } => accumulated.push_str(text),
                CodingAgentStreamEvent::Completed { .. }
                | CodingAgentStreamEvent::Error { .. }
                | CodingAgentStreamEvent::Cancelled { .. } => terminal = true,
                _ => {}
            }
            let _ = events.send(event);
        }
    }
    if terminal {
        return Ok(());
    }
    if cancel.load(Ordering::Acquire) {
        let _ = events.send(CodingAgentStreamEvent::Cancelled {
            session_id: request.session_id,
        });
        return Ok(());
    }
    let exit = match result {
        Ok(exit) => exit,
        Err(error) => return Err(error),
    };
    if !exit.success {
        let message = summarize_stderr(&stderr)
            .unwrap_or_else(|| format!("coding agent exited with code {:?}", exit.code));
        let _ = events.send(CodingAgentStreamEvent::Error {
            session_id: request.session_id,
            message,
        });
        return Ok(());
    }
    let final_text = if request.provider == CodingAgentProvider::DshCli {
        stdout.trim().to_string()
    } else {
        accumulated.trim().to_string()
    };
    let _ = events.send(CodingAgentStreamEvent::Completed {
        session_id: request.session_id,
        text: final_text,
        cost_usd: None,
        duration_ms: None,
    });
    Ok(())
}

fn summarize_stderr(stderr: &str) -> Option<String> {
    let mut lines = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    lines
        .clone()
        .find(|line| line.starts_with("dsh:") || line.contains("error:"))
        .or_else(|| lines.next_back())
        .map(str::to_string)
}

fn consume_runner_event(
    expected: &str,
    event: CodingAgentStreamEvent,
    text: &mut String,
    cost_usd: &mut Option<f64>,
    duration_ms: &mut Option<u64>,
    terminal: &mut Option<CodingAgentRunOutcome>,
) {
    if terminal.is_some() {
        return;
    }
    match event {
        CodingAgentStreamEvent::Delta {
            session_id,
            text: delta,
        } if session_id == expected => text.push_str(&delta),
        CodingAgentStreamEvent::Completed {
            session_id,
            text: value,
            cost_usd: cost,
            duration_ms: duration,
        } if session_id == expected => {
            *text = value;
            *cost_usd = cost;
            *duration_ms = duration;
            *terminal = Some(if text.trim().is_empty() {
                CodingAgentRunOutcome::Failed("coding agent returned no result".into())
            } else {
                CodingAgentRunOutcome::Completed {
                    text: text.trim().into(),
                    cost_usd: cost,
                    duration_ms: duration,
                }
            });
        }
        CodingAgentStreamEvent::Error {
            session_id,
            message,
        } if session_id == expected => *terminal = Some(CodingAgentRunOutcome::Failed(message)),
        CodingAgentStreamEvent::Cancelled { session_id } if session_id == expected => {
            *terminal = Some(CodingAgentRunOutcome::Cancelled)
        }
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentDetectRequest {
    pub provider: CodingAgentProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpHealth {
    Connected,
    Failed,
    NeedsAuth,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub name: String,
    pub detail: String,
    pub health: McpHealth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentAvailability {
    pub provider: CodingAgentProvider,
    pub installed: bool,
    pub executable: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerStatus>,
    pub has_computer_use: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentModelsRequest {
    pub provider: CodingAgentProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    #[serde(default = "default_refresh_models")]
    pub refresh: bool,
}

const fn default_refresh_models() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentTestRequest {
    pub provider: CodingAgentProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub permission_mode: CodingAgentPermissionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    #[serde(default = "default_test_timeout_secs")]
    pub timeout_secs: u64,
}

const fn default_test_timeout_secs() -> u64 {
    120
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedCodingAgentTestRequest {
    pub provider: CodingAgentProvider,
    pub executable: String,
    pub prompt: String,
    pub permission_mode: CodingAgentPermissionMode,
    pub workdir: Option<PathBuf>,
    pub model: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentTestStatus {
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandRisk {
    Safe,
    RequiresApproval,
    Denied,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandRiskAssessment {
    pub risk: CommandRisk,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub fn normalize_coding_agent_executable(
    provider: CodingAgentProvider,
    executable: Option<String>,
) -> Result<String, BackendError> {
    let executable = executable
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| provider.default_exe().to_string());
    if executable.contains('\0') {
        return Err(invalid_argument("executable contains a null byte"));
    }
    let has_separator = executable.contains('/') || executable.contains('\\');
    if !has_separator {
        return Ok(executable);
    }
    let path = Path::new(&executable);
    if !path.is_absolute() {
        return Err(invalid_argument(
            "executable must be a bare command name or an absolute path",
        ));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(invalid_argument("executable path cannot contain '..'"));
    }
    Ok(executable)
}

pub fn normalize_coding_agent_test_request(
    request: CodingAgentTestRequest,
) -> Result<NormalizedCodingAgentTestRequest, BackendError> {
    let prompt = request.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(invalid_argument("coding agent prompt cannot be empty"));
    }
    let executable = normalize_coding_agent_executable(request.provider, request.executable)?;
    let workdir = match request.workdir {
        Some(path) if !path.as_os_str().is_empty() => {
            if !path.is_absolute() {
                return Err(invalid_argument("coding agent workdir must be absolute"));
            }
            if path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            {
                return Err(invalid_argument("coding agent workdir cannot contain '..'"));
            }
            Some(path)
        }
        _ => None,
    };
    if !(1..=3600).contains(&request.timeout_secs) {
        return Err(invalid_argument(
            "coding agent timeout must be between 1 and 3600 seconds",
        ));
    }
    if let Some(budget) = request.max_budget_usd {
        if !budget.is_finite() || budget <= 0.0 {
            return Err(invalid_argument(
                "coding agent budget must be a positive finite number",
            ));
        }
        match request.provider.max_budget_usd() {
            Some(maximum) if budget <= maximum => {}
            Some(maximum) => {
                return Err(invalid_argument(format!(
                    "coding agent budget cannot exceed {maximum} USD"
                )))
            }
            None => {
                return Err(invalid_argument(
                    "selected coding agent provider does not support a hard USD budget",
                ))
            }
        }
    }
    let permission_mode = match (request.provider, request.permission_mode) {
        (
            CodingAgentProvider::CodexCli | CodingAgentProvider::DshCli,
            CodingAgentPermissionMode::Default | CodingAgentPermissionMode::BypassPermissions,
        ) => CodingAgentPermissionMode::Plan,
        (_, mode) => mode,
    };
    Ok(NormalizedCodingAgentTestRequest {
        provider: request.provider,
        executable,
        prompt,
        permission_mode,
        workdir,
        model: resolve_coding_agent_model(request.provider, request.model),
        max_budget_usd: request.max_budget_usd,
        timeout_secs: request.timeout_secs,
    })
}

fn invalid_argument(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::InvalidArgument, message)
}

pub fn parse_cli_version(output: &str) -> Option<String> {
    for raw in output.split_whitespace() {
        let Some(start) = raw.find(|character: char| character.is_ascii_digit()) else {
            continue;
        };
        let candidate = &raw[start..];
        let mut parts = candidate.splitn(3, '.');
        let (Some(major), Some(minor), Some(rest)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let all_digits =
            |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
        if !all_digits(major) || !all_digits(minor) {
            continue;
        }
        let patch_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        if patch_len == 0 {
            continue;
        }
        let tail = &rest[patch_len..];
        let keep = usize::from(tail.starts_with('-') || tail.starts_with('+')) * tail.len();
        return Some(format!("{major}.{minor}.{}", &rest[..patch_len + keep]));
    }
    None
}

pub fn parse_claude_version(output: &str) -> Option<String> {
    parse_cli_version(output)
}

pub fn parse_mcp_list(output: &str) -> Vec<McpServerStatus> {
    let mut servers = Vec::new();
    for line in output.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("Checking") {
            continue;
        }
        let Some((name, rest)) = line.split_once(": ") else {
            continue;
        };
        let (detail, status) = match rest.rfind(" - ") {
            Some(index) => (rest[..index].trim(), rest[index + 3..].trim()),
            None => (rest.trim(), ""),
        };
        let health = if status.contains("Connected") {
            McpHealth::Connected
        } else if status.contains("Failed") {
            McpHealth::Failed
        } else if status.contains("authentication") || status.contains("Needs") {
            McpHealth::NeedsAuth
        } else {
            McpHealth::Unknown
        };
        servers.push(McpServerStatus {
            name: name.trim().to_string(),
            detail: detail.to_string(),
            health,
        });
    }
    servers
}

pub fn has_computer_use_mcp(servers: &[McpServerStatus]) -> bool {
    servers.iter().any(|server| {
        let name = server.name.to_lowercase();
        name.contains("computer") || name.contains("desktop") || name.contains("screen")
    })
}

pub struct CodingAgentService {
    runner: Arc<CodingAgentRunner>,
    process: Arc<dyn CodingAgentProcessAdapter>,
    less_computer: Arc<dyn crate::domains::LessComputerApi>,
    events: BackendEventPublisher,
    active_test: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    request_counter: AtomicU64,
}

impl CodingAgentService {
    pub fn new(
        runner: Arc<CodingAgentRunner>,
        process: Arc<dyn CodingAgentProcessAdapter>,
        less_computer: Arc<dyn crate::domains::LessComputerApi>,
        events: BackendEventPublisher,
    ) -> Self {
        Self {
            runner,
            process,
            less_computer,
            events,
            active_test: Arc::new(Mutex::new(None)),
            request_counter: AtomicU64::new(0),
        }
    }

    fn next_request_id(&self) -> String {
        format!(
            "console-{}",
            self.request_counter.fetch_add(1, Ordering::Relaxed) + 1
        )
    }
}

#[derive(Default)]
struct CapturedProcessOutput(Mutex<Vec<ProcessOutputLine>>);

impl ProcessOutputSink for CapturedProcessOutput {
    fn write(&self, line: ProcessOutputLine) {
        self.0
            .lock()
            .expect("coding agent output lock poisoned")
            .push(line);
    }
}

async fn execute_capture(
    process: Arc<dyn CodingAgentProcessAdapter>,
    command: AgentCommand,
    timeout: std::time::Duration,
) -> Result<(ProcessExit, String, String), BackendError> {
    let output = Arc::new(CapturedProcessOutput::default());
    let cancellation = CancellationToken::new();
    let result = tokio::time::timeout(
        timeout,
        process.execute(command, output.clone(), cancellation.clone()),
    )
    .await;
    let exit = match result {
        Ok(result) => result?,
        Err(_) => {
            cancellation.cancel();
            return Err(BackendError::new(
                BackendErrorCode::Provider,
                "coding agent command timed out",
            )
            .retryable(true));
        }
    };
    let lines = output.0.lock().expect("coding agent output lock poisoned");
    let collect = |stream| {
        lines
            .iter()
            .filter(|line| line.stream == stream)
            .map(|line| line.line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((
        exit,
        collect(ProcessStream::Stdout),
        collect(ProcessStream::Stderr),
    ))
}

fn simple_command(executable: String, argv: Vec<String>) -> AgentCommand {
    AgentCommand {
        executable,
        argv,
        env: BTreeMap::new(),
        cwd: None,
        prompt: PromptPayload::Stdin(String::new()),
        temporary_files: Vec::new(),
    }
}

impl crate::domains::CodingAgentApi for CodingAgentService {
    fn detect(
        &self,
        request: CodingAgentDetectRequest,
    ) -> BoxFuture<'static, Result<CodingAgentAvailability, BackendError>> {
        let process = Arc::clone(&self.process);
        Box::pin(async move {
            let executable =
                normalize_coding_agent_executable(request.provider, request.executable)?;
            let probe = execute_capture(
                Arc::clone(&process),
                simple_command(executable.clone(), vec!["--version".into()]),
                std::time::Duration::from_secs(10),
            )
            .await;
            let (installed, version) = match probe {
                Ok((exit, stdout, stderr)) => {
                    let output = format!("{stdout}\n{stderr}");
                    (exit.success, parse_cli_version(&output))
                }
                Err(_) => (false, None),
            };
            let mcp_servers = if installed && request.provider == CodingAgentProvider::ClaudeCodeCli
            {
                match execute_capture(
                    process,
                    simple_command(executable.clone(), vec!["mcp".into(), "list".into()]),
                    std::time::Duration::from_secs(15),
                )
                .await
                {
                    Ok((exit, stdout, stderr)) if exit.success => {
                        parse_mcp_list(&format!("{stdout}\n{stderr}"))
                    }
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            let has_computer_use = has_computer_use_mcp(&mcp_servers);
            Ok(CodingAgentAvailability {
                provider: request.provider,
                installed,
                executable,
                version,
                mcp_servers,
                has_computer_use,
            })
        })
    }

    fn list_models(
        &self,
        request: CodingAgentModelsRequest,
    ) -> BoxFuture<'static, Result<Vec<String>, BackendError>> {
        let process = Arc::clone(&self.process);
        Box::pin(async move {
            if request.provider != CodingAgentProvider::OpenCodeCli {
                return Err(BackendError::new(
                    BackendErrorCode::Unsupported,
                    "selected coding agent provider does not expose a model-list command",
                ));
            }
            let executable =
                normalize_coding_agent_executable(request.provider, request.executable)?;
            let mut argv = vec!["models".into()];
            if request.refresh {
                argv.push("--refresh".into());
            }
            let (exit, stdout, stderr) = execute_capture(
                process,
                simple_command(executable, argv),
                std::time::Duration::from_secs(45),
            )
            .await?;
            if !exit.success {
                return Err(BackendError::new(
                    BackendErrorCode::Provider,
                    summarize_stderr(&stderr)
                        .unwrap_or_else(|| "OpenCode model command failed".into()),
                ));
            }
            let models = parse_coding_agent_models(&stdout);
            if models.is_empty() {
                Err(BackendError::new(
                    BackendErrorCode::Provider,
                    "OpenCode returned no available models",
                ))
            } else {
                Ok(models)
            }
        })
    }

    fn command_risk(
        &self,
        command: String,
    ) -> BoxFuture<'static, Result<CommandRiskAssessment, BackendError>> {
        Box::pin(async move { Ok(assess_command_risk(&command)) })
    }

    fn run_test(
        &self,
        request: CodingAgentTestRequest,
    ) -> BoxFuture<'static, Result<CodingAgentTestStatus, BackendError>> {
        let normalized = normalize_coding_agent_test_request(request);
        let runner = Arc::clone(&self.runner);
        let active_test = Arc::clone(&self.active_test);
        let events = self.events.clone();
        let request_id = self.next_request_id();
        Box::pin(async move {
            let normalized = normalized?;
            let cancel = Arc::new(AtomicBool::new(false));
            {
                let mut active = active_test.lock().expect("coding agent test lock poisoned");
                if active.is_some() {
                    return Err(BackendError::new(
                        BackendErrorCode::Busy,
                        "a coding agent test is already running",
                    ));
                }
                *active = Some(Arc::clone(&cancel));
            }
            let mut agent = CodingAgentRequest::new(request_id.clone(), normalized.prompt);
            agent.provider = normalized.provider;
            agent.executable = Some(normalized.executable);
            agent.cwd = normalized.workdir;
            agent.model = normalized.model;
            agent.permission_mode = normalized.permission_mode;
            agent.max_budget_usd = normalized.max_budget_usd;
            agent.timeout_secs = normalized.timeout_secs;
            agent.session_persistence = false;
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            let result = runner
                .run_streaming(agent, Arc::clone(&cancel), Some(sender))
                .await;
            while let Ok(event) = receiver.try_recv() {
                events.publish(None, BackendEventKind::CodingAgentTest(event));
            }
            let mut active = active_test.lock().expect("coding agent test lock poisoned");
            if active
                .as_ref()
                .is_some_and(|value| Arc::ptr_eq(value, &cancel))
            {
                active.take();
            }
            match result?.outcome {
                CodingAgentRunOutcome::Completed { .. } => Ok(CodingAgentTestStatus {
                    running: false,
                    request_id: Some(request_id),
                    message: None,
                }),
                CodingAgentRunOutcome::Cancelled => Err(BackendError::new(
                    BackendErrorCode::Cancelled,
                    "coding agent test cancelled",
                )),
                CodingAgentRunOutcome::Failed(message) => {
                    Err(BackendError::new(BackendErrorCode::Provider, message))
                }
            }
        })
    }

    fn cancel_test(&self) -> BoxFuture<'static, Result<(), BackendError>> {
        let active_test = Arc::clone(&self.active_test);
        Box::pin(async move {
            if let Some(cancel) = active_test
                .lock()
                .expect("coding agent test lock poisoned")
                .clone()
            {
                cancel.store(true, Ordering::Release);
            }
            Ok(())
        })
    }

    fn approve(
        &self,
        token: String,
        approved: bool,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        self.less_computer.approve(token, approved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScriptedProcess(Vec<ProcessOutputLine>, Result<ProcessExit, BackendError>);

    impl CodingAgentProcessAdapter for ScriptedProcess {
        fn execute(
            &self,
            _command: AgentCommand,
            output: Arc<dyn ProcessOutputSink>,
            _cancel: CancellationToken,
        ) -> BoxFuture<'static, Result<ProcessExit, BackendError>> {
            let lines = self.0.clone();
            let result = self.1.clone();
            Box::pin(async move {
                for line in lines {
                    output.write(line);
                }
                result
            })
        }
    }

    fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .map(String::as_str)
    }

    #[test]
    fn provider_ids_round_trip_through_preferences_and_serde() {
        let cases = [
            (CodingAgentProvider::ClaudeCodeCli, "claude-code-cli"),
            (CodingAgentProvider::OpenCodeCli, "opencode-cli"),
            (CodingAgentProvider::CodexCli, "codex-cli"),
            (CodingAgentProvider::DshCli, "dsh-cli"),
        ];
        for (provider, value) in cases {
            assert_eq!(CodingAgentProvider::from_pref(value), provider);
            assert_eq!(provider.as_pref(), value);
            assert_eq!(
                serde_json::to_string(&provider).unwrap(),
                format!("\"{value}\"")
            );
        }
        assert_eq!(
            CodingAgentProvider::from_pref("unknown-provider"),
            CodingAgentProvider::ClaudeCodeCli
        );
    }

    #[test]
    fn provider_capabilities_are_explicit() {
        assert!(CodingAgentProvider::ClaudeCodeCli.supports_command_approval());
        assert!(CodingAgentProvider::OpenCodeCli.supports_command_approval());
        assert!(!CodingAgentProvider::CodexCli.supports_command_approval());
        assert!(!CodingAgentProvider::DshCli.supports_command_approval());
        assert_eq!(CodingAgentProvider::ClaudeCodeCli.default_exe(), "claude");
        assert_eq!(CodingAgentProvider::OpenCodeCli.default_exe(), "opencode");
        assert_eq!(CodingAgentProvider::CodexCli.default_exe(), "codex");
        assert_eq!(CodingAgentProvider::DshCli.default_exe(), "dsh");
        assert_eq!(
            CodingAgentProvider::ClaudeCodeCli.max_budget_usd(),
            Some(2.0)
        );
        assert_eq!(CodingAgentProvider::OpenCodeCli.max_budget_usd(), None);
    }

    #[test]
    fn models_follow_provider_specific_contracts() {
        assert_eq!(
            resolve_coding_agent_model(CodingAgentProvider::ClaudeCodeCli, None),
            Some("sonnet".into())
        );
        assert_eq!(
            resolve_coding_agent_model(CodingAgentProvider::OpenCodeCli, Some("sonnet".into())),
            None
        );
        assert_eq!(
            resolve_coding_agent_model(
                CodingAgentProvider::OpenCodeCli,
                Some("openai/gpt-5".into())
            ),
            Some("openai/gpt-5".into())
        );
        assert_eq!(
            resolve_coding_agent_model(CodingAgentProvider::CodexCli, Some(" gpt-5 ".into())),
            Some("gpt-5".into())
        );
        assert_eq!(
            resolve_coding_agent_model(CodingAgentProvider::DshCli, Some("ignored".into())),
            None
        );
    }

    #[test]
    fn claude_args_are_headless_and_keep_prompt_out_of_process_list() {
        let mut request = CodingAgentRequest::new("session", "secret prompt");
        request.cwd = Some(PathBuf::from("/tmp/work"));
        request.model = Some("sonnet".into());
        request.fallback_model = Some("haiku".into());
        request.permission_mode = CodingAgentPermissionMode::Plan;
        request.allowed_tools = vec!["Read".into(), "Edit".into()];
        request.disallowed_tools = vec!["Bash(rm:*)".into()];
        request.max_budget_usd = Some(0.5);
        request.settings_json_path = Some(PathBuf::from("/tmp/guard.json"));
        request.extra_system_prompt = Some("be terse".into());
        request.session_persistence = false;
        request.continue_session = true;

        let args = build_claude_args(&request);
        assert_eq!(arg_value(&args, "--output-format"), Some("stream-json"));
        assert_eq!(arg_value(&args, "--permission-mode"), Some("plan"));
        assert_eq!(arg_value(&args, "--model"), Some("sonnet"));
        assert_eq!(arg_value(&args, "--fallback-model"), Some("haiku"));
        assert_eq!(arg_value(&args, "--add-dir"), Some("/tmp/work"));
        assert_eq!(arg_value(&args, "--allowedTools"), Some("Read,Edit"));
        assert_eq!(arg_value(&args, "--disallowedTools"), Some("Bash(rm:*)"));
        assert_eq!(arg_value(&args, "--max-budget-usd"), Some("0.5"));
        assert_eq!(arg_value(&args, "--settings"), Some("/tmp/guard.json"));
        assert_eq!(arg_value(&args, "--append-system-prompt"), Some("be terse"));
        assert!(args.contains(&"--no-session-persistence".into()));
        assert!(args.contains(&"--continue".into()));
        assert!(!args.iter().any(|arg| arg.contains("secret prompt")));
    }

    #[test]
    fn every_provider_has_a_distinct_headless_command_shape() {
        let mut request = CodingAgentRequest::new("session", "prompt");
        request.permission_mode = CodingAgentPermissionMode::AcceptEdits;
        assert_eq!(build_opencode_args(&request)[0], "run");
        assert_eq!(
            build_opencode_args(&request).last().map(String::as_str),
            Some("--")
        );
        assert_eq!(
            build_codex_args(&request).first().map(String::as_str),
            Some("exec")
        );
        assert_eq!(
            build_codex_args(&request).last().map(String::as_str),
            Some("-")
        );
        assert_eq!(build_dsh_args(&request), vec!["--profile", "headless"]);
        assert_ne!(build_opencode_args(&request), vec!["-p"]);
        assert_ne!(build_codex_args(&request), vec!["-p"]);
        assert_ne!(build_dsh_args(&request), vec!["-p"]);
    }

    #[test]
    fn command_payloads_keep_hostile_prompts_out_of_flags() {
        let prompt = "--\n-line\n".repeat(10_000);
        for provider in [
            CodingAgentProvider::ClaudeCodeCli,
            CodingAgentProvider::OpenCodeCli,
            CodingAgentProvider::CodexCli,
            CodingAgentProvider::DshCli,
        ] {
            let mut request = CodingAgentRequest::new("session", prompt.clone());
            request.provider = provider;
            let command = build_agent_command(&request).unwrap();
            assert!(!command
                .argv
                .iter()
                .any(|argument| argument.contains("-line")));
            match provider {
                CodingAgentProvider::ClaudeCodeCli | CodingAgentProvider::CodexCli => {
                    assert!(
                        matches!(command.prompt, PromptPayload::Stdin(value) if value == prompt)
                    );
                }
                CodingAgentProvider::OpenCodeCli => {
                    assert!(
                        matches!(command.prompt, PromptPayload::Argv(value) if value == prompt)
                    );
                }
                CodingAgentProvider::DshCli => {
                    assert!(matches!(command.prompt, PromptPayload::PatchFile { .. }));
                    assert!(command.temporary_files.iter().any(|file| {
                        file.name == "openless.patch.yml"
                            && String::from_utf8_lossy(&file.contents).contains("-line")
                    }));
                    assert!(command
                        .temporary_files
                        .iter()
                        .any(|file| file.name == "dsh-events.mjs"));
                }
            }
        }
    }

    #[tokio::test]
    async fn first_terminal_event_survives_late_error_and_process_failure() {
        let lines = vec![
            ProcessOutputLine {
                stream: ProcessStream::Stdout,
                line: r#"{"type":"result","result":"done"}"#.into(),
            },
            ProcessOutputLine {
                stream: ProcessStream::Stdout,
                line: r#"{"type":"result","is_error":true,"result":"late"}"#.into(),
            },
        ];
        let process = Arc::new(ScriptedProcess(
            lines,
            Err(BackendError::new(
                BackendErrorCode::Provider,
                "process failed late",
            )),
        ));
        let runner = CodingAgentRunner::new(process);
        let result = runner
            .run(
                CodingAgentRequest::new("session", "task"),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
        assert_eq!(
            result.outcome,
            CodingAgentRunOutcome::Completed {
                text: "done".into(),
                cost_usd: None,
                duration_ms: None,
            }
        );
    }

    #[test]
    fn shared_stream_parsers_cover_all_provider_protocols() {
        assert!(matches!(
            parse_claude_stream_line(
                "s",
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"ok"}}}"#
            ),
            Some(CodingAgentStreamEvent::Delta { .. })
        ));
        assert!(matches!(
            parse_opencode_stream_line("s", r#"{"type":"text","part":{"text":"ok"}}"#),
            Some(CodingAgentStreamEvent::Delta { .. })
        ));
        assert!(matches!(
            parse_codex_stream_line(
                "s",
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}"#
            ),
            Some(CodingAgentStreamEvent::Delta { .. })
        ));
        assert!(matches!(
            parse_dsh_stream_line("s", r#"{"v":1,"type":"text.delta","text":"ok"}"#),
            Some(CodingAgentStreamEvent::Delta { .. })
        ));
        assert_eq!(
            parse_coding_agent_models("\u{1b}[32mopenai/gpt-5\u{1b}[0m\nopenai/gpt-5\n"),
            vec!["openai/gpt-5"]
        );
    }

    #[test]
    fn command_risk_is_fail_closed_and_never_unknown() {
        assert_eq!(assess_command_risk("git status").risk, CommandRisk::Safe);
        assert_eq!(
            assess_command_risk("git push --force origin main").risk,
            CommandRisk::RequiresApproval
        );
        assert_eq!(assess_command_risk("sudo reboot").risk, CommandRisk::Denied);
    }

    #[test]
    fn executable_normalization_accepts_only_bare_names_or_absolute_paths() {
        assert_eq!(
            normalize_coding_agent_executable(CodingAgentProvider::ClaudeCodeCli, None).unwrap(),
            "claude"
        );
        assert_eq!(
            normalize_coding_agent_executable(
                CodingAgentProvider::OpenCodeCli,
                Some(" custom-opencode ".into())
            )
            .unwrap(),
            "custom-opencode"
        );
        let absolute = std::env::temp_dir().join("openless-codex");
        assert_eq!(
            normalize_coding_agent_executable(
                CodingAgentProvider::CodexCli,
                Some(absolute.to_string_lossy().into_owned())
            )
            .unwrap(),
            absolute.to_string_lossy()
        );
        for invalid in ["../claude", "bin/claude", "bin\\claude", "bad\0exe"] {
            let error = normalize_coding_agent_executable(
                CodingAgentProvider::ClaudeCodeCli,
                Some(invalid.into()),
            )
            .unwrap_err();
            assert_eq!(error.code, BackendErrorCode::InvalidArgument);
        }
    }

    #[test]
    fn test_request_is_normalized_and_validated_before_reaching_an_adapter() {
        let workdir = std::env::temp_dir();
        let normalized = normalize_coding_agent_test_request(CodingAgentTestRequest {
            provider: CodingAgentProvider::ClaudeCodeCli,
            executable: Some(" claude ".into()),
            prompt: "  inspect this repository  ".into(),
            permission_mode: CodingAgentPermissionMode::AcceptEdits,
            workdir: Some(workdir.clone()),
            model: None,
            max_budget_usd: Some(0.5),
            timeout_secs: 120,
        })
        .unwrap();
        assert_eq!(normalized.prompt, "inspect this repository");
        assert_eq!(normalized.executable, "claude");
        assert_eq!(normalized.model.as_deref(), Some("sonnet"));
        assert_eq!(normalized.workdir, Some(workdir));

        let invalid_cases = [
            CodingAgentTestRequest {
                prompt: "   ".into(),
                ..test_request()
            },
            CodingAgentTestRequest {
                max_budget_usd: Some(f64::NAN),
                ..test_request()
            },
            CodingAgentTestRequest {
                max_budget_usd: Some(2.5),
                ..test_request()
            },
            CodingAgentTestRequest {
                timeout_secs: 0,
                ..test_request()
            },
            CodingAgentTestRequest {
                workdir: Some(PathBuf::from("relative/work")),
                ..test_request()
            },
        ];
        for request in invalid_cases {
            let error = normalize_coding_agent_test_request(request).unwrap_err();
            assert_eq!(error.code, BackendErrorCode::InvalidArgument);
        }
    }

    #[test]
    fn sandbox_providers_fail_closed_for_legacy_wide_permission_values() {
        for provider in [CodingAgentProvider::CodexCli, CodingAgentProvider::DshCli] {
            for mode in [
                CodingAgentPermissionMode::Default,
                CodingAgentPermissionMode::BypassPermissions,
            ] {
                let normalized = normalize_coding_agent_test_request(CodingAgentTestRequest {
                    provider,
                    permission_mode: mode,
                    max_budget_usd: None,
                    ..test_request()
                })
                .unwrap();
                assert_eq!(normalized.permission_mode, CodingAgentPermissionMode::Plan);
            }
        }
    }

    #[test]
    fn cli_versions_include_prerelease_and_ignore_layout_noise() {
        let cases = [
            ("2.1.161 (Claude Code)", Some("2.1.161")),
            ("Claude Code version 2.1.161", Some("2.1.161")),
            ("codex-cli 0.146.0", Some("0.146.0")),
            ("0.1.0-rc.6", Some("0.1.0-rc.6")),
            ("2.0.0+build.7", Some("2.0.0+build.7")),
            ("(1.2.3)", Some("1.2.3")),
            ("1.2", None),
            ("no version", None),
        ];
        for (output, expected) in cases {
            assert_eq!(parse_cli_version(output).as_deref(), expected);
        }
    }

    #[test]
    fn mcp_list_parsing_preserves_detail_and_classifies_health() {
        let output = "Checking MCP server health…\n\
memory: npx -y @modelcontextprotocol/server-memory - ✓ Connected\n\
desktop: https://desktop-control.example/mcp (HTTP) - ! Needs authentication\n\
broken: npx broken - ✗ Failed to connect\n";
        let servers = parse_mcp_list(output);
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].health, McpHealth::Connected);
        assert_eq!(servers[1].health, McpHealth::NeedsAuth);
        assert!(servers[1].detail.contains("desktop-control.example"));
        assert_eq!(servers[2].health, McpHealth::Failed);
        assert!(has_computer_use_mcp(&servers));
        assert!(!has_computer_use_mcp(&[McpServerStatus {
            name: "memory".into(),
            detail: String::new(),
            health: McpHealth::Connected,
        }]));
    }

    #[test]
    #[ignore = "requires the pinned Codex CLI"]
    fn hardening_actually_narrows_the_writable_roots() {
        let dir = std::env::temp_dir().join(format!(
            "openless-codex-sandbox-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let codex = std::env::var("OPENLESS_CODEX_TEST_EXE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "codex".to_string());
        let run = |extra: &[String]| {
            let output = std::process::Command::new(&codex)
                .args(["debug", "prompt-input"])
                .args(["-c", "sandbox_mode=\"workspace-write\""])
                .args(extra)
                .arg("x")
                .current_dir(&dir)
                .output()
                .expect("Codex CLI must run");
            assert!(output.status.success(), "Codex CLI debug command failed");
            String::from_utf8_lossy(&output.stdout).into_owned()
        };

        let mut request = CodingAgentRequest::new("sandbox", "test");
        request.provider = CodingAgentProvider::CodexCli;
        request.permission_mode = CodingAgentPermissionMode::AcceptEdits;
        let args = build_codex_args(&request);
        let hardening = args
            .iter()
            .enumerate()
            .filter(|(index, arg)| {
                arg.as_str() == "-c" || (*index > 0 && arg.starts_with("sandbox_workspace_write."))
            })
            .map(|(_, arg)| arg.clone())
            .collect::<Vec<_>>();
        assert!(!hardening.is_empty(), "Codex sandbox hardening is missing");

        let before = run(&[]);
        let after = run(&hardening);
        let roots = |output: &str| {
            output
                .split("writable root")
                .nth(1)
                .unwrap_or_default()
                .chars()
                .take(400)
                .collect::<String>()
        };
        let before = roots(&before);
        let after = roots(&after);
        let count_roots = |output: &str| output.matches('`').count() / 2;
        assert!(
            count_roots(&before) > 1,
            "Codex default writable roots changed: {before}"
        );
        assert_eq!(
            count_roots(&after),
            1,
            "Codex sandbox hardening no longer limits writable roots: {after}"
        );
        let cwd = std::fs::canonicalize(&dir).unwrap();
        assert!(after.contains(&cwd.to_string_lossy().into_owned()));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn test_request() -> CodingAgentTestRequest {
        CodingAgentTestRequest {
            provider: CodingAgentProvider::ClaudeCodeCli,
            executable: None,
            prompt: "test".into(),
            permission_mode: CodingAgentPermissionMode::AcceptEdits,
            workdir: None,
            model: None,
            max_budget_usd: Some(0.5),
            timeout_secs: 120,
        }
    }
}
