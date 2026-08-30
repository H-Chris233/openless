//! Windows Selection Voice recording helpers.
//!
//! QA lifecycle, messages and cancellation are owned by `openless-core::QaService`.
//! This module remains only because Selection Voice still uses the legacy native
//! recorder/provider wiring while its business session already lives in core.

use super::resources::*;
use super::*;

#[cfg(all(not(mobile), target_os = "windows"))]
fn selection_voice_recording_can_continue(inner: &Arc<Inner>, session_id: SessionId) -> bool {
    super::selection_voice_session::selection_voice_recording_active(inner, session_id)
}

#[cfg(all(not(mobile), target_os = "windows"))]
async fn wait_for_selection_voice_cancel(inner: &Arc<Inner>, session_id: SessionId) {
    while selection_voice_recording_can_continue(inner, session_id) {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[cfg(all(not(mobile), target_os = "windows"))]
fn spawn_selection_voice_recorder_error_monitor(
    inner: &Arc<Inner>,
    session_id: SessionId,
    rx: mpsc::Receiver<RecorderError>,
) {
    let inner = Arc::clone(inner);
    std::thread::Builder::new()
        .name("openless-selection-voice-recorder-error-monitor".into())
        .spawn(move || {
            if let Ok(error) = rx.recv() {
                if !selection_voice_recording_can_continue(&inner, session_id) {
                    log::warn!(
                        "[selection-voice] recorder error from stale session {session_id} dropped: {error}"
                    );
                    return;
                }
                log::error!("[selection-voice] recorder runtime error: {error}");
                stop_selection_voice_recorder_for_session(&inner, session_id);
                cancel_selection_voice_asr_for_session(&inner, session_id);
                super::selection_voice_session::handle_selection_voice_recorder_error(
                    &inner,
                    session_id,
                    format!("录音设备异常: {error}"),
                );
            }
        })
        .ok();
}

#[cfg(all(not(mobile), target_os = "windows"))]
enum SelectionVoiceTranscribeOutcome {
    Done(Result<RawTranscript, String>),
    Cancelled,
}

#[cfg(all(not(mobile), target_os = "windows"))]
async fn transcribe_selection_voice_asr(
    inner: &Arc<Inner>,
    session_id: SessionId,
    asr: ActiveAsr,
) -> SelectionVoiceTranscribeOutcome {
    let uses_global_timeout = asr_transcribe_uses_global_timeout(&asr);
    let result = match asr {
        ActiveAsr::Volcengine(asr) => {
            debug_assert!(uses_global_timeout);
            if let Err(error) = asr.send_last_frame().await {
                log::error!("[selection-voice] send last frame failed: {error}");
            }
            let timeout = std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS);
            match tokio::time::timeout(timeout, asr.await_final_result()).await {
                Ok(Ok(raw)) => Ok(raw),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => {
                    asr.cancel();
                    Err("global timeout".to_string())
                }
            }
        }
        ActiveAsr::Bailian(asr) => {
            debug_assert!(uses_global_timeout);
            if let Err(error) = asr.send_last_frame().await {
                log::error!("[selection-voice] Bailian send last frame failed: {error}");
            }
            let timeout = std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS);
            match tokio::time::timeout(timeout, asr.await_final_result()).await {
                Ok(Ok(raw)) => Ok(raw),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => {
                    asr.cancel();
                    Err("bailian global timeout".to_string())
                }
            }
        }
        ActiveAsr::Qwen3Realtime(asr) => {
            debug_assert!(uses_global_timeout);
            if let Err(error) = asr.send_last_frame().await {
                log::error!("[selection-voice] Qwen3 send last frame failed: {error}");
            }
            let timeout = std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS);
            match tokio::time::timeout(timeout, asr.await_final_result()).await {
                Ok(Ok(raw)) => Ok(raw),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => {
                    asr.cancel();
                    Err("qwen3 realtime global timeout".to_string())
                }
            }
        }
        ActiveAsr::StepfunRealtime(asr) => {
            debug_assert!(uses_global_timeout);
            if let Err(error) = asr.send_last_frame().await {
                log::error!("[selection-voice] StepFun send last frame failed: {error}");
            }
            let timeout = std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS);
            match tokio::time::timeout(timeout, asr.await_final_result()).await {
                Ok(Ok(raw)) => Ok(raw),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => {
                    asr.cancel();
                    Err("stepfun realtime global timeout".to_string())
                }
            }
        }
        ActiveAsr::Xfyun(asr) => {
            debug_assert!(uses_global_timeout);
            if let Err(error) = asr.send_last_frame().await {
                log::error!("[selection-voice] iFlytek send last frame failed: {error}");
            }
            let timeout = std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS);
            match tokio::time::timeout(timeout, asr.await_final_result()).await {
                Ok(Ok(raw)) => Ok(raw),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => {
                    asr.cancel();
                    Err("xfyun global timeout".to_string())
                }
            }
        }
        ActiveAsr::Whisper(asr) => {
            debug_assert!(uses_global_timeout);
            let timeout = std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS);
            match tokio::time::timeout(timeout, asr.transcribe()).await {
                Ok(Ok(raw)) => Ok(raw),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err("whisper global timeout".to_string()),
            }
        }
        ActiveAsr::Mimo(asr) => {
            debug_assert!(uses_global_timeout);
            let timeout = std::time::Duration::from_secs(COORDINATOR_GLOBAL_TIMEOUT_SECS);
            match tokio::time::timeout(timeout, asr.transcribe()).await {
                Ok(Ok(raw)) => Ok(raw),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err("mimo global timeout".to_string()),
            }
        }
        ActiveAsr::DashScopeMultimodal(asr) => {
            debug_assert!(uses_global_timeout);
            let audio_secs = asr.buffer_duration_ms() as f64 / 1000.0;
            let timeout = asr.transcribe_timeout(audio_secs);
            tokio::select! {
                result = tokio::time::timeout(timeout, asr.transcribe()) => match result {
                    Ok(Ok(raw)) => Ok(raw),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err("dashscope multimodal global timeout".to_string()),
                },
                _ = wait_for_selection_voice_cancel(inner, session_id) => {
                    asr.cancel();
                    return SelectionVoiceTranscribeOutcome::Cancelled;
                }
            }
        }
        ActiveAsr::ElevenLabs(asr) => {
            debug_assert!(uses_global_timeout);
            let audio_secs = asr.buffer_duration_ms() as f64 / 1000.0;
            let timeout = crate::asr::elevenlabs::transcribe_timeout(audio_secs);
            tokio::select! {
                result = tokio::time::timeout(timeout, asr.transcribe()) => match result {
                    Ok(Ok(raw)) => Ok(raw),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err("elevenlabs dynamic timeout".to_string()),
                },
                _ = wait_for_selection_voice_cancel(inner, session_id) => {
                    asr.cancel();
                    return SelectionVoiceTranscribeOutcome::Cancelled;
                }
            }
        }
        ActiveAsr::FoundryLocalWhisper(local) => {
            debug_assert!(!uses_global_timeout);
            let audio_secs = local.buffer_duration_ms() as f64 / 1000.0;
            let timeout = windows_local_asr_transcribe_timeout(audio_secs);
            let notices = foundry_selection_voice_fallback_notice_callback(inner, session_id);
            tokio::select! {
                result = local.transcribe_with_fallback_notice(timeout, notices) => match result {
                    Ok(outcome) => {
                        if !selection_voice_recording_can_continue(inner, session_id) {
                            local.cancel();
                            schedule_foundry_local_asr_release(
                                inner,
                                AsrReleaseSession::SelectionVoice(session_id),
                                None,
                            );
                            return SelectionVoiceTranscribeOutcome::Cancelled;
                        }
                        schedule_foundry_local_asr_release(
                            inner,
                            AsrReleaseSession::SelectionVoice(session_id),
                            outcome.primary_recovery,
                        );
                        Ok(outcome.raw)
                    }
                    Err(error) => {
                        schedule_foundry_local_asr_release(
                            inner,
                            AsrReleaseSession::SelectionVoice(session_id),
                            None,
                        );
                        Err(error.to_string())
                    }
                },
                _ = wait_for_selection_voice_cancel(inner, session_id) => {
                    local.cancel();
                    schedule_foundry_local_asr_release(
                        inner,
                        AsrReleaseSession::SelectionVoice(session_id),
                        None,
                    );
                    return SelectionVoiceTranscribeOutcome::Cancelled;
                }
            }
        }
        ActiveAsr::SherpaOnnxLocal(local) => {
            debug_assert!(!uses_global_timeout);
            let audio_secs = local.buffer_duration_ms() as f64 / 1000.0;
            let timeout = windows_local_asr_transcribe_timeout(audio_secs);
            let result = local
                .transcribe(timeout)
                .await
                .map_err(|error| error.to_string());
            schedule_sherpa_onnx_release(inner, AsrReleaseSession::SelectionVoice(session_id));
            result
        }
    };
    SelectionVoiceTranscribeOutcome::Done(result)
}

