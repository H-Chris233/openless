//! Tauri Coding Agent Adapter：只负责临时文件与子进程 I/O。

pub mod commands;

use std::collections::BTreeMap;
use std::path::{Component, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use openless_core::{
    AgentCommand, CancellationToken, CodingAgentProcessAdapter, ProcessExit, ProcessOutputLine,
    ProcessOutputSink, ProcessStream, PromptPayload,
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
    std::fs::create_dir(&directory).map_err(platform_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(platform_error)?;
    }
    let workspace = TemporaryWorkspace(directory.clone());
    let mut paths = BTreeMap::new();
    for file in &command.temporary_files {
        let path = PathBuf::from(&file.name);
        if file.name.is_empty()
            || path.components().count() != 1
            || !matches!(path.components().next(), Some(Component::Normal(_)))
        {
            return Err(invalid("invalid temporary file name"));
        }
        paths.insert(file.name.clone(), directory.join(&file.name));
    }
    for file in &command.temporary_files {
        let text = std::str::from_utf8(&file.contents)
            .map_err(|_| invalid("temporary file contents must be UTF-8"))?;
        let contents = replace_path_tokens(text, &paths)?;
        std::fs::write(
            paths.get(&file.name).expect("validated temporary path"),
            contents,
        )
        .map_err(platform_error)?;
    }
    for argument in &mut command.argv {
        *argument = replace_path_tokens(argument, &paths)?;
    }
    Ok(Some(workspace))
}

fn replace_path_tokens(
    input: &str,
    paths: &BTreeMap<String, PathBuf>,
) -> Result<String, openless_core::BackendError> {
    let mut output = input.to_string();
    for (name, path) in paths {
        let value = path.to_string_lossy();
        output = output.replace(&openless_core::temporary_path_token(name), &value);
        let encoded =
            serde_json::to_string(value.as_ref()).map_err(|error| invalid(error.to_string()))?;
        output = output.replace(
            &openless_core::temporary_json_path_token(name),
            encoded.trim_matches('"'),
        );
    }
    Ok(output)
}

async fn augment_path(_command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        let command = _command;
        let current = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = std::env::var_os("HOME")
            .map(PathBuf::from)
            .into_iter()
            .flat_map(|home| {
                [
                    home.join(".local/bin"),
                    home.join(".opencode/bin"),
                    home.join(".npm-global/bin"),
                    home.join(".bun/bin"),
                ]
            })
            .collect::<Vec<_>>();
        paths.extend(std::env::split_paths(&current));
        if let Ok(path) = std::env::join_paths(paths) {
            command.env("PATH", path);
        }
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
                            #[cfg(not(unix))]
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
