use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::*;

/// Less Computer 浮窗的 Tauri 事件名（前端 LessComputerPanel 订阅）。
const LESS_COMPUTER_EVENT: &str = "less-computer:event";

/// Less Computer 内联审批：等待用户决断的 token → oneshot sender 注册表。
///
/// 无头 `claude -p` 没有 mid-run 的 `--permission-prompt-tool` 通道（v2.1.165 不支持），
/// 所以护栏拦截发生在「整轮跑完、护栏 deny 生效」之后。这个注册表是审批 UI 的实回路：
/// 后端发 `approval` 事件后把一个 oneshot 接收端挂在这里，等前端 `less_computer_approve`
/// 命令按 token 解析出用户决断（true=Approve / false=Deny）。
static LESS_COMPUTER_APPROVALS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
> = std::sync::OnceLock::new();

fn less_computer_approvals(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>
{
    LESS_COMPUTER_APPROVALS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 前端 `less_computer_approve` 命令调到这里：按 token 解析等待中的审批。
/// token 不存在（已超时 / 已解析）时静默忽略。
pub(crate) fn resolve_less_computer_approval(token: &str, approved: bool) {
    let sender = less_computer_approvals()
        .lock()
        .ok()
        .and_then(|mut m| m.remove(token));
    if let Some(tx) = sender {
        let _ = tx.send(approved);
        log::info!("[less-computer] 审批 token={token} approved={approved}");
    } else {
        log::info!("[less-computer] 审批 token={token} 已失效（超时/重复）");
    }
}

/// 往 Less Computer 浮窗发一条事件（macOS only；前端按 `kind` 渲染聊天结构）。
fn emit_less_computer(inner: &Arc<Inner>, payload: serde_json::Value) {
    if let Some(app) = inner.app.lock().clone() {
        let _ = app.emit_to("less-computer", LESS_COMPUTER_EVENT, payload);
    }
}

/// Less Computer 收尾：把转写当作指令交给无头 Claude，结果以胶囊展示（不插入到光标）。
pub(crate) async fn run_voice_agent_transcript(
    inner: &Arc<Inner>,
    _session_id: SessionId,
    transcript: String,
    elapsed: u64,
) -> Result<(), String> {
    log::info!(
        "[coord] Cloud Agent 语音：指令 {} 字",
        transcript.chars().count()
    );
    // 胶囊保留「处理中」反馈（用户熟悉的小录音条状态机）；聊天浮窗承载完整对话。
    emit_capsule(
        inner,
        CapsuleState::Polishing,
        0.0,
        elapsed,
        Some("Claude 处理中…".to_string()),
        None,
    );

    // 聊天浮窗：显示窗口 + 落用户气泡（语音指令转写）。macOS only（helper 内部 gating）。
    if let Some(app) = inner.app.lock().clone() {
        crate::show_less_computer_window(&app);
        // 全屏彩虹描边已在按下键时（handle_less_computer_pressed）点亮，这里不重复。
    }
    // 连续对话：浮窗里已有进行中的会话 → 本轮 `claude --continue` 续上下文；否则是新会话（fresh）。
    // dismiss 关窗会把标志复位为 false。
    let continue_session = inner
        .less_computer_conversation
        .swap(true, Ordering::SeqCst);
    emit_less_computer(
        inner,
        serde_json::json!({ "kind": "user", "text": transcript, "fresh": !continue_session }),
    );

    let prefs = inner.prefs.get();
    // 工作目录：用户设的 workdir，否则 $HOME。--add-dir 把文件作用域限定在此。
    let cwd = prefs
        .coding_agent_workdir
        .clone()
        .filter(|d| !d.trim().is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(std::path::PathBuf::from));
    // 运行前 git 快照（cwd 是 git 仓库才有效；非仓库无副作用），便于回滚文件改动。
    if let Some(dir) = &cwd {
        if let Some(sha) = crate::coding_agent::create_git_snapshot(dir) {
            log::info!("[less-computer] 运行前 git 快照 {sha}（git stash apply 可回滚）");
        }
    }

    // 钳制：语音 → shell 这条全自动路径禁止 bypassPermissions 绕过护栏（无人审、动手即生效）。
    // 即便用户在偏好里设了 bypass，这里也降级为 acceptEdits（仍带 deny 护栏）。
    let mode = match coding_agent_mode_from_pref(&prefs.coding_agent_permission_mode) {
        crate::coding_agent::CodingAgentPermissionMode::BypassPermissions => {
            log::warn!(
                "[less-computer] 语音 Agent 路径禁止 bypassPermissions，已降级为 acceptEdits（保留护栏）"
            );
            crate::coding_agent::CodingAgentPermissionMode::AcceptEdits
        }
        other => other,
    };
    let model = prefs
        .coding_agent_model
        .clone()
        .filter(|m| !m.trim().is_empty())
        .or_else(|| Some("sonnet".to_string()));
    let prompt = crate::coding_agent::autonomous_prompt(&transcript);

    // 第一轮：默认护栏（高风险全 deny）。运行后若检测到护栏拦截，弹审批卡；
    // 用户 Approve 则在第二轮把该高风险模式从 deny 移除 + 加进 allowed，重跑一次。
    let outcome = run_less_computer_once(
        inner,
        &prompt,
        cwd.as_deref(),
        mode,
        model.as_deref(),
        &[],
        continue_session,
    )
    .await;

    let final_outcome = match maybe_request_approval(inner, &outcome).await {
        Some(approved_pattern) => {
            log::info!("[less-computer] 审批通过，放行高风险模式后重跑：{approved_pattern}");
            run_less_computer_once(
                inner,
                &prompt,
                cwd.as_deref(),
                mode,
                model.as_deref(),
                &[approved_pattern],
                continue_session,
            )
            .await
        }
        None => outcome,
    };

    inner.state.lock().phase = SessionPhase::Idle;
    // 工作结束：熄灭全屏彩虹描边（聊天浮窗保留，等用户读完/关闭）。
    if let Some(app) = inner.app.lock().clone() {
        crate::hide_less_computer_glow(&app);
    }

    match final_outcome {
        LessComputerOutcome::Done { text, cost_usd } => {
            let text = text.trim().to_string();
            if text.is_empty() {
                let msg = "Claude 无结果（确认已登录 claude 且额度充足）".to_string();
                emit_less_computer(
                    inner,
                    serde_json::json!({ "kind": "error", "message": msg }),
                );
                emit_capsule(inner, CapsuleState::Error, 0.0, elapsed, Some(msg), None);
                schedule_capsule_idle(inner, CAPSULE_AUTO_HIDE_DELAY_MS);
                return Err("voice agent empty".to_string());
            }
            log::info!("[coord] Cloud Agent 语音：返回 {} 字", text.chars().count());
            emit_less_computer(
                inner,
                serde_json::json!({ "kind": "completed", "text": text, "costUsd": cost_usd }),
            );
            emit_capsule(inner, CapsuleState::Done, 0.0, elapsed, Some(text), None);
            schedule_capsule_idle(inner, CAPSULE_AUTO_HIDE_DELAY_MS);
            Ok(())
        }
        LessComputerOutcome::Failed { message } => {
            log::warn!("[coord] Cloud Agent 语音失败: {message}");
            emit_less_computer(
                inner,
                serde_json::json!({ "kind": "error", "message": message }),
            );
            emit_capsule(
                inner,
                CapsuleState::Error,
                0.0,
                elapsed,
                Some(message),
                None,
            );
            schedule_capsule_idle(inner, CAPSULE_AUTO_HIDE_DELAY_MS);
            Err("voice agent failed".to_string())
        }
        LessComputerOutcome::Cancelled => {
            log::info!("[coord] Cloud Agent 语音已取消");
            emit_less_computer(inner, serde_json::json!({ "kind": "cancelled" }));
            emit_capsule(inner, CapsuleState::Cancelled, 0.0, elapsed, None, None);
            schedule_capsule_idle(inner, CAPSULE_AUTO_HIDE_DELAY_MS);
            Err("voice agent cancelled".to_string())
        }
    }
}

/// 一轮无头 Less Computer 运行的结果。
enum LessComputerOutcome {
    Done { text: String, cost_usd: Option<f64> },
    Failed { message: String },
    Cancelled,
}

/// 跑一轮无头 Claude（「放行 + 护栏」），把 Delta/ToolUse 实时 stream 到聊天浮窗，
/// 终局收敛为 [`LessComputerOutcome`]。`extra_allow_patterns` 为审批通过后放行的
/// 高风险子串（如 "git push --force"）：从 deny 清单剔除 + 作为 `Bash(<pat>:*)` 加进 allowed。
async fn run_less_computer_once(
    inner: &Arc<Inner>,
    prompt: &str,
    cwd: Option<&std::path::Path>,
    mode: crate::coding_agent::CodingAgentPermissionMode,
    model: Option<&str>,
    extra_allow_patterns: &[String],
    continue_session: bool,
) -> LessComputerOutcome {
    // 护栏 deny：默认全量；审批放行的模式从 deny 中剔除。
    // 审批 UI 只回传命中的单个高风险子串，但同一风险有等价写法（如 --force / -f）。
    // 按「风险等价组」整组放行：只放行被点那一个会让等价写法仍卡在 deny（deny 优先级高于
    // allow）→ 命令仍被拦。见 guard::risk_equivalent_patterns。
    let mut deny = crate::coding_agent::guard::default_deny_rules();
    let approved_patterns: Vec<String> = extra_allow_patterns
        .iter()
        .flat_map(|p| {
            let group = crate::coding_agent::guard::risk_equivalent_patterns(p);
            if group.is_empty() {
                vec![p.clone()]
            } else {
                group.into_iter().map(|s| s.to_string()).collect()
            }
        })
        .collect();
    let allow_rules: Vec<String> = approved_patterns
        .iter()
        .map(|p| format!("Bash({p}:*)"))
        .collect();
    if !allow_rules.is_empty() {
        deny.retain(|d| !allow_rules.iter().any(|a| a == d));
    }
    let settings_json = serde_json::json!({
        "permissions": { "defaultMode": mode.as_cli_arg(), "deny": deny }
    });
    let settings_path = std::env::temp_dir().join(format!(
        "openless-less-computer-guard-{}.json",
        uuid::Uuid::new_v4()
    ));
    // fail-closed：序列化或写入失败时立即中止，绝不在「无护栏」下把无效路径交给
    // `claude -p --settings`（找不到文件 = 完全裸跑）。宁可不跑也不裸跑。
    let settings_bytes = match serde_json::to_vec_pretty(&settings_json) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("[less-computer] 序列化护栏配置失败: {e}");
            return LessComputerOutcome::Failed {
                message: "护栏配置写入失败，已中止（拒绝在无护栏下执行）".into(),
            };
        }
    };
    if let Err(e) = std::fs::write(&settings_path, settings_bytes) {
        log::warn!("[less-computer] 写护栏配置失败: {e}");
        return LessComputerOutcome::Failed {
            message: "护栏配置写入失败，已中止（拒绝在无护栏下执行）".into(),
        };
    }

    let mut req = crate::coding_agent::CodingAgentRequest::new("less-computer", prompt.to_string());
    req.cwd = cwd.map(|p| p.to_path_buf());
    req.model = model.map(|m| m.to_string());
    req.permission_mode = mode;
    // 写护栏成功后才设置：写失败已在上面 fail-closed 返回，不会带无效路径裸跑。
    req.settings_json_path = Some(settings_path.clone());
    // 去掉 WebFetch：无出站白名单时它是 prompt 注入 SSRF 面（诱导拉取内网/元数据端点）。
    // 保留 WebSearch（走搜索引擎，不直接抓任意 URL）。
    req.allowed_tools = vec![
        "Bash".into(),
        "Read".into(),
        "Edit".into(),
        "Write".into(),
        "Glob".into(),
        "Grep".into(),
        "WebSearch".into(),
    ];
    req.allowed_tools.extend(allow_rules);
    // 真实任务（开应用、多步操作、读写文件）常超过 120s/0.5$ → 老是「运行超时」。放宽到
    // 5 分钟 / 2$，给多步任务足够空间；仍有硬上限兜底，不会无限跑/烧钱。
    req.max_budget_usd = Some(2.0);
    req.timeout_secs = 300;
    // 连续对话需要保留会话：本轮保存（供下轮 --continue），第二轮起带 --continue 续上下文。
    req.session_persistence = true;
    req.continue_session = continue_session;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_runner = Arc::clone(&cancel);
    let run = async_runtime::spawn(async move {
        crate::coding_agent::run_claude_agent("claude", req, tx, cancel_for_runner).await
    });
    let cancel_for_watcher = Arc::clone(&cancel);
    let inner_for_cancel = Arc::clone(inner);
    let cancel_watcher = async_runtime::spawn(async move {
        loop {
            if cancel_for_watcher.load(Ordering::Relaxed) {
                return;
            }
            if inner_for_cancel.state.lock().cancelled {
                cancel_for_watcher.store(true, Ordering::Relaxed);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        }
    });

    let mut final_text = String::new();
    let mut cost_usd: Option<f64> = None;
    let mut error_msg: Option<String> = None;
    let mut cancelled = false;
    while let Some(ev) = rx.recv().await {
        use crate::coding_agent::CodingAgentEvent as E;
        match ev {
            E::Started { .. } => {
                emit_less_computer(inner, serde_json::json!({ "kind": "started" }));
            }
            E::Delta { text, .. } => {
                emit_less_computer(inner, serde_json::json!({ "kind": "delta", "text": text }));
            }
            E::ToolUse { name, .. } => {
                emit_less_computer(inner, serde_json::json!({ "kind": "tool", "name": name }));
            }
            E::Completed {
                text, cost_usd: c, ..
            } => {
                final_text = text;
                cost_usd = c;
            }
            E::Error { message, .. } => error_msg = Some(message),
            E::Cancelled { .. } => cancelled = true,
        }
    }
    let run_result = run.await;
    cancel.store(true, Ordering::Relaxed);
    let _ = cancel_watcher.await;
    let _ = std::fs::remove_file(&settings_path);

    if cancelled
        || matches!(
            &run_result,
            Ok(Err(crate::coding_agent::CodingAgentError::Cancelled))
        )
    {
        return LessComputerOutcome::Cancelled;
    }

    let trimmed = final_text.trim().to_string();
    if !trimmed.is_empty() {
        LessComputerOutcome::Done {
            text: trimmed,
            cost_usd,
        }
    } else {
        let message = error_msg
            .or_else(|| match run_result {
                Ok(Err(e)) => Some(e.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "Claude 无结果（确认已登录 claude 且额度充足）".to_string());
        LessComputerOutcome::Failed { message }
    }
}

/// 护栏拦截探测 + 内联审批（best-effort）。
///
/// 无头 `claude -p`（v2.1.165）没有 mid-run 的 `--permission-prompt-tool` 通道，所以
/// 我们只能在「一轮跑完」后判断护栏是否拦了高风险动作：扫描终局文本里是否提到某个
/// 高风险模式 + 权限/拒绝/blocked 关键词。命中则发 `approval` 事件、挂一个 oneshot 等
/// 用户决断（前端 Approve/Deny → `less_computer_approve` 命令解析）。
///
/// 返回 `Some(pattern)` 表示用户 Approve 了某高风险模式 → 调用方应放行该模式重跑一轮；
/// `None` 表示无需审批 / 用户 Deny / 超时。**注意**这是「重跑放行」而非真正的 mid-run
/// 续跑——headless 下没有干净的 mid-run round-trip，详见 report。
async fn maybe_request_approval(
    inner: &Arc<Inner>,
    outcome: &LessComputerOutcome,
) -> Option<String> {
    let text = match outcome {
        LessComputerOutcome::Done { text, .. } => text.as_str(),
        LessComputerOutcome::Failed { message } => message.as_str(),
        LessComputerOutcome::Cancelled => return None,
    };
    let lowered = text.to_lowercase();
    // 必须同时出现「拒绝/权限/blocked」语义 + 某个已知高风险模式，才认为是护栏拦截，
    // 避免把正常提到 "rm" 的回答误判成审批请求。
    let mentions_block = [
        "denied",
        "permission",
        "not allowed",
        "blocked",
        "拒绝",
        "权限",
        "被拦",
    ]
    .iter()
    .any(|kw| lowered.contains(kw));
    if !mentions_block {
        return None;
    }
    let hit = crate::coding_agent::guard::HIGH_RISK_PATTERNS
        .iter()
        .find(|(pat, _)| lowered.contains(*pat))?;
    let (pattern, reason) = (hit.0.to_string(), hit.1.to_string());

    // 挂 oneshot 等用户决断。
    let token = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    if let Ok(mut map) = less_computer_approvals().lock() {
        map.insert(token.clone(), tx);
    }
    emit_less_computer(
        inner,
        serde_json::json!({
            "kind": "approval",
            "token": token,
            "command": pattern,
            "reason": reason,
        }),
    );

    // 等用户点 Approve/Deny；90s 无响应按 Deny 处理并清理注册表项。
    let approved = match tokio::time::timeout(std::time::Duration::from_secs(90), rx).await {
        Ok(Ok(v)) => v,
        _ => {
            less_computer_approvals()
                .lock()
                .ok()
                .map(|mut m| m.remove(&token));
            false
        }
    };
    if approved {
        Some(pattern)
    } else {
        None
    }
}

/// 把 prefs 里的权限模式字符串映射成枚举；未知值回落到 acceptEdits（放行+护栏的默认）。
fn coding_agent_mode_from_pref(s: &str) -> crate::coding_agent::CodingAgentPermissionMode {
    use crate::coding_agent::CodingAgentPermissionMode as M;
    match s.trim() {
        "plan" => M::Plan,
        "default" => M::Default,
        "bypassPermissions" => M::BypassPermissions,
        _ => M::AcceptEdits,
    }
}
