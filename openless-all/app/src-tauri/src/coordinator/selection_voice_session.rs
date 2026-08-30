//! Selection-voice edit session (issue #987 desktop MVP, Windows-first).

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use super::{
    emit_capsule, qa_session, schedule_capsule_idle, Coordinator, Inner,
};
use crate::coordinator_state::SessionId;
use crate::selection::SelectionInsertionTarget;
use crate::types::{CapsuleState, HotkeyMode, InsertStatus};
use openless_core::{
    BackendError, BackendErrorCode, SelectionCapture, SelectionVoiceApplyOutcome,
    SelectionVoiceDisposition, SelectionVoiceEditAction, SelectionVoicePhase,
    SessionId as CoreSessionId,
};

static SELECTION_VOICE_BUSY: AtomicBool = AtomicBool::new(false);

/// 与听写 Auto 模式一致：短于该阈值视为点按（切换式锁存），否则视为按住说话。
const AUTO_HOLD_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(350);

/// 选区语音会话占用麦克风时，禁止再开听写/追问录音。
pub(super) fn selection_voice_blocks_other_recording(inner: &Arc<Inner>) -> bool {
    inner.selection_voice_host.lock().blocks_recording
}

fn selection_voice_user_message(error: &str) -> String {
    match error {
        "dictationActive" => "正在听写，请先结束录音".into(),
        "selectionVoiceNoSelection" => "请先选中文字".into(),
        "selectionVoiceTargetUnavailable" => "无法定位选区，请重试".into(),
        "selectionVoiceBusy" => "选区语音会话进行中".into(),
        other => other.into(),
    }
}

fn emit_selection_voice_begin_error(inner: &Arc<Inner>, error: &str) {
    emit_capsule(
        inner,
        CapsuleState::Error,
        0.0,
        0,
        Some(selection_voice_user_message(error)),
        None,
    );
}

fn emit_selection_voice_end_error(inner: &Arc<Inner>, error: &str) {
    log::warn!("[selection-voice] workflow failed: {error}");
    let message = selection_voice_end_message(error);
    emit_capsule(inner, CapsuleState::Error, 0.0, 0, Some(message), None);
    schedule_capsule_idle(inner, 2500);
}

fn selection_voice_end_message(error: &str) -> String {
    if error.contains("invalid EditPlan XML") || error.contains("invalid EditPlan JSON") {
        return "编辑方案解析失败，请重试".into();
    }
    if error.contains("edit plan has no operations") {
        return "未能生成有效编辑方案，请重试".into();
    }
    if error.contains("edit plan has too many operations") {
        return "编辑方案过于复杂，请缩短指令".into();
    }
    if error.contains("edit operation exceeds size limit") {
        return "编辑内容过长，请缩短选区或拆步操作".into();
    }
    if error.contains("global timeout") || error.contains("bailian global timeout") {
        return "语音识别超时，请重试".into();
    }
    if error.contains("selectionVoiceAsrUnavailable") {
        return "语音识别不可用，请重试".into();
    }
    if error.contains("translation unchanged") {
        return "翻译结果与原文相同，请重试或调整指令".into();
    }
    selection_voice_user_message(error)
}

#[derive(Debug, Clone, Default)]
pub(super) struct SelectionVoiceHostState {
    /// Opaque native target only. Selection text, instruction, intent and
    /// preview remain exclusively owned by `openless-core`.
    target_session_id: Option<CoreSessionId>,
    insertion_target: SelectionInsertionTarget,
    /// Host resource ownership used by synchronous audio callbacks.
    recorder_session_id: Option<SessionId>,
    /// Host shortcut arbitration state; not part of the business session.
    blocks_recording: bool,
    /// Auto 模式判定短按/长按的按下时刻。
    auto_press_at: Option<std::time::Instant>,
}

fn core_error(error: BackendError) -> String {
    match error.code {
        BackendErrorCode::Busy => "selectionVoiceBusy".to_string(),
        BackendErrorCode::Cancelled => "selectionVoicePreviewUnavailable".to_string(),
        BackendErrorCode::InvalidArgument if error.message.contains("intent") => error
            .message
            .rsplit_once(':')
            .map(|(_, intent)| format!("selectionVoiceInvalidIntent:{}", intent.trim()))
            .unwrap_or_else(|| "selectionVoiceInvalidIntent".to_string()),
        BackendErrorCode::InvalidState if error.message.contains("intent prompt") => {
            "selectionVoiceIntentPromptUnavailable".to_string()
        }
        BackendErrorCode::InvalidState if error.message.contains("preview") => {
            "selectionVoicePreviewUnavailable".to_string()
        }
        _ => error.message,
    }
}

