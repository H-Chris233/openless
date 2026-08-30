//! Tauri compatibility re-exports for the shared Coding Agent guard policy.

pub use openless_core::{
    build_guard_settings_json, build_opencode_guard_config, default_deny_rules,
    deny_rule_for_pattern, is_high_risk_command, risk_equivalent_patterns, HIGH_RISK_PATTERNS,
};
