use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use openless_core::{
    BackendError, BackendErrorCode, InsertOutcome, SelectionCapture, SelectionRuntimeAdapter,
    SessionId,
};

trait LinuxSelectionBridge: Send + Sync + 'static {
    fn selection_text(&self) -> Result<String, BackendError>;
    fn commit_text(&self, text: &str) -> Result<(), BackendError>;
}

struct Fcitx5SelectionBridge;

impl LinuxSelectionBridge for Fcitx5SelectionBridge {
    fn selection_text(&self) -> Result<String, BackendError> {
        crate::fcitx5::selection_text()
    }

    fn commit_text(&self, text: &str) -> Result<(), BackendError> {
        crate::fcitx5::commit_text(text)
    }
}

#[derive(Clone)]
pub struct LinuxSelectionRuntime {
    bridge: Arc<dyn LinuxSelectionBridge>,
    target: Arc<Mutex<Option<(SessionId, String)>>>,
}

impl Default for LinuxSelectionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxSelectionRuntime {
    pub fn new() -> Self {
        Self::with_bridge(Arc::new(Fcitx5SelectionBridge))
    }

    fn with_bridge(bridge: Arc<dyn LinuxSelectionBridge>) -> Self {
        Self {
            bridge,
            target: Arc::new(Mutex::new(None)),
        }
    }
}