fn owner_session_id(session_id: SessionId) -> CoreSessionId {
    CoreSessionId::from_uuid(session_id)
}

fn target_for_session(
    inner: &Arc<Inner>,
    session_id: CoreSessionId,
) -> Result<SelectionInsertionTarget, String> {
    let host = inner.selection_voice_host.lock();
    if host.target_session_id != Some(session_id) {
        return Err("selectionVoiceTargetUnavailable".to_string());
    }
    Ok(host.insertion_target.clone())
}

fn clear_host_session(inner: &Arc<Inner>, session_id: CoreSessionId) {
    let mut host = inner.selection_voice_host.lock();
    if host.target_session_id == Some(session_id) {
        *host = SelectionVoiceHostState::default();
    }
}

pub(super) fn bind_selection_voice_target_state(
    host_slot: &Arc<parking_lot::Mutex<SelectionVoiceHostState>>,
    session_id: CoreSessionId,
    insertion_target: SelectionInsertionTarget,
) -> Result<(), String> {
    if !crate::selection::selection_insertion_target_is_captured(&insertion_target) {
        return Err("selectionVoiceTargetUnavailable".to_string());
    }
    let mut host = host_slot.lock();
    host.target_session_id = Some(session_id);
    host.insertion_target = insertion_target;
    host.recorder_session_id = None;
    host.blocks_recording = false;
    Ok(())
}

pub(super) fn selection_voice_recording_active(inner: &Arc<Inner>, session_id: SessionId) -> bool {
    inner.selection_voice_host.lock().recorder_session_id == Some(session_id)
}

pub(super) fn handle_selection_voice_recorder_error(
    inner: &Arc<Inner>,
    recorder_session_id: SessionId,
    message: String,
) {
    let core_session_id = {
        let mut host = inner.selection_voice_host.lock();
        if host.recorder_session_id != Some(recorder_session_id) {
            return;
        }
        let core_session_id = host.target_session_id;
        *host = SelectionVoiceHostState::default();
        core_session_id
    };
    if let Some(core_session_id) = core_session_id {
        let inner = Arc::clone(inner);
        let host = inner.host.clone();
        host.spawn(async move {
            let _ = inner
                .backend
                .services()
                .selection_voice
                .cancel(Some(core_session_id))
                .await;
        });
    }
    emit_selection_voice_end_error(inner, &message);
}

#[cfg(test)]
pub(super) fn set_selection_voice_recorder_session_for_test(
    inner: &Arc<Inner>,
    session_id: Option<SessionId>,
) {
    inner.selection_voice_host.lock().recorder_session_id = session_id;
}

pub(super) async fn handle_selection_voice_pressed(inner: &Arc<Inner>) {
    if !inner.backend.get_preferences().selection_voice_enabled {
        return;
    }

    let mode = inner.backend.get_preferences().hotkey.mode;
    let phase = match inner.backend.services().selection_voice.snapshot().await {
        Ok(snapshot) => snapshot.phase,
        Err(error) => {
            log::warn!("[selection-voice] snapshot failed: {error}");
            return;
        }
    };

    // 切换式 / Auto 锁存态的「再按一次停止」不能被子 busy 挡住。
    match (mode, phase) {
        (HotkeyMode::Toggle, SelectionVoicePhase::Recording)
        | (HotkeyMode::Auto, SelectionVoicePhase::Recording) => {
            if let Err(error) = end_selection_voice_session(inner).await {
                log::warn!("[selection-voice] end on stop press failed: {error}");
            }
            SELECTION_VOICE_BUSY.store(false, Ordering::Release);
            inner.selection_voice_host.lock().auto_press_at = None;
            return;
        }
        _ => {}
    }

    if SELECTION_VOICE_BUSY.swap(true, Ordering::AcqRel) {
        return;
    }

    let begin_result = match (mode, phase) {
        (HotkeyMode::Toggle, SelectionVoicePhase::Idle) => {
            begin_selection_voice_session(inner).await
        }
        (HotkeyMode::Hold, SelectionVoicePhase::Idle) => begin_selection_voice_session(inner).await,
        (HotkeyMode::Auto, SelectionVoicePhase::Idle) => {
            {
                inner.selection_voice_host.lock().auto_press_at = Some(std::time::Instant::now());
            }
            begin_selection_voice_session(inner).await
        }
        _ => {
            SELECTION_VOICE_BUSY.store(false, Ordering::Release);
            return;
        }
    };

    if let Err(error) = begin_result {
        log::warn!("[selection-voice] begin failed: {error}");
        emit_selection_voice_begin_error(inner, &error);
        {
            inner.selection_voice_host.lock().auto_press_at = None;
        }
    }
    SELECTION_VOICE_BUSY.store(false, Ordering::Release);
}

