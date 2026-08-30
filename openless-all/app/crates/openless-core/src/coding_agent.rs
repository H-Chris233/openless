//! Coding Agent 的跨宿主请求类型、参数归一化和纯业务规则。
//!
//! 进程创建、Git 快照、临时配置文件和事件转发属于宿主 Adapter；本模块只保留两个宿主
//! 必须共享的规则，避免 Tauri 与 Linux 各维护一份 provider/权限/模型语义。

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::{BackendError, BackendErrorCode};

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

#[cfg(test)]
mod tests {
    use super::*;

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
