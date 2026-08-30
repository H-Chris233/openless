//! CLI/MCP 输出的纯解析规则位于 `openless-core`；本模块只保留兼容重导出和宿主实机测试。

pub use openless_core::coding_agent::{parse_cli_version, parse_mcp_list, McpServerStatus};

#[cfg(test)]
mod live {
    use super::parse_cli_version;

    #[test]
    #[ignore = "要本机装了对应 CLI"]
    fn every_installed_cli_version_parses() {
        for executable in ["claude", "opencode", "codex", "dsh"] {
            let output = std::process::Command::new(executable)
                .arg("--version")
                .output();
            let Ok(output) = output else {
                println!("[skip] {executable} 未安装");
                continue;
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            let parsed = parse_cli_version(&stdout);
            println!("{executable:>9}: {:?} → {parsed:?}", stdout.trim());
            assert!(
                parsed.is_some(),
                "{executable} 的版本号解析不出来；原始输出: {stdout:?}"
            );
        }
    }
}