pub(super) async fn handle_selection_voice_released(inner: &Arc<Inner>) {
    if !inner.backend.get_preferences().selection_voice_enabled {
        return;
    }
    let mode = inner.backend.get_preferences().hotkey.mode;
    if mode == HotkeyMode::Toggle {
        return;
    }
    let phase = match inner.backend.services().selection_voice.snapshot().await {
        Ok(snapshot) => snapshot.phase,
        Err(error) => {
            log::warn!("[selection-voice] snapshot failed: {error}");
            return;
        }
    };
    if phase != SelectionVoicePhase::Recording {
        SELECTION_VOICE_BUSY.store(false, Ordering::Release);
        return;
    }
    if mode == HotkeyMode::Hold {
        if let Err(error) = end_selection_voice_session(inner).await {
            log::warn!("[selection-voice] end on hold release failed: {error}");
        }
        SELECTION_VOICE_BUSY.store(false, Ordering::Release);
        return;
    }
    if mode == HotkeyMode::Auto {
        let released_at = std::time::Instant::now();
        let held_long = {
            inner
                .selection_voice_host
                .lock()
                .auto_press_at
                .take()
                .map(|pressed_at| {
                    released_at.saturating_duration_since(pressed_at) >= AUTO_HOLD_THRESHOLD
                })
                .unwrap_or(false)
        };
        if held_long {
            if let Err(error) = end_selection_voice_session(inner).await {
                log::warn!("[selection-voice] end on auto hold release failed: {error}");
            }
        } else {
            log::info!("[selection-voice] auto short-tap latched; next press stops");
        }
        SELECTION_VOICE_BUSY.store(false, Ordering::Release);
    }
}

async fn begin_selection_voice_session(inner: &Arc<Inner>) -> Result<(), String> {
    if !matches!(
        inner.state.lock().phase,
        crate::coordinator_state::SessionPhase::Idle
    ) {
        return Err("dictationActive".into());
    }
    if selection_voice_blocks_other_recording(inner) {
        return Err("selectionVoiceBusy".into());
    }

    let (selection_opt, insertion_target) = crate::selection::resolve_selection_workspace_capture();
    let selection = selection_opt.ok_or_else(|| "selectionVoiceNoSelection".to_string())?;
    if !crate::selection::selection_insertion_target_is_captured(&insertion_target) {
        return Err("selectionVoiceTargetUnavailable".into());
    }

    let session_id = inner
        .backend
        .services()
        .selection_voice
        .begin(SelectionCapture {
            text: selection.text,
            source_app: selection.source_app,
        })
        .await
        .map_err(core_error)?;
    let recorder_session_id = session_id.as_uuid();
    {
        let mut host = inner.selection_voice_host.lock();
        host.target_session_id = Some(session_id);
        host.insertion_target = insertion_target;
        host.recorder_session_id = Some(recorder_session_id);
        host.blocks_recording = true;
    }

    emit_capsule(inner, CapsuleState::Recording, 0.0, 0, None, None);
    if let Err(error) = qa_session::start_selection_voice_recorder(inner, recorder_session_id).await
    {
        let _ = inner
            .backend
            .services()
            .selection_voice
            .cancel(Some(session_id))
            .await;
        clear_host_session(inner, session_id);
        return Err(error);
    }
    Ok(())
}

