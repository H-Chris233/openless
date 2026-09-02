use std::sync::Mutex;

use crate::{BackendError, BackendErrorCode, SessionId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoiceSessionKind {
    Dictation,
    LessComputer,
    SelectionVoice,
    Qa,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveVoiceSession {
    session_id: SessionId,
    kind: VoiceSessionKind,
}

#[derive(Debug, Default)]
pub(crate) struct VoiceSessionGate {
    active: Mutex<Option<ActiveVoiceSession>>,
}

impl VoiceSessionGate {
    pub(crate) fn acquire(
        &self,
        session_id: SessionId,
        kind: VoiceSessionKind,
    ) -> Result<(), BackendError> {
        let mut active = self.active.lock().expect("voice session lock poisoned");
        match *active {
            Some(current) if current.session_id == session_id && current.kind == kind => Ok(()),
            Some(current) => Err(BackendError::new(
                BackendErrorCode::Busy,
                format!("another voice session is active: {:?}", current.kind),
            )),
            None => {
                *active = Some(ActiveVoiceSession { session_id, kind });
                Ok(())
            }
        }
    }

    pub(crate) fn release(&self, session_id: SessionId) {
        let mut active = self.active.lock().expect("voice session lock poisoned");
        if active.is_some_and(|current| current.session_id == session_id) {
            *active = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_session_is_idempotent_and_other_kinds_are_busy() {
        let gate = VoiceSessionGate::default();
        let session_id = SessionId::new();
        gate.acquire(session_id, VoiceSessionKind::Dictation)
            .unwrap();
        gate.acquire(session_id, VoiceSessionKind::Dictation)
            .unwrap();
        assert_eq!(
            gate.acquire(SessionId::new(), VoiceSessionKind::Qa)
                .unwrap_err()
                .code,
            BackendErrorCode::Busy
        );
        gate.release(SessionId::new());
        assert_eq!(
            gate.acquire(SessionId::new(), VoiceSessionKind::Qa)
                .unwrap_err()
                .code,
            BackendErrorCode::Busy
        );
        gate.release(session_id);
        gate.acquire(SessionId::new(), VoiceSessionKind::Qa)
            .unwrap();
    }
}