#[cfg(all(not(mobile), target_os = "windows"))]
pub(super) async fn start_selection_voice_recorder(
    inner: &Arc<Inner>,
    session_id: SessionId,
) -> Result<(), String> {
    if pipeline_multimodal_enabled(&inner.backend.get_preferences()) {
        return Err("selectionVoiceOmniUnsupported".into());
    }
    let preferences = inner.backend.get_preferences();
    ensure_asr_credentials(&preferences).map_err(|message| format!("缺少 ASR 凭据：{message}"))?;
    let active_asr = CredentialsVault::get_active_asr();
    let selection_voice_asr = build_qa_asr_start(inner, &active_asr)
        .await
        .map(|(start, _)| start)
        .map_err(|message| format!("ASR 初始化失败: {message}"))?;
    ensure_microphone_permission(inner)?;

    let consumer = selection_voice_asr.recorder_consumer();
    store_selection_voice_asr_for_session(inner, session_id, selection_voice_asr.active_asr());

    let inner_for_level = Arc::clone(inner);
    let level_handler: Arc<dyn Fn(f32) + Send + Sync> = Arc::new(move |level| {
        if selection_voice_recording_can_continue(&inner_for_level, session_id) {
            emit_capsule(
                &inner_for_level,
                CapsuleState::Recording,
                level,
                0,
                None,
                None,
            );
        }
    });

    let microphone_device_name = selected_microphone_device_name(inner);
    stop_microphone_preview_monitor(inner, "selection-voice recorder");
    acquire_recording_mute(inner, "selection-voice").await;
    if !selection_voice_recording_can_continue(inner, session_id) {
        cancel_selection_voice_asr_for_session(inner, session_id);
        release_recording_mute(inner, "selection-voice");
        return Ok(());
    }
    match Recorder::start(microphone_device_name, consumer, level_handler, None) {
        Ok((recorder, runtime_errors, archive_active)) => {
            if !selection_voice_recording_can_continue(inner, session_id) {
                drop(recorder);
                cancel_selection_voice_asr_for_session(inner, session_id);
                release_recording_mute(inner, "selection-voice");
                return Ok(());
            }
            inner
                .audio_archive_active
                .store(archive_active, std::sync::atomic::Ordering::Relaxed);
            store_selection_voice_recorder_for_session(inner, session_id, recorder);
            spawn_selection_voice_recorder_error_monitor(inner, session_id, runtime_errors);
        }
        Err(error) => {
            cancel_selection_voice_asr_for_session(inner, session_id);
            release_recording_mute(inner, "selection-voice");
            return Err(error.user_message());
        }
    }

    selection_voice_asr
        .open_streaming_session()
        .await
        .map_err(|error| {
            stop_selection_voice_recorder_for_session(inner, session_id);
            cancel_selection_voice_asr_for_session(inner, session_id);
            format!("ASR 连接失败: {error}")
        })?;
    Ok(())
}

#[cfg(all(not(mobile), target_os = "windows"))]
pub(super) async fn finish_selection_voice_transcript(
    inner: &Arc<Inner>,
    session_id: SessionId,
) -> Result<String, String> {
    stop_selection_voice_recorder_for_session(inner, session_id);
    let asr = take_selection_voice_asr_for_session(inner, session_id)
        .ok_or_else(|| "selectionVoiceAsrUnavailable".to_string())?;
    let transcript = match transcribe_selection_voice_asr(inner, session_id, asr).await {
        SelectionVoiceTranscribeOutcome::Done(result) => result?.text,
        SelectionVoiceTranscribeOutcome::Cancelled => {
            return Err("selectionVoiceCancelled".into());
        }
    };
    Ok(transcript)
}
