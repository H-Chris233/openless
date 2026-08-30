//! Vocabulary entries, hit accounting, and preset persistence.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;

use crate::errors::{BackendError, BackendErrorCode};
use crate::persistence::{atomic_write, persistence_error, read_or_default};
use crate::types::{DictionaryEntry, VocabPresetStore};

pub struct DictionaryStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl DictionaryStore {
    pub fn at_data_dir(data_dir: impl AsRef<Path>) -> Self {
        Self::at_path(data_dir.as_ref().join("dictionary.json"))
    }

    pub fn at_path(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn list(&self) -> Result<Vec<DictionaryEntry>, BackendError> {
        let _guard = self.lock_store()?;
        read_or_default(&self.path)
    }

    /// Manual entries are intentionally inserted at the front.
    pub fn add(
        &self,
        phrase: String,
        note: Option<String>,
    ) -> Result<DictionaryEntry, BackendError> {
        let _guard = self.lock_store()?;
        let mut entries = self.read_locked()?;
        let entry = new_entry(phrase, note);
        entries.insert(0, entry.clone());
        self.write_locked(&entries)?;
        Ok(entry)
    }

    /// Learned entries are deduplicated and appended behind manual entries.
    pub fn add_if_absent(
        &self,
        phrase: String,
        note: Option<String>,
    ) -> Result<Option<DictionaryEntry>, BackendError> {
        let phrase = phrase.trim().to_string();
        if phrase.is_empty() {
            return Ok(None);
        }
        let _guard = self.lock_store()?;
        let mut entries = self.read_locked()?;
        if entries.iter().any(|entry| entry.phrase == phrase) {
            return Ok(None);
        }
        let entry = new_entry(phrase, note);
        entries.push(entry.clone());
        self.write_locked(&entries)?;
        Ok(Some(entry))
    }

    pub fn remove(&self, id: &str) -> Result<(), BackendError> {
        let _guard = self.lock_store()?;
        let mut entries = self.read_locked()?;
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() != before {
            self.write_locked(&entries)?;
        }
        Ok(())
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), BackendError> {
        let _guard = self.lock_store()?;
        let mut entries = self.read_locked()?;
        let entry = entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .ok_or_else(|| {
                BackendError::new(
                    BackendErrorCode::InvalidArgument,
                    "dictionary entry not found",
                )
            })?;
        if entry.enabled != enabled {
            entry.enabled = enabled;
            self.write_locked(&entries)?;
        }
        Ok(())
    }

    /// Count case-insensitive, non-overlapping occurrences in final output.
    pub fn record_hits(&self, text: &str) -> Result<u64, BackendError> {
        if text.is_empty() {
            return Ok(0);
        }
        let _guard = self.lock_store()?;
        let mut entries = self.read_locked()?;
        let haystack = text.to_lowercase();
        let mut total = 0_u64;
        let mut changed = false;
        for entry in entries.iter_mut().filter(|entry| entry.enabled) {
            let needle = entry.phrase.trim().to_lowercase();
            let count = count_occurrences(&haystack, &needle);
            if count > 0 {
                entry.hits = entry.hits.saturating_add(count);
                total = total.saturating_add(count);
                changed = true;
            }
        }
        if changed {
            self.write_locked(&entries)?;
        }
        Ok(total)
    }

    fn lock_store(&self) -> Result<std::sync::MutexGuard<'_, ()>, BackendError> {
        self.lock.lock().map_err(|_| {
            BackendError::new(BackendErrorCode::Internal, "dictionary store lock poisoned")
        })
    }

    fn read_locked(&self) -> Result<Vec<DictionaryEntry>, BackendError> {
        read_or_default(&self.path)
    }

    fn write_locked(&self, entries: &[DictionaryEntry]) -> Result<(), BackendError> {
        let json = serde_json::to_vec_pretty(entries)
            .map_err(|_| persistence_error("encode dictionary entries"))?;
        atomic_write(&self.path, &json)
    }
}

fn new_entry(phrase: String, note: Option<String>) -> DictionaryEntry {
    DictionaryEntry {
        id: uuid::Uuid::new_v4().to_string(),
        phrase,
        note,
        enabled: true,
        hits: 0,
        created_at: Utc::now().to_rfc3339(),
    }
}

fn count_occurrences(haystack: &str, needle: &str) -> u64 {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let mut count = 0_u64;
    let mut start = 0_usize;
    while let Some(position) = haystack[start..].find(needle) {
        count = count.saturating_add(1);
        start += position + needle.len();
        if start >= haystack.len() {
            break;
        }
    }
    count
}

pub fn list_vocab_presets(data_dir: &Path) -> Result<VocabPresetStore, BackendError> {
    read_or_default(&data_dir.join("vocab-presets.json"))
}

pub fn save_vocab_presets(data_dir: &Path, store: &VocabPresetStore) -> Result<(), BackendError> {
    let json = serde_json::to_vec_pretty(store)
        .map_err(|_| persistence_error("encode vocabulary presets"))?;
    atomic_write(&data_dir.join("vocab-presets.json"), &json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::VocabPreset;

    fn temp_store() -> (DictionaryStore, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "openless-core-vocab-{}.json",
            uuid::Uuid::new_v4().simple()
        ));
        (DictionaryStore::at_path(path.clone()), path)
    }

    #[test]
    fn manual_entries_lead_learned_entries_and_learning_deduplicates() {
        let (store, path) = temp_store();
        store.add("手动一".into(), None).unwrap();
        assert!(store
            .add_if_absent("学来的".into(), Some("自动收集".into()))
            .unwrap()
            .is_some());
        assert!(store
            .add_if_absent("学来的".into(), None)
            .unwrap()
            .is_none());
        store.add("手动二".into(), None).unwrap();
        let phrases = store
            .list()
            .unwrap()
            .into_iter()
            .map(|entry| entry.phrase)
            .collect::<Vec<_>>();
        assert_eq!(phrases, vec!["手动二", "手动一", "学来的"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn records_hits_only_for_enabled_entries() {
        let (store, path) = temp_store();
        let enabled = store.add("Codex".into(), None).unwrap();
        let disabled = store.add("Rust".into(), None).unwrap();
        store.set_enabled(&disabled.id, false).unwrap();
        assert_eq!(store.record_hits("codex CODEX Rust").unwrap(), 2);
        let entries = store.list().unwrap();
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id == enabled.id)
                .unwrap()
                .hits,
            2
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.id == disabled.id)
                .unwrap()
                .hits,
            0
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn vocabulary_presets_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "openless-core-vocab-presets-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = VocabPresetStore {
            custom: vec![VocabPreset {
                id: "test".into(),
                name: "测试".into(),
                phrases: vec!["PR".into(), "CI".into()],
            }],
            overrides: vec![],
            disabled_builtin_preset_ids: vec!["chef".into()],
        };
        save_vocab_presets(&dir, &store).unwrap();
        assert_eq!(list_vocab_presets(&dir).unwrap(), store);
        let _ = std::fs::remove_dir_all(dir);
    }
}
