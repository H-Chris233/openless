//! 流式插入的纯策略与 Unicode 边界规则。

pub const STREAMING_FLUSH_INTERVAL_MS: u64 = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingInsertState {
    pub pending: String,
    pub typed_text: String,
    pub failed: Option<String>,
}

impl Default for StreamingInsertState {
    fn default() -> Self {
        Self {
            pending: String::new(),
            typed_text: String::new(),
            failed: None,
        }
    }
}

impl StreamingInsertState {
    pub fn push_delta(&mut self, delta: &str) {
        if self.failed.is_none() {
            self.pending.push_str(delta);
        }
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
        state.push_delta("你好🙂");
        let written = state.flush(|_| Ok(2)).unwrap();
        assert_eq!(written, 2);
        assert_eq!(state.typed_text, "你好");
        assert!(state.failed.is_some());
    }

    #[test]
    fn policy_blocks_only_unsafe_modes() {
        assert!(streaming_insert_eligible(true, false, false, false));
        assert!(!streaming_insert_eligible(true, true, false, false));
        assert!(!streaming_insert_eligible(true, false, true, false));
    }
}
