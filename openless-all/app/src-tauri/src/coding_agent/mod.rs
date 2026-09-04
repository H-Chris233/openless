//! Tauri Coding Agent Adapter：只负责临时文件与子进程 I/O。

pub mod commands;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use openless_core::{
    AgentCommand, AgentMaterializationPlan, CancellationToken, CodingAgentProcessAdapter,
    ProcessExit, ProcessOutputLine, ProcessOutputSink, ProcessStream, PromptPayload,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Default)]
pub struct TauriCodingAgentProcessAdapter;

struct TemporaryWorkspace(PathBuf);

impl Drop for TemporaryWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn materialize_temporary_files(
    command: &mut AgentCommand,
) -> Result<Option<TemporaryWorkspace>, openless_core::BackendError> {
    if command.temporary_files.is_empty() {
        return Ok(None);
    }
    let directory =
        std::env::temp_dir().join(format!("openless-agent-{}", uuid::Uuid::new_v4().simple()));
    let plan = AgentMaterializationPlan::new(command, &directory)?;
    std::fs::create_dir(&directory).map_err(platform_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(platform_error)?;
    }
    let workspace = TemporaryWorkspace(directory.clone());
    for file in plan.files {
        std::fs::write(file.path, file.contents).map_err(platform_error)?;
    }
    command.argv = plan.argv;
    Ok(Some(workspace))
}

#[cfg(unix)]
static LOGIN_SHELL_PATH: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();

#[cfg(unix)]
async fn login_shell_path() -> Option<&'static str> {
    LOGIN_SHELL_PATH
        .get_or_init(|| async {
            let plan = openless_core::AgentLoginShellPathPlan::new(std::env::var("SHELL").ok())?;
            let deadline = tokio::time::Instant::now() + plan.timeout;
            for arguments in &plan.attempts {
                let mut command = tokio::process::Command::new(&plan.shell);
                command
                    .args(arguments)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                let Ok(Ok(output)) = tokio::time::timeout_at(deadline, command.output()).await
                else {
                    continue;
                };
                if output.status.success() {
                    if let Some(path) = openless_core::parse_agent_login_shell_path(
                        &String::from_utf8_lossy(&output.stdout),
                    ) {
                        return Some(path);
                    }
                }
            }
            None
        })
        .await
        .as_deref()
}

async fn augment_path(_command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        let command = _command;
        let current = std::env::var_os("PATH").unwrap_or_default();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = &home {
            command.env("HOME", home);
        }
        command.env(
            "PATH",
            openless_core::merge_agent_path(&current, home.as_deref(), login_shell_path().await),
        );
    }
}

impl CodingAgentProcessAdapter for TauriCodingAgentProcessAdapter {
    fn execute(
        &self,
        mut request: AgentCommand,
        output: Arc<dyn ProcessOutputSink>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<ProcessExit, openless_core::BackendError>> {
        Box::pin(async move {
            let _workspace = materialize_temporary_files(&mut request)?;
            let mut command = tokio::process::Command::new(&request.executable);
            augment_path(&mut command).await;
            command
                .args(&request.argv)
                .envs(&request.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(unix)]
            command.process_group(0);
            if let Some(cwd) = &request.cwd {
                command.current_dir(cwd);
            }
            if let PromptPayload::Argv(prompt) = &request.prompt {
                command.arg(prompt);
            }
            let mut child = command.spawn().map_err(|error| {
                let code = if error.kind() == std::io::ErrorKind::NotFound {
                    openless_core::BackendErrorCode::Unsupported
                } else {
                    openless_core::BackendErrorCode::Platform
                };
                openless_core::BackendError::new(code, error.to_string())
            })?;
            if let PromptPayload::Stdin(prompt) = &request.prompt {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin
                        .write_all(prompt.as_bytes())
                        .await
                        .map_err(platform_error)?;
                    stdin.shutdown().await.map_err(platform_error)?;
                }
            } else {
                drop(child.stdin.take());
            }
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| invalid("missing process stdout"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| invalid("missing process stderr"))?;
            let stdout_sink = Arc::clone(&output);
            let stdout_task = tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    stdout_sink.write(ProcessOutputLine {
                        stream: ProcessStream::Stdout,
                        line,
                    });
                }
            });
            let stderr_task = tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    output.write(ProcessOutputLine {
                        stream: ProcessStream::Stderr,
                        line,
                    });
                }
            });
            let status = loop {
                tokio::select! {
                    status = child.wait() => break status.map_err(platform_error)?,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                        if cancel.is_cancelled() {
                            #[cfg(unix)]
                            let killed_group = child.id().is_some_and(|pid| {
                                // SAFETY: the child was started as process-group leader above.
                                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) == 0 }
                            });
                            #[cfg(windows)]
                            let killed_group = if let Some(pid) = child.id() {
                                let mut taskkill = tokio::process::Command::new("taskkill");
                                taskkill
                                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                                    .stdout(Stdio::null())
                                    .stderr(Stdio::null());
                                taskkill.creation_flags(0x08000000);
                                taskkill.status().await.is_ok_and(|status| status.success())
                            } else {
                                false
                            };
                            #[cfg(not(any(unix, windows)))]
                            let killed_group = false;
                            if !killed_group {
                                child.start_kill().map_err(platform_error)?;
                            }
                            break child.wait().await.map_err(platform_error)?;
                        }
                    }
                }
            };
            let _ = tokio::join!(stdout_task, stderr_task);
            Ok(ProcessExit {
                code: status.code(),
                success: status.success() && !cancel.is_cancelled(),
            })
        })
    }
}

fn invalid(message: impl Into<String>) -> openless_core::BackendError {
    openless_core::BackendError::new(openless_core::BackendErrorCode::InvalidArgument, message)
}

fn platform_error(error: impl std::fmt::Display) -> openless_core::BackendError {
    openless_core::BackendError::new(openless_core::BackendErrorCode::Platform, error.to_string())
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    struct IgnoreOutput;

    impl ProcessOutputSink for IgnoreOutput {
        fn write(&self, _line: ProcessOutputLine) {}
    }

    #[tokio::test]
    async fn cancellation_kills_a_windows_process_tree() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            flag.store(true, Ordering::Release);
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            TauriCodingAgentProcessAdapter.execute(
                AgentCommand {
                    executable: "cmd.exe".into(),
                    argv: vec!["/C".into(), "ping -n 11 127.0.0.1 >nul".into()],
                    env: BTreeMap::new(),
                    cwd: None,
                    prompt: PromptPayload::Stdin(String::new()),
                    temporary_files: Vec::new(),
                },
                Arc::new(IgnoreOutput),
                CancellationToken::from_flag(cancelled),
            ),
        )
        .await
        .expect("cancelled Windows process tree must exit promptly")
        .unwrap();
        assert!(!result.success);
    }
}
