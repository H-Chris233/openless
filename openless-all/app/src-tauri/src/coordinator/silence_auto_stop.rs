//! Toggle 模式「说完自动停止」的纯逻辑静音检测器（issue #860）。
//!
//! 输入是录音电平流（recorder 的 `level_handler`，约 185 Hz 的 0..1 RMS 电平），
//! 输出是一个**一次性**决策：
//! - 检测到语音后，连续静音达到配置阈值 → `Stop`（自动停止并提交）；
//! - 开始录音后一直没有检测到语音 → `Cancel`（不提交空录音）。
//!
//! 不依赖 Tauri / 音频 / 外部时间源，`on_level` 由调用方传入 `now`，方便单测。
//! 决策产生后即锁存，后续帧一律返回 `None`，不会重复触发。

use std::time::{Duration, Instant};

/// 静音检测的一次性决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilenceDecision {
    /// 检测到语音后连续静音达到阈值 → 停止并提交。
    Stop,
    /// 开始录音后一直没有检测到语音 → 取消，不提交空录音。
    Cancel,
}

/// 把一次电平判定为「语音」所需的最低电平。`level = output_rms × 4`（clamp 0..1）：
/// 麦克风底噪实测约 0.001–0.005，正常语音约 0.05+，取 0.02 作分界。
pub const SPEECH_LEVEL_THRESHOLD: f32 = 0.02;

/// 判定为语音所需的**连续**语音块数（每块约 5 ms），滤掉键盘敲击 / 一声咳嗽的毛刺。
pub const MIN_SPEECH_BLOCKS: u32 = 3;

/// 开始录音后一直没检测到语音的取消时限。
pub const NO_SPEECH_CANCEL: Duration = Duration::from_secs(10);

pub struct SilenceAutoStop {
    /// 语音后的连续静音阈值。
    silence_after_speech: Duration,
    speech_detected: bool,
    consecutive_speech_blocks: u32,
    last_speech_at: Option<Instant>,
    started_at: Instant,
    decided: bool,
}

impl SilenceAutoStop {
    pub fn new(silence_after_speech: Duration, started_at: Instant) -> Self {
        Self {
            silence_after_speech,
            speech_detected: false,
            consecutive_speech_blocks: 0,
            last_speech_at: None,
            started_at,
            decided: false,
        }
    }

    /// 喂入一帧电平。返回非 `None` 表示本次会话已产生决策，之后不会再返回任何值。
    pub fn on_level(&mut self, level: f32, now: Instant) -> Option<SilenceDecision> {
        if self.decided {
            return None;
        }
        if level >= SPEECH_LEVEL_THRESHOLD {
            self.consecutive_speech_blocks += 1;
            if self.consecutive_speech_blocks >= MIN_SPEECH_BLOCKS {
                self.speech_detected = true;
                self.last_speech_at = Some(now);
            }
        } else {
            self.consecutive_speech_blocks = 0;
        }

        if self.speech_detected {
            if let Some(last) = self.last_speech_at {
                if now.duration_since(last) >= self.silence_after_speech {
                    self.decided = true;
                    return Some(SilenceDecision::Stop);
                }
            }
        } else if now.duration_since(self.started_at) >= NO_SPEECH_CANCEL {
            self.decided = true;
            return Some(SilenceDecision::Cancel);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    fn feed_frames(
        detector: &mut SilenceAutoStop,
        frames: impl IntoIterator<Item = (f32, Duration)>,
        base: Instant,
    ) -> Option<SilenceDecision> {
        let mut decision = None;
        for (level, offset) in frames {
            if let Some(d) = detector.on_level(level, base + offset) {
                decision = Some(d);
                break;
            }
        }
        decision
    }

    #[test]
    fn speech_then_silence_reaches_threshold_stops() {
        let base = base();
        let mut detector = SilenceAutoStop::new(Duration::from_secs(3), base);
        // 3 个连续语音块（约 15 ms）确认为语音。
        let decision = feed_frames(
            &mut detector,
            [
                (0.1, Duration::from_millis(10)),
                (0.1, Duration::from_millis(20)),
                (0.1, Duration::from_millis(30)),
                (0.0, Duration::from_secs(4)),
            ],
            base,
        );
        assert_eq!(decision, Some(SilenceDecision::Stop));
    }

    #[test]
    fn silence_short_of_threshold_does_not_stop() {
        let base = base();
        let mut detector = SilenceAutoStop::new(Duration::from_secs(3), base);
        let decision = feed_frames(
            &mut detector,
            [
                (0.1, Duration::from_millis(10)),
                (0.1, Duration::from_millis(20)),
                (0.1, Duration::from_millis(30)),
                (0.0, Duration::from_secs(2)),
            ],
            base,
        );
        assert_eq!(decision, None);
    }

    #[test]
    fn no_speech_at_all_cancels_after_ten_seconds() {
        let base = base();
        let mut detector = SilenceAutoStop::new(Duration::from_secs(3), base);
        let decision = feed_frames(
            &mut detector,
            [
                (0.0, Duration::from_secs(10)),
                (0.0, Duration::from_secs(11)),
            ],
            base,
        );
        assert_eq!(decision, Some(SilenceDecision::Cancel));
    }

    #[test]
    fn short_noise_burst_does_not_count_as_speech() {
        let base = base();
        let mut detector = SilenceAutoStop::new(Duration::from_secs(3), base);
        // 1 块高电平（键盘敲击）不够 MIN_SPEECH_BLOCKS，仍应走 10 秒取消。
        let decision = feed_frames(
            &mut detector,
            [
                (0.5, Duration::from_millis(10)),
                (0.0, Duration::from_millis(20)),
                (0.0, Duration::from_secs(10)),
            ],
            base,
        );
        assert_eq!(decision, Some(SilenceDecision::Cancel));
    }

    #[test]
    fn speech_after_grace_resets_to_silence_threshold() {
        let base = base();
        let mut detector = SilenceAutoStop::new(Duration::from_secs(2), base);
        // 9 秒静音 → 恰好还没触发 10 秒取消；第 9 秒开口，之后 2 秒静音 → Stop。
        let decision = feed_frames(
            &mut detector,
            [
                (0.0, Duration::from_secs(9)),
                (0.1, Duration::from_millis(9100)),
                (0.1, Duration::from_millis(9110)),
                (0.1, Duration::from_millis(9120)),
                (0.0, Duration::from_millis(11200)),
            ],
            base,
        );
        assert_eq!(decision, Some(SilenceDecision::Stop));
    }

    #[test]
    fn decision_is_one_shot() {
        let base = base();
        let mut detector = SilenceAutoStop::new(Duration::from_secs(1), base);
        assert_eq!(
            feed_frames(
                &mut detector,
                [
                    (0.1, Duration::from_millis(10)),
                    (0.1, Duration::from_millis(20)),
                    (0.1, Duration::from_millis(30)),
                    (0.0, Duration::from_secs(2)),
                ],
                base,
            ),
            Some(SilenceDecision::Stop)
        );
        // 决策后继续喂帧不再产生新决策。
        assert_eq!(detector.on_level(0.0, base + Duration::from_secs(30)), None);
        assert_eq!(detector.on_level(0.5, base + Duration::from_secs(31)), None);
    }
}