async fn end_selection_voice_session(inner: &Arc<Inner>) -> Result<(), String> {
    let snapshot = inner
        .backend
        .services()
        .selection_voice
        .snapshot()
        .await
        .map_err(core_error)?;
    if snapshot.phase != SelectionVoicePhase::Recording {
        return Ok(());
    }
    let session_id = snapshot
        .session_id
        .ok_or_else(|| "selectionVoiceSessionUnavailable".to_string())?;
    inner
        .backend
        .services()
        .selection_voice
        .mark_processing(session_id)
        .await
        .map_err(core_error)?;
    // 结束录音后熄灭胶囊；预览模式才打开华词面板，直接覆盖则静默处理。
    emit_capsule(inner, CapsuleState::Idle, 0.0, 0, None, None);
    schedule_capsule_idle(inner, 0);
    let workflow: Result<EndWorkflowOutcome, String> = async {
        let transcript =
            qa_session::finish_selection_voice_transcript(inner, session_id.as_uuid()).await?;
        {
            let mut host = inner.selection_voice_host.lock();
            host.recorder_session_id = None;
            host.blocks_recording = false;
        }
        if transcript.trim().is_empty() {
            inner
                .backend
                .services()
                .selection_voice
                .cancel(Some(session_id))
                .await
                .map_err(core_error)?;
            clear_host_session(inner, session_id);
            emit_capsule(
                inner,
                CapsuleState::Cancelled,
                0.0,
                0,
                Some("未识别到指令".into()),
                None,
            );
            schedule_capsule_idle(inner, 2000);
            return Ok(EndWorkflowOutcome::Finished);
        }

        let disposition = inner
            .backend
            .services()
            .selection_voice
            .process_transcript(session_id, transcript)
            .await
            .map_err(core_error)?;
        if disposition.is_awaiting_intent() {
            inner.host.show_selection_voice_intent_prompt();
            return Ok(EndWorkflowOutcome::AwaitingIntent);
        }

        continue_selection_voice_disposition(inner, disposition).await?;
        Ok(EndWorkflowOutcome::Finished)
    }
    .await;

    match workflow {
        Ok(EndWorkflowOutcome::AwaitingIntent) => Ok(()),
        Ok(EndWorkflowOutcome::Finished) => Ok(()),
        Err(error) => {
            let _ = inner
                .backend
                .services()
                .selection_voice
                .cancel(Some(session_id))
                .await;
            clear_host_session(inner, session_id);
            emit_selection_voice_end_error(inner, &error);
            Err(error)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndWorkflowOutcome {
    Finished,
    AwaitingIntent,
}

async fn continue_selection_voice_disposition(
    inner: &Arc<Inner>,
    disposition: SelectionVoiceDisposition,
) -> Result<(), String> {
    match disposition {
        SelectionVoiceDisposition::AwaitingIntent { .. } => {
            return Err("selectionVoiceIntentPromptUnavailable".to_string());
        }
        SelectionVoiceDisposition::Question {
            session_id,
            instruction,
            ..
        } => run_selection_voice_question(inner, session_id, &instruction).await?,
        SelectionVoiceDisposition::Edit { session_id, .. } => {
            run_selection_voice_edit(inner, session_id).await?
        }
    }
    Ok(())
}

async fn run_selection_voice_question(
    inner: &Arc<Inner>,
    session_id: CoreSessionId,
    instruction_polished: &str,
) -> Result<(), String> {
    inner
        .backend
        .services()
        .qa
        .show()
        .await
        .map_err(core_error)?;
    inner
        .backend
        .services()
        .qa
        .set_edit_instruction_mode(false)
        .await
        .map_err(core_error)?;
    inner
        .backend
        .services()
        .qa
        .submit_text(instruction_polished.to_string())
        .await
        .map_err(core_error)?;
    inner
        .backend
        .services()
        .selection_voice
        .complete(session_id)
        .await
        .map_err(core_error)?;
    clear_host_session(inner, session_id);
    Ok(())
}

async fn run_selection_voice_edit(
    inner: &Arc<Inner>,
    session_id: CoreSessionId,
) -> Result<(), String> {
    emit_capsule(
        inner,
        CapsuleState::Polishing,
        0.0,
        0,
        Some("正在生成编辑…".into()),
        None,
    );
    match inner
        .backend
        .services()
        .selection_voice
        .prepare_edit(session_id, None)
        .await
        .map_err(core_error)?
    {
        SelectionVoiceEditAction::OpenConversation { instruction, .. } => {
            inner
                .backend
                .services()
                .qa
                .show()
                .await
                .map_err(core_error)?;
            inner
                .backend
                .services()
                .qa
                .set_edit_instruction_mode(true)
                .await
                .map_err(core_error)?;
            inner
                .backend
                .services()
                .qa
                .submit_text(instruction)
                .await
                .map_err(core_error)?;
        }
        SelectionVoiceEditAction::ReadyToApply { preview } => {
            let coordinator = Coordinator {
                inner: Arc::clone(inner),
            };
            coordinator
                .confirm_selection_voice_preview(preview.text, None)
                .await?;
        }
    }
    emit_capsule(inner, CapsuleState::Idle, 0.0, 0, None, None);
    schedule_capsule_idle(inner, 0);
    Ok(())
}

impl Coordinator {
    pub(crate) async fn continue_confirmed_selection_voice_intent(
        &self,
        session_id: CoreSessionId,
        disposition: SelectionVoiceDisposition,
    ) -> Result<(), String> {
        self.inner.host.hide_selection_voice_intent_prompt();
        let result = continue_selection_voice_disposition(&self.inner, disposition).await;
        if let Err(error) = &result {
            let _ = self
                .inner
                .backend
                .services()
                .selection_voice
                .cancel(Some(session_id))
                .await;
            clear_host_session(&self.inner, session_id);
            emit_selection_voice_end_error(&self.inner, error);
        }
        result
    }

    pub(crate) fn finish_cancelled_selection_voice_host(&self, session_id: Option<CoreSessionId>) {
        if let Some(session_id) = session_id {
            clear_host_session(&self.inner, session_id);
        }
        self.inner.host.hide_selection_voice_intent_prompt();
    }

    pub(crate) fn bind_selection_voice_target(
        &self,
        session_id: CoreSessionId,
        insertion_target: SelectionInsertionTarget,
    ) -> Result<(), String> {
        bind_selection_voice_target_state(
            &self.inner.selection_voice_host,
            session_id,
            insertion_target,
        )
    }

    pub(crate) async fn confirm_selection_voice_preview(
        &self,
        text: String,
        qa_session_id: Option<SessionId>,
    ) -> Result<(), String> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err("selectionVoiceEmptyOutput".into());
        }

        if qa_session_id.is_some() {
            if !self.inner.qa_context.is_panel_visible() {
                return Err("selectionVoicePreviewUnavailable".into());
            }
        }
        let owner = qa_session_id.map(owner_session_id);
        let ticket = self
            .inner
            .backend
            .services()
            .selection_voice
            .begin_preview_apply(owner, text.clone())
            .await
            .map_err(core_error)?;
        match self.apply_selection_voice_preview_ticket(&ticket) {
            Ok(outcome) => {
                self.inner
                    .backend
                    .services()
                    .selection_voice
                    .finish_preview_apply(ticket.ticket_id, outcome)
                    .await
                    .map_err(core_error)?;
            }
            Err(error) => {
                let _ = self
                    .inner
                    .backend
                    .services()
                    .selection_voice
                    .finish_preview_apply(ticket.ticket_id, SelectionVoiceApplyOutcome::Failed)
                    .await;
                return Err(error);
            }
        }

        self.finish_selection_voice_preview_host(ticket.session_id);
        Ok(())
    }

    pub(crate) fn apply_selection_voice_preview_ticket(
        &self,
        ticket: &openless_core::SelectionVoiceApplyTicket,
    ) -> Result<SelectionVoiceApplyOutcome, String> {
        let prefs = self.inner.backend.get_preferences();
        let insertion_target = target_for_session(&self.inner, ticket.session_id)?;
        if !crate::selection::reactivate_selection_insertion_target(&insertion_target) {
            return Err("selectionVoiceTargetUnavailable".to_string());
        }
        let validation = crate::selection::validate_selection_insertion_target(
            &insertion_target,
            &ticket.source_text,
        );
        if let Some(code) = validation.error_code() {
            return Err(code.to_string());
        }
        let status = self.inner.inserter.insert(
            &ticket.replacement_text,
            prefs.restore_clipboard_after_paste,
            prefs.paste_shortcut,
        );
        match status {
            InsertStatus::Inserted => Ok(SelectionVoiceApplyOutcome::Inserted),
            InsertStatus::PasteSent => Ok(SelectionVoiceApplyOutcome::PasteSent),
            InsertStatus::CopiedFallback => Ok(SelectionVoiceApplyOutcome::CopiedFallback),
            InsertStatus::Failed => Err("selectionVoiceInsertFailed".to_string()),
        }
    }

    pub(crate) fn finish_selection_voice_preview_host(&self, session_id: CoreSessionId) {
        clear_host_session(&self.inner, session_id);
        emit_capsule(&self.inner, CapsuleState::Idle, 0.0, 0, None, None);
        schedule_capsule_idle(&self.inner, 0);
    }
}
