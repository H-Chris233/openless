#![allow(dead_code, unused_imports, unused_variables)]
use crate::types::InsertStatus;
use crate::windows_ime_ipc::{ImeSubmitRequest, WindowsImeIpcServer};
use crate::windows_ime_profile::{
    is_openless_profile_snapshot, restore_decision, ImeProfileSnapshot, ProfileRestoreDecision,
    WindowsImeProfileError, WindowsImeProfileManager, WindowsImeProfileResult,
};
use crate::windows_ime_protocol::ImeSubmitStatus;

/// 恢复后校验未生效时，重试前的等待时长。
const RESTORE_RETRY_DELAY_MS: u64 = 250;

#[derive(Debug)]
pub enum WindowsImeSessionError {
    Profile(String),
    Ipc(String),
}

impl std::fmt::Display for WindowsImeSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Profile(message) | Self::Ipc(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for WindowsImeSessionError {}

pub fn map_ime_status_to_insert_status(status: ImeSubmitStatus) -> InsertStatus {
    match status {
        ImeSubmitStatus::Committed => InsertStatus::Inserted,
        ImeSubmitStatus::Rejected | ImeSubmitStatus::Failed => InsertStatus::CopiedFallback,
    }
}

pub fn should_fallback_after_ime_result(status: ImeSubmitStatus) -> bool {
    !matches!(status, ImeSubmitStatus::Committed)
}

fn describe_snapshot(snapshot: &ImeProfileSnapshot) -> String {
    format!(
        "kind={:?} lang=0x{:04X} clsid={} profile={}",
        snapshot.kind(),
        snapshot.lang_id(),
        snapshot.clsid().unwrap_or("none"),
        snapshot.profile_guid().unwrap_or("none"),
    )
}

/// 等待重试：在 tokio runtime 线程上执行时用 `block_in_place` 让出工作线程，
/// 避免阻塞 runtime 上其它任务；非 runtime 上下文（如纯同步调用链）直接 sleep。
fn sleep_restore_retry(retry_delay: std::time::Duration) {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(move || std::thread::sleep(retry_delay));
    } else {
        std::thread::sleep(retry_delay);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestoreOutcome {
    /// saved 快照本身是 OpenLess（上次会话疑似未恢复）→ 跳过恢复。
    SkippedSticky,
    /// 校验确认 OpenLess 已不再激活。
    Verified,
    /// 两次尝试后 OpenLess 仍激活（或校验无法确认）。
    FailedAfterRetry,
}

/// 恢复阶段完整流程：粘滞态防护 → 恢复 → 校验 → 一次重试。
///
/// 通过注入 `restore_profile` / `is_openless_active` 让校验-重试逻辑可在任意
/// 平台被单元测试覆盖（生产路径由 `WindowsImeProfileManager` 提供实现）。
fn run_restore_flow(
    saved_profile: &ImeProfileSnapshot,
    mut restore_profile: impl FnMut(&ImeProfileSnapshot) -> WindowsImeProfileResult<()>,
    mut is_openless_active: impl FnMut() -> WindowsImeProfileResult<bool>,
    retry_delay: std::time::Duration,
) -> RestoreOutcome {
    // 粘滞态防护：saved 本身就是 OpenLess（上次会话疑似未恢复）→ 不把 OpenLess
    // 当原输入法写死，跳过恢复并留下诊断日志。
    if is_openless_profile_snapshot(saved_profile) {
        log::warn!(
            "[windows-ime] saved profile is OpenLess itself — previous session likely failed to restore; skipping restore"
        );
        return RestoreOutcome::SkippedSticky;
    }

    // 第一次恢复 + 校验 + 一次重试：TSF 会话级切换偶发不生效时，短等待后重试一次。
    for attempt in 0..2 {
        if attempt > 0 {
            log::info!("[windows-ime] restore did not take effect; retrying (attempt {attempt})");
            sleep_restore_retry(retry_delay);
        }
        if let Err(error) = restore_profile(saved_profile) {
            log::warn!("[windows-ime] restore saved profile failed (attempt {attempt}): {error}");
        }
        match is_openless_active() {
            Ok(false) => {
                log::info!(
                    "[windows-ime] restore verified: OpenLess is no longer the active profile"
                );
                return RestoreOutcome::Verified;
            }
            Ok(true) => {
                log::warn!(
                    "[windows-ime] restore verification: OpenLess is still active (attempt {attempt})"
                );
            }
            Err(error) => {
                log::warn!(
                    "[windows-ime] restore verification check failed (attempt {attempt}): {error}"
                );
            }
        }
    }
    log::error!(
        "[windows-ime] restore did not take effect after retry — IME may remain on OpenLess"
    );
    RestoreOutcome::FailedAfterRetry
}

#[derive(Debug)]
pub struct PreparedWindowsImeSession {
    saved_profile: Option<ImeProfileSnapshot>,
    openless_activated: bool,
}

impl PreparedWindowsImeSession {
    pub fn unavailable() -> Self {
        Self {
            saved_profile: None,
            openless_activated: false,
        }
    }

    pub fn activation_failed(saved_profile: ImeProfileSnapshot) -> Self {
        Self {
            saved_profile: Some(saved_profile),
            openless_activated: false,
        }
    }

    pub fn is_ready_for_tsf_submit(&self) -> bool {
        self.has_saved_profile() && self.openless_was_activated()
    }

    pub fn has_saved_profile(&self) -> bool {
        self.saved_profile.is_some()
    }

    pub fn openless_was_activated(&self) -> bool {
        self.openless_activated
    }

    pub fn activation_failed_with_saved_profile(&self) -> bool {
        self.has_saved_profile() && !self.openless_was_activated()
    }
}

pub struct WindowsImeSessionController {
    profile_manager: WindowsImeProfileManager,
    ipc: WindowsImeIpcServer,
}

impl WindowsImeSessionController {
    pub fn new() -> Self {
        Self {
            profile_manager: WindowsImeProfileManager::new(),
            ipc: WindowsImeIpcServer::new(),
        }
    }

    pub fn prepare_session(&self) -> PreparedWindowsImeSession {
        #[cfg(target_os = "windows")]
        {
            let saved_profile = match self.profile_manager.capture_active_profile() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let error = WindowsImeSessionError::Profile(error.to_string());
                    log::warn!("[windows-ime] capture active profile failed: {error}");
                    return PreparedWindowsImeSession::unavailable();
                }
            };

            // 诊断：会话开始时 OpenLess 已是当前输入法 → 上次会话疑似恢复失败。
            // 此时仍照常激活（幂等），restore_session 的粘滞态防护会跳过"恢复"，
            // 避免把 OpenLess 当原输入法写死（issue #852 的失败状态自粘）。
            if is_openless_profile_snapshot(&saved_profile) {
                log::warn!(
                    "[windows-ime] session began while OpenLess IME was already the active profile — previous session likely failed to restore"
                );
            }

            match self.profile_manager.activate_openless_profile() {
                Ok(()) => PreparedWindowsImeSession {
                    saved_profile: Some(saved_profile),
                    openless_activated: true,
                },
                Err(error) => {
                    let error = WindowsImeSessionError::Profile(error.to_string());
                    log::warn!("[windows-ime] activate OpenLess profile failed: {error}");
                    PreparedWindowsImeSession::activation_failed(saved_profile)
                }
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            PreparedWindowsImeSession::unavailable()
        }
    }

    pub async fn submit_prepared(
        &self,
        prepared: &PreparedWindowsImeSession,
        request: ImeSubmitRequest,
    ) -> Result<InsertStatus, WindowsImeSessionError> {
        if !prepared.is_ready_for_tsf_submit() {
            return Err(WindowsImeSessionError::Ipc(
                "OpenLess IME session is not active".to_string(),
            ));
        }

        let status = self
            .ipc
            .submit_text(request)
            .await
            .map_err(|error| WindowsImeSessionError::Ipc(error.to_string()))?;
        if should_fallback_after_ime_result(status) {
            log::warn!(
                "[windows-ime] TSF submit returned {status:?}; falling back to non-TSF insertion"
            );
        }
        Ok(map_ime_status_to_insert_status(status))
    }

    pub fn restore_session(&self, prepared: PreparedWindowsImeSession) {
        let saved_profile = prepared.saved_profile.as_ref();
        let openless_was_activated = prepared.openless_was_activated();
        let activation_failed = prepared.activation_failed_with_saved_profile();

        // 诊断：记录决策依据 + 恢复前探测到的当前 profile（不影响决策）。
        // issue #852 的恢复决策只依赖会话已知的激活事实，不依赖该探测结果。
        let active_profile_desc = match self.profile_manager.capture_active_profile() {
            Ok(snapshot) => describe_snapshot(&snapshot),
            Err(error) => format!("unavailable: {error}"),
        };
        let saved_desc = match prepared.saved_profile.as_ref() {
            Some(snapshot) => describe_snapshot(snapshot),
            None => "none".to_string(),
        };
        let decision = restore_decision(saved_profile, openless_was_activated, activation_failed);
        log::info!(
            "[windows-ime] restore decision={decision:?} saved_profile={saved_desc} openless_was_activated={openless_was_activated} activation_failed={activation_failed} active_profile={active_profile_desc}"
        );

        if decision != ProfileRestoreDecision::RestoreSavedProfile {
            return;
        }

        let Some(saved_profile) = saved_profile else {
            return;
        };

        run_restore_flow(
            saved_profile,
            |snapshot| self.profile_manager.restore_profile(snapshot),
            || self.profile_manager.is_openless_profile_active(),
            std::time::Duration::from_millis(RESTORE_RETRY_DELAY_MS),
        );
    }
}

impl Default for WindowsImeSessionController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_ime_result_maps_to_inserted() {
        assert_eq!(
            map_ime_status_to_insert_status(ImeSubmitStatus::Committed),
            InsertStatus::Inserted
        );
    }

    #[test]
    fn rejected_ime_result_requests_fallback() {
        assert!(should_fallback_after_ime_result(ImeSubmitStatus::Rejected));
        assert!(should_fallback_after_ime_result(ImeSubmitStatus::Failed));
        assert!(!should_fallback_after_ime_result(
            ImeSubmitStatus::Committed
        ));
    }

    #[tokio::test]
    async fn submit_prepared_reports_unavailable_session() {
        let controller = WindowsImeSessionController::new();
        let result = controller
            .submit_prepared(
                &PreparedWindowsImeSession::unavailable(),
                ImeSubmitRequest {
                    session_id: "session-1".to_string(),
                    text: "hello".to_string(),
                    created_at: "2026-05-01T12:00:00Z".to_string(),
                    target: None,
                },
            )
            .await;

        assert!(
            matches!(result, Err(WindowsImeSessionError::Ipc(message)) if message == "OpenLess IME session is not active")
        );
    }

    #[test]
    fn restore_decision_uses_confirmed_activation_state_only() {
        // 激活成功且有原快照 → 恢复（决策不再依赖 profile-current 探测，issue #852）。
        let activated = PreparedWindowsImeSession {
            saved_profile: Some(ImeProfileSnapshot::keyboard_layout(0x0409, 0x0409_0409)),
            openless_activated: true,
        };
        assert_eq!(
            restore_decision(
                activated.saved_profile.as_ref(),
                activated.openless_was_activated(),
                activated.activation_failed_with_saved_profile(),
            ),
            ProfileRestoreDecision::RestoreSavedProfile
        );

        // 从未激活（unavailable）→ 保持现状。
        let unavailable = PreparedWindowsImeSession::unavailable();
        assert_eq!(
            restore_decision(
                unavailable.saved_profile.as_ref(),
                unavailable.openless_was_activated(),
                unavailable.activation_failed_with_saved_profile(),
            ),
            ProfileRestoreDecision::KeepCurrentProfile
        );
    }

    #[test]
    fn activation_failed_session_keeps_snapshot_but_cannot_submit() {
        let prepared = PreparedWindowsImeSession::activation_failed(
            ImeProfileSnapshot::keyboard_layout(0x0409, 0x0409_0409),
        );

        assert!(prepared.has_saved_profile());
        assert!(!prepared.openless_was_activated());
        assert!(!prepared.is_ready_for_tsf_submit());
        assert!(prepared.activation_failed_with_saved_profile());
    }

    #[test]
    fn restore_flow_skips_when_saved_profile_is_openless_itself() {
        // 粘滞态防护：saved 是 OpenLess → 跳过恢复，restore 不被调用（issue #852）。
        let mut restore_calls = 0;
        let outcome = run_restore_flow(
            &ImeProfileSnapshot::text_service(
                0x0804,
                "{6b9f3f4f-5ee7-42d6-9c61-9f80b03a5d7d}".to_string(),
                "{9b5f5e04-23f6-47da-9a26-d221f6c3f02e}".to_string(),
            ),
            |_| {
                restore_calls += 1;
                Ok(())
            },
            || Ok(false),
            std::time::Duration::ZERO,
        );

        assert_eq!(outcome, RestoreOutcome::SkippedSticky);
        assert_eq!(restore_calls, 0);
    }

    #[test]
    fn restore_flow_verifies_without_retry_when_openless_no_longer_active() {
        let mut restore_calls = 0;
        let outcome = run_restore_flow(
            &ImeProfileSnapshot::keyboard_layout(0x0409, 0x0409_0409),
            |_| {
                restore_calls += 1;
                Ok(())
            },
            || Ok(false),
            std::time::Duration::ZERO,
        );

        assert_eq!(outcome, RestoreOutcome::Verified);
        assert_eq!(restore_calls, 1);
    }

    #[test]
    fn restore_flow_retries_once_when_openless_stays_active() {
        let mut restore_calls = 0;
        let outcome = run_restore_flow(
            &ImeProfileSnapshot::keyboard_layout(0x0409, 0x0409_0409),
            |_| {
                restore_calls += 1;
                Ok(())
            },
            || Ok(true),
            std::time::Duration::ZERO,
        );

        assert_eq!(outcome, RestoreOutcome::FailedAfterRetry);
        assert_eq!(restore_calls, 2);
    }

    #[test]
    fn restore_flow_treats_restore_error_with_verified_profile_as_success() {
        // legacy 已生效但 API 报错时，校验确认切走仍算成功（任一成功即整体成功）。
        let outcome = run_restore_flow(
            &ImeProfileSnapshot::keyboard_layout(0x0409, 0x0409_0409),
            |_| {
                Err(WindowsImeProfileError::WindowsApi(
                    "legacy failed".to_string(),
                ))
            },
            || Ok(false),
            std::time::Duration::ZERO,
        );

        assert_eq!(outcome, RestoreOutcome::Verified);
    }

    #[test]
    fn restore_flow_retries_when_verification_check_errors() {
        // 校验探测报错不能视为成功：重试一次后仍失败。
        let mut restore_calls = 0;
        let outcome = run_restore_flow(
            &ImeProfileSnapshot::keyboard_layout(0x0409, 0x0409_0409),
            |_| {
                restore_calls += 1;
                Ok(())
            },
            || {
                Err(WindowsImeProfileError::WindowsApi(
                    "probe failed".to_string(),
                ))
            },
            std::time::Duration::ZERO,
        );

        assert_eq!(outcome, RestoreOutcome::FailedAfterRetry);
        assert_eq!(restore_calls, 2);
    }
}