impl SelectionRuntimeAdapter for LinuxSelectionRuntime {
    fn capture(
        &self,
        session_id: SessionId,
        supplied_text: Option<String>,
    ) -> BoxFuture<'static, Result<SelectionCapture, BackendError>> {
        let bridge = Arc::clone(&self.bridge);
        let target = Arc::clone(&self.target);
        Box::pin(async move {
            if supplied_text.is_some() {
                return Err(BackendError::new(
                    BackendErrorCode::Unsupported,
                    "Linux selection replacement requires a live fcitx5 target",
                ));
            }
            tokio::task::spawn_blocking(move || {
                let text = bridge.selection_text()?;
                let mut target = target.lock().expect("Linux selection target lock poisoned");
                if target
                    .as_ref()
                    .is_some_and(|(active_session, _)| *active_session == session_id)
                {
                    return Err(BackendError::new(
                        BackendErrorCode::Busy,
                        "the Linux selection session is already captured",
                    ));
                }
                *target = Some((session_id, text.clone()));
                Ok(SelectionCapture {
                    text,
                    source_app: None,
                })
            })
            .await
            .map_err(|error| {
                BackendError::new(
                    BackendErrorCode::Platform,
                    format!("Linux selection capture task failed: {error}"),
                )
            })?
        })
    }

    fn apply(
        &self,
        session_id: SessionId,
        source_text: String,
        replacement_text: String,
    ) -> BoxFuture<'static, Result<InsertOutcome, BackendError>> {
        let bridge = Arc::clone(&self.bridge);
        let target = Arc::clone(&self.target);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let selected_text = bridge.selection_text()?;
                let mut target = target.lock().expect("Linux selection target lock poisoned");
                let Some((active_session, captured_text)) = target.as_ref() else {
                    return Err(BackendError::new(
                        BackendErrorCode::Cancelled,
                        "the Linux selection target is no longer active",
                    ));
                };
                if *active_session != session_id
                    || captured_text != &source_text
                    || selected_text != source_text
                {
                    return Err(BackendError::new(
                        BackendErrorCode::Cancelled,
                        "the Linux selection changed before replacement",
                    ));
                }
                bridge.commit_text(&replacement_text)?;
                *target = None;
                Ok(InsertOutcome::Inserted)
            })
            .await
            .map_err(|error| {
                BackendError::new(
                    BackendErrorCode::Platform,
                    format!("Linux selection apply task failed: {error}"),
                )
            })?
        })
    }

    fn prepare_preview(
        &self,
        _session_id: SessionId,
    ) -> BoxFuture<'static, Result<(), BackendError>> {
        Box::pin(async {
            Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "Linux selection preview cannot safely retain an fcitx5 target",
            ))
        })
    }

    fn revert(
        &self,
        _session_id: SessionId,
    ) -> BoxFuture<'static, Result<InsertOutcome, BackendError>> {
        Box::pin(async {
            Err(BackendError::new(
                BackendErrorCode::Unsupported,
                "Linux selection replacement cannot be safely reverted",
            ))
        })
    }

    fn cancel(&self, session_id: SessionId) -> BoxFuture<'static, Result<(), BackendError>> {
        let target = Arc::clone(&self.target);
        Box::pin(async move {
            let mut target = target.lock().expect("Linux selection target lock poisoned");
            if target
                .as_ref()
                .is_some_and(|(active_session, _)| *active_session == session_id)
            {
                *target = None;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct TestSelectionBridge {
        selected_text: Mutex<String>,
        committed: Mutex<Vec<String>>,
    }

    impl LinuxSelectionBridge for TestSelectionBridge {
        fn selection_text(&self) -> Result<String, openless_core::BackendError> {
            Ok(self.selected_text.lock().unwrap().clone())
        }

        fn commit_text(&self, text: &str) -> Result<(), openless_core::BackendError> {
            self.committed.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn direct_apply_revalidates_the_selection_before_committing() {
        let bridge = Arc::new(TestSelectionBridge {
            selected_text: Mutex::new("source".to_string()),
            committed: Mutex::new(Vec::new()),
        });
        let runtime = LinuxSelectionRuntime::with_bridge(bridge.clone());
        let session_id = SessionId::new();
        runtime.capture(session_id, None).await.unwrap();

        let outcome = runtime
            .apply(session_id, "source".to_string(), "replacement".to_string())
            .await
            .unwrap();

        assert_eq!(outcome, InsertOutcome::Inserted);
        assert_eq!(
            bridge.committed.lock().unwrap().as_slice(),
            &["replacement"]
        );
    }

    #[tokio::test]
    async fn changed_selection_is_rejected_without_committing() {
        let bridge = Arc::new(TestSelectionBridge {
            selected_text: Mutex::new("source".to_string()),
            committed: Mutex::new(Vec::new()),
        });
        let runtime = LinuxSelectionRuntime::with_bridge(bridge.clone());
        let session_id = SessionId::new();
        runtime.capture(session_id, None).await.unwrap();
        *bridge.selected_text.lock().unwrap() = "changed".to_string();

        let error = runtime
            .apply(session_id, "source".to_string(), "replacement".to_string())
            .await
            .expect_err("a changed selection must not be replaced");

        assert_eq!(error.code, BackendErrorCode::Cancelled);
        assert!(bridge.committed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelled_selection_is_rejected_without_committing() {
        let bridge = Arc::new(TestSelectionBridge {
            selected_text: Mutex::new("source".to_string()),
            committed: Mutex::new(Vec::new()),
        });
        let runtime = LinuxSelectionRuntime::with_bridge(bridge.clone());
        let session_id = SessionId::new();
        runtime.capture(session_id, None).await.unwrap();
        runtime.cancel(session_id).await.unwrap();

        let error = runtime
            .apply(session_id, "source".to_string(), "replacement".to_string())
            .await
            .expect_err("a cancelled selection must not be replaced");

        assert_eq!(error.code, BackendErrorCode::Cancelled);
        assert!(bridge.committed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_new_capture_invalidates_the_previous_session_target() {
        let bridge = Arc::new(TestSelectionBridge {
            selected_text: Mutex::new("first".to_string()),
            committed: Mutex::new(Vec::new()),
        });
        let runtime = LinuxSelectionRuntime::with_bridge(bridge.clone());
        let first = SessionId::new();
        let second = SessionId::new();
        runtime.capture(first, None).await.unwrap();
        *bridge.selected_text.lock().unwrap() = "second".to_string();
        runtime.capture(second, None).await.unwrap();

        let error = runtime
            .apply(first, "first".to_string(), "replacement".to_string())
            .await
            .expect_err("the previous session must lose target ownership");

        assert_eq!(error.code, BackendErrorCode::Cancelled);
        assert!(bridge.committed.lock().unwrap().is_empty());
        runtime
            .apply(second, "second".to_string(), "replacement".to_string())
            .await
            .expect("stale apply must not discard the new session target");
        assert_eq!(
            bridge.committed.lock().unwrap().as_slice(),
            &["replacement"]
        );
    }

    #[tokio::test]
    async fn duplicate_capture_is_busy_and_preserves_the_original_target() {
        let bridge = Arc::new(TestSelectionBridge {
            selected_text: Mutex::new("original".to_string()),
            committed: Mutex::new(Vec::new()),
        });
        let runtime = LinuxSelectionRuntime::with_bridge(bridge.clone());
        let session_id = SessionId::new();
        runtime.capture(session_id, None).await.unwrap();
        *bridge.selected_text.lock().unwrap() = "changed".to_string();

        let error = runtime
            .capture(session_id, None)
            .await
            .expect_err("duplicate capture must be rejected");

        assert_eq!(error.code, BackendErrorCode::Busy);
        *bridge.selected_text.lock().unwrap() = "original".to_string();
        runtime
            .apply(
                session_id,
                "original".to_string(),
                "replacement".to_string(),
            )
            .await
            .expect("duplicate capture must not overwrite the original target");
        assert_eq!(
            bridge.committed.lock().unwrap().as_slice(),
            &["replacement"]
        );
    }

    #[tokio::test]
    async fn supplied_text_without_a_live_target_is_unsupported() {
        let bridge = Arc::new(TestSelectionBridge::default());
        let runtime = LinuxSelectionRuntime::with_bridge(bridge);

        let error = runtime
            .capture(SessionId::new(), Some("detached text".to_string()))
            .await
            .expect_err("detached text cannot prove a Linux replacement target");

        assert_eq!(error.code, BackendErrorCode::Unsupported);
    }

    #[tokio::test]
    async fn preview_target_retention_is_explicitly_unsupported() {
        let runtime = LinuxSelectionRuntime::with_bridge(Arc::new(TestSelectionBridge::default()));

        let error = runtime
            .prepare_preview(SessionId::new())
            .await
            .expect_err("fcitx5 cannot prove a retained preview target");

        assert_eq!(error.code, BackendErrorCode::Unsupported);
    }

    #[tokio::test]
    async fn revert_is_explicitly_unsupported() {
        let runtime = LinuxSelectionRuntime::with_bridge(Arc::new(TestSelectionBridge::default()));

        let error = runtime
            .revert(SessionId::new())
            .await
            .expect_err("fcitx5 replacement has no safe undo contract");

        assert_eq!(error.code, BackendErrorCode::Unsupported);
    }
}
