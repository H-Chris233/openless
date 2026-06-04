//! 无头 Claude Code 调用子系统（「快速 Agent」后端）。
//!
//! - [`args`]：`claude -p` 参数构造。
//! - [`stream`]：stream-json 输出解析为 [`stream::CodingAgentEvent`]。
//! - [`guard`]：高风险命令分类 + `--settings` 护栏 JSON。
//! - [`detect`]：解析 `claude --version` / `claude mcp list`。
//!
//! 本模块只负责「跑无头 Claude 并把事件抛出来」，不碰录音 / ASR / 前端——
//! 那些由 coordinator 串联（镜像现有 QA 链路）。

pub mod args;
pub mod commands;
pub mod detect;
pub mod guard;
pub mod stream;

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

pub use args::{build_claude_args, CodingAgentPermissionMode, CodingAgentRequest};
pub use detect::McpServerStatus;
pub use stream::{parse_stream_json_line, CodingAgentEvent};

/// 运行器把事件投递到这个 sink（coordinator / 命令层再转成 Tauri event）。
pub type CodingAgentEventSink = tokio::sync::mpsc::UnboundedSender<CodingAgentEvent>;

#[derive(Debug, thiserror::Error)]
pub enum CodingAgentError {
    #[error("找不到可执行文件: {0}")]
    ExecutableNotFound(String),
    #[error("启动 agent 进程失败: {0}")]
    Spawn(String),
    #[error("agent 进程异常退出 (code={0:?})")]
    ProcessExit(Option<i32>),
    #[error("agent 运行超时 ({0}s)")]
    Timeout(u64),
    #[error("已取消")]
    Cancelled,
    #[error("IO 错误: {0}")]
    Io(String),
}

/// 给 GUI 进程补 PATH / HOME：macOS 从 Finder 启动的进程不继承登录 shell 环境，
/// `claude` 常装在 `~/.local/bin`、Homebrew 在 `/opt/homebrew/bin`。
fn augment_env(cmd: &mut Command) {
    let mut path = std::env::var("PATH").unwrap_or_default();
    if let Some(home_os) = std::env::var_os("HOME") {
        let home = home_os.to_string_lossy().to_string();
        let extras = [
            format!("{home}/.local/bin"),
            "/opt/homebrew/bin".to_string(),
            "/usr/local/bin".to_string(),
        ];
        for extra in extras {
            if !path.split(':').any(|p| p == extra) {
                path = if path.is_empty() {
                    extra
                } else {
                    format!("{extra}:{path}")
                };
            }
        }
        cmd.env("HOME", home);
    }
    cmd.env("PATH", path);
}

fn augmented_command(exe: &str) -> Command {
    let mut cmd = Command::new(exe);
    augment_env(&mut cmd);
    cmd
}

/// `git stash create`：生成一个表示当前工作区的提交对象，**不改动工作区、也不进 stash 列表**。
/// 返回该快照的 commit SHA，供出问题时 `git stash apply <sha>` 回滚。无改动时返回 `None`。
pub fn create_git_snapshot(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["stash", "create", "openless-agent-pre-run"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// 探测 `claude` 版本（`None` 表示未安装或无法运行）。
pub async fn detect_claude(exe: &str) -> Option<String> {
    let out = augmented_command(exe).arg("--version").output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    detect::parse_claude_version(&String::from_utf8_lossy(&out.stdout))
}

/// 列出 Claude Code 已配置的 MCP server（含健康状态）。
pub async fn claude_mcp_list(exe: &str) -> Vec<McpServerStatus> {
    match augmented_command(exe).args(["mcp", "list"]).output().await {
        Ok(out) => detect::parse_mcp_list(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => Vec::new(),
    }
}

async fn wait_cancel(cancel: &Arc<AtomicBool>) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// 无头跑一次 Claude：写 prompt 到 stdin，逐行解析 stream-json，把事件投到 `sink`。
/// 支持取消（`cancel` 置 true）与超时（`req.timeout_secs`），两者都会 kill 子进程。
pub async fn run_claude_agent(
    exe: &str,
    req: CodingAgentRequest,
    sink: CodingAgentEventSink,
    cancel: Arc<AtomicBool>,
) -> Result<(), CodingAgentError> {
    let args = build_claude_args(&req);
    let mut cmd = augmented_command(exe);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CodingAgentError::ExecutableNotFound(exe.to_string())
        } else {
            CodingAgentError::Spawn(e.to_string())
        }
    })?;

    // 写入 prompt 后立即关闭 stdin，触发 claude 开始处理。
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(req.prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    // 后台排空 stderr，避免管道写满导致子进程阻塞；出错时用作摘要。
    let stderr_task = child.stderr.take().map(|s| {
        tokio::spawn(async move {
            let mut buf = String::new();
            let _ = BufReader::new(s).read_to_string(&mut buf).await;
            buf
        })
    });

    let _ = sink.send(CodingAgentEvent::Started {
        session_id: req.session_id.clone(),
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CodingAgentError::Io("子进程无 stdout".into()))?;
    let mut lines = BufReader::new(stdout).lines();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(req.timeout_secs.max(1));
    let mut got_terminal = false;
    let mut outcome: Result<(), CodingAgentError> = Ok(());

    loop {
        tokio::select! {
            biased;
            _ = wait_cancel(&cancel) => {
                let _ = child.start_kill();
                let _ = sink.send(CodingAgentEvent::Cancelled { session_id: req.session_id.clone() });
                got_terminal = true;
                outcome = Err(CodingAgentError::Cancelled);
                break;
            }
            _ = tokio::time::sleep_until(deadline) => {
                let _ = child.start_kill();
                let _ = sink.send(CodingAgentEvent::Error {
                    session_id: req.session_id.clone(),
                    message: format!("运行超时（{}s）", req.timeout_secs),
                });
                got_terminal = true;
                outcome = Err(CodingAgentError::Timeout(req.timeout_secs));
                break;
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        if let Some(ev) = parse_stream_json_line(&req.session_id, &l) {
                            if matches!(ev, CodingAgentEvent::Completed { .. } | CodingAgentEvent::Error { .. }) {
                                got_terminal = true;
                            }
                            let _ = sink.send(ev);
                        }
                    }
                    Ok(None) => break, // EOF
                    Err(e) => {
                        outcome = Err(CodingAgentError::Io(e.to_string()));
                        break;
                    }
                }
            }
        }
    }

    let status = child.wait().await.map_err(|e| CodingAgentError::Io(e.to_string()))?;
    if !status.success() && outcome.is_ok() {
        // 进程非 0 退出且我们还没判终局：补一条 Error。
        if !got_terminal {
            let stderr = match stderr_task {
                Some(t) => t.await.unwrap_or_default(),
                None => String::new(),
            };
            let summary = stderr.lines().last().unwrap_or("").trim().to_string();
            let _ = sink.send(CodingAgentEvent::Error {
                session_id: req.session_id.clone(),
                message: if summary.is_empty() {
                    format!("agent 异常退出 (code={:?})", status.code())
                } else {
                    summary
                },
            });
        }
        return Err(CodingAgentError::ProcessExit(status.code()));
    }

    outcome
}
