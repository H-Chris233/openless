//! 流式插入的纯策略与 Unicode 边界规则。

use crate::shared_types::ChineseScriptPreference;

pub const STREAMING_FLUSH_INTERVAL_MS: u64 = 12;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamingInsertState {
    pub pending: String,
    pub typed_text: String,
    pub failed: Option<String>,
    accepted_chars: u64,
}

impl StreamingInsertState {
    pub fn push_delta(&mut self, offset: u64, delta: &str) {
        if self.failed.is_some() || delta.is_empty() {
            return;
        }
        if offset > self.accepted_chars {
            self.failed = Some(format!(
                "polish delta skipped from {} to {offset}",
                self.accepted_chars
            ));
            return;
        }
        let overlap = self.accepted_chars.saturating_sub(offset) as usize;
        let suffix = delta.chars().skip(overlap).collect::<String>();
        self.accepted_chars = self
            .accepted_chars
            .saturating_add(suffix.chars().count() as u64);
        self.pending.push_str(&suffix);
    }

    /// Flushes pending text through the host inserter. A partial Unicode write
    /// is retained as a typed prefix and becomes an explicit fallback.
    pub fn flush<F>(&mut self, mut insert: F) -> Result<usize, String>
    where
        F: FnMut(&str) -> Result<usize, String>,
    {
        if self.failed.is_some() || self.pending.is_empty() {
            return Ok(0);
        }
        let delta = std::mem::take(&mut self.pending);
        let expected = delta.chars().count();
        match insert(&delta) {
            Ok(typed) if typed >= expected => {
                self.typed_text.push_str(&delta);
                Ok(expected)
            }
            Ok(typed) => {
                let appended = append_typed_prefix(&mut self.typed_text, &delta, typed);
                self.failed = Some(format!(
                    "host inserted only {appended}/{expected} characters"
                ));
                Ok(appended)
            }
            Err(error) => {
                self.failed = Some(error.clone());
                Err(error)
            }
        }
    }
}

pub fn apply_chinese_script_preference(text: &str, preference: ChineseScriptPreference) -> String {
    use ferrous_opencc::config::BuiltinConfig;

    let config = match preference {
        ChineseScriptPreference::Simplified => Some(BuiltinConfig::T2s),
        ChineseScriptPreference::Traditional => Some(BuiltinConfig::S2t),
        ChineseScriptPreference::Auto => None,
    };
    config
        .and_then(|config| ferrous_opencc::OpenCC::from_config(config).ok())
        .map_or_else(|| text.to_string(), |converter| converter.convert(text))
}

pub fn append_typed_prefix(target: &mut String, delta: &str, typed_chars: usize) -> usize {
    let prefix: String = delta.chars().take(typed_chars).collect();
    let count = prefix.chars().count();
    target.push_str(&prefix);
    count
}

pub fn streaming_insert_eligible(
    enabled: bool,
    translation_active: bool,
    traditional_script: bool,
    windows_paste_insertion: bool,
) -> bool {
    enabled && !translation_active && !traditional_script && !windows_paste_insertion
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_unicode_write_is_explicit_and_prefix_safe() {
        let mut state = StreamingInsertState::default();
        state.push_delta(0, "你好🙂");
        let written = state.flush(|_| Ok(2)).unwrap();
        assert_eq!(written, 2);
        assert_eq!(state.typed_text, "你好");
        assert!(state.failed.is_some());
    }

    #[test]
    fn duplicate_and_out_of_order_deltas_are_not_typed_twice() {
        let mut state = StreamingInsertState::default();
        state.push_delta(0, "你好");
        state.push_delta(0, "你好");
        state.push_delta(3, "跳");
        assert_eq!(state.pending, "你好");
        assert!(state.failed.is_some());
    }

    #[test]
    fn policy_blocks_only_unsafe_modes() {
        assert!(streaming_insert_eligible(true, false, false, false));
        assert!(!streaming_insert_eligible(true, true, false, false));
        assert!(!streaming_insert_eligible(true, false, true, false));
    }
}
