//! Linux Coding Agent Adapter：只负责临时文件与子进程 I/O。

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
pub(crate) struct LinuxCodingAgentProcessAdapter;

struct TemporaryWorkspace(PathBuf);

impl Drop for TemporaryWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn materialize(
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

static LOGIN_SHELL_PATH: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();

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

async fn augment_path(command: &mut tokio::process::Command, cancel: &CancellationToken) -> bool {
    if cancel.is_cancelled() {
        return false;
    }
    let current = std::env::var_os("PATH").unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = &home {
        command.env("HOME", home);
    }
    // Finder/desktop-launched shells may need up to five seconds to discover
    // login PATH. Cancellation must still win during that lookup; otherwise a
    // request cancelled before spawn appears to hang and no child exists for
    // the normal process-group kill path to terminate.
    let login_path = tokio::select! {
        path = login_shell_path() => path,
        _ = async {
            while !cancel.is_cancelled() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        } => return false,
    };
    command.env(
        "PATH",
        openless_core::merge_agent_path(&current, home.as_deref(), login_path),
    );
    true
}

pub(crate) fn isolate_process_group(command: &mut tokio::process::Command) {
    #[cfg(target_os = "linux")]
    command.process_group(0);
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

pub(crate) fn kill_process_group(
    child: &mut tokio::process::Child,
) -> Result<(), openless_core::BackendError> {
    #[cfg(target_os = "linux")]
    if child.id().is_some_and(|pid| {
        // SAFETY: `isolate_process_group` starts the child as process-group leader.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) == 0 }
    }) {
        return Ok(());
    }
    child.start_kill().map_err(platform_error)
}

impl CodingAgentProcessAdapter for LinuxCodingAgentProcessAdapter {
    fn execute(
        &self,
        mut request: AgentCommand,
        output: Arc<dyn ProcessOutputSink>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<ProcessExit, openless_core::BackendError>> {
        Box::pin(async move {
            let _workspace = materialize(&mut request)?;
            let mut command = tokio::process::Command::new(&request.executable);
            if !augment_path(&mut command, &cancel).await {
                return Ok(ProcessExit {
                    code: None,
                    success: false,
                });
            }
            command
                .args(&request.argv)
                .envs(&request.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            isolate_process_group(&mut command);
            if let Some(cwd) = &request.cwd {
                command.current_dir(cwd);
            }
            if let PromptPayload::Argv(prompt) = &request.prompt {
                command.arg(prompt);
            }
            let mut child = command.spawn().map_err(|error| {
                openless_core::BackendError::new(
                    if error.kind() == std::io::ErrorKind::NotFound {
                        openless_core::BackendErrorCode::Unsupported
                    } else {
                        openless_core::BackendErrorCode::Platform
                    },
                    error.to_string(),
                )
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
                            kill_process_group(&mut child)?;
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use openless_core::{
        AgentCommand, CancellationToken, CodingAgentProcessAdapter, ProcessOutputLine,
        ProcessOutputSink, PromptPayload,
    };

    use super::LinuxCodingAgentProcessAdapter;

    struct IgnoreOutput;

    impl ProcessOutputSink for IgnoreOutput {
        fn write(&self, _line: ProcessOutputLine) {}
    }

    #[tokio::test]
    async fn cancellation_kills_a_running_child() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            flag.store(true, Ordering::Release);
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            LinuxCodingAgentProcessAdapter.execute(
                AgentCommand {
                    executable: "sh".into(),
                    argv: vec!["-c".into(), "sleep 10".into()],
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
        .expect("cancelled child must exit promptly")
        .unwrap();
        assert!(!result.success);
    }
}
