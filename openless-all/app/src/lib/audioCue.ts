// 录音提示音：用 Web Audio API 即时「合成」一段短促上升双音，不打包任何音频文件。
// 提供两个操作：
//   - playRecordStartCue()  播放（按下录音热键、进入 recording 状态时调用）
//   - stopAudioCue()        关闭/停止（离开 recording、或连按热键避免叠音时调用）
//
// 触发点在 capsule 窗口（始终存活、收到 capsule:state 事件）；设置页「试听」也复用同一份。
// 设计原则：任何环境（无 Web Audio、AudioContext 被自动播放策略挂起、单音创建失败）都
// 静默降级，绝不抛错影响录音主流程。

/** 单个正弦音符的合成参数（相对提示音起点）。 */
export interface CueTone {
  /** 频率 (Hz)。 */
  freq: number;
  /** 相对提示音起点的开始时间 (ms)。 */
  startMs: number;
  /** 持续时长 (ms)。 */
  durationMs: number;
  /** 指数包络峰值增益 (0..1)，控制响度。 */
  peakGain: number;
}

// 上升小三度双音 (A5 880Hz → C#6 1108.73Hz)：给「开始录音」一个明确、轻快、不刺耳的听感。
// 两个音轻微交叠，听感连贯成一个「叮咚」而非两声独立 beep。纯数据 → 便于单测。
export function recordStartCueTones(): CueTone[] {
  return [
    { freq: 880, startMs: 0, durationMs: 130, peakGain: 0.16 },
    { freq: 1108.73, startMs: 95, durationMs: 170, peakGain: 0.18 },
  ];
}

/** 提示音总时长 (ms) = 最后一个音的结束时刻。供调用方排期 stop / 试听反馈用。 */
export function cueTotalDurationMs(tones: CueTone[]): number {
  return tones.reduce((max, t) => Math.max(max, t.startMs + t.durationMs), 0);
}

// Safari/WKWebView 旧前缀；用结构化类型而非 any 拿到 webkit 兜底构造器。
type AudioContextCtor = typeof AudioContext;
interface WebkitWindow {
  webkitAudioContext?: AudioContextCtor;
}

// 模块级单例。Tauri 每个窗口是独立 webview = 独立 JS 模块实例，所以 capsule 窗口与
// 设置窗口各自持有一份 ctx / activeVoices，不会互相打架。
let sharedCtx: AudioContext | null = null;
interface ActiveVoice {
  osc: OscillatorNode;
  gain: GainNode;
}
let activeVoices: ActiveVoice[] = [];
// 每次「关闭」或「新一轮播放」自增。suspended 时 play 会等 resume() 再排期，
// 这个代号让挂起的 resume 回调能判断「等待期间是否已被叫停/被新一轮取代」，
// 避免录音已经结束、提示音却姗姗来迟地响起来（冷启动 WebView 上快按热键可复现）。
let playGeneration = 0;

function resolveAudioContextCtor(): AudioContextCtor | null {
  if (typeof window === 'undefined') return null;
  // window.AudioContext 来自全局声明；webkit 前缀单独用结构化类型拿，避免 any。
  const webkit = window as WebkitWindow;
  return window.AudioContext ?? webkit.webkitAudioContext ?? null;
}

function getContext(): AudioContext | null {
  const Ctor = resolveAudioContextCtor();
  if (!Ctor) return null;
  if (!sharedCtx) {
    try {
      sharedCtx = new Ctor();
    } catch {
      sharedCtx = null;
      return null;
    }
  }
  return sharedCtx;
}

// 停掉当前正在发声的节点（不影响 playGeneration —— 仅做去叠音 / 收尾）。
function stopVoices(): void {
  const ctx = sharedCtx;
  const now = ctx?.currentTime ?? 0;
  for (const { osc, gain } of activeVoices) {
    try {
      gain.gain.cancelScheduledValues(now);
      // 指数 ramp 不能到 0，用极小值做近似静音后立即停振。
      gain.gain.setValueAtTime(0.0001, now);
      osc.stop(now + 0.02);
    } catch {
      // 已停止 / 已断开，忽略。
    }
  }
  activeVoices = [];
}

/** 关闭/停止提示音：停掉在播节点，并作废任何还挂在 resume() 上、尚未排期的播放。 */
export function stopAudioCue(): void {
  playGeneration++;
  stopVoices();
}

// 实际排期合成节点。必须在 AudioContext 处于 running（非 suspended）时调用：
// suspended 时 currentTime 冻结在暂停时刻，节点会排到过期时间点 → 不发声还堆积。
function scheduleCueVoices(ctx: AudioContext): void {
  // 连按热键时先停掉上一轮，避免叠音越来越响。用 stopVoices 而非 stopAudioCue：
  // 这里不该作废自己这一轮的 generation。
  stopVoices();

  const base = ctx.currentTime + 0.01;
  for (const tone of recordStartCueTones()) {
    try {
      const osc = ctx.createOscillator();
      const gain = ctx.createGain();
      osc.type = 'sine';
      const t0 = base + tone.startMs / 1000;
      const tEnd = t0 + tone.durationMs / 1000;
      osc.frequency.setValueAtTime(tone.freq, t0);
      // 5ms attack + 指数 release：避免起停的 click 爆音。
      gain.gain.setValueAtTime(0.0001, t0);
      gain.gain.exponentialRampToValueAtTime(tone.peakGain, t0 + 0.005);
      gain.gain.exponentialRampToValueAtTime(0.0001, tEnd);
      osc.connect(gain).connect(ctx.destination);
      osc.start(t0);
      osc.stop(tEnd + 0.02);

      const voice: ActiveVoice = { osc, gain };
      activeVoices.push(voice);
      osc.onended = () => {
        activeVoices = activeVoices.filter(v => v !== voice);
        try {
          osc.disconnect();
          gain.disconnect();
        } catch {
          // noop
        }
      };
    } catch {
      // 单个音创建/排期失败不影响其余音。
    }
  }
}

/** 播放「开始录音」提示音。无 Web Audio 或被挂起且无法恢复时静默降级。 */
export function playRecordStartCue(): void {
  const ctx = getContext();
  if (!ctx) return;

  // WKWebView / WebView2 的 AudioContext 常处于 suspended：必须先 resume 再排期，
  // 不能在 resume 未完成时就用冻结的 currentTime 排节点。resume() 失败也不抛（无声降级）。
  if (ctx.state === 'suspended') {
    const gen = ++playGeneration;
    ctx
      .resume()
      .then(() => {
        // 等待 resume 期间若已 stopAudioCue（录音结束）或有新一轮播放，本次作废，
        // 否则会出现「录音已停，提示音却晚到」。
        if (gen !== playGeneration) return;
        scheduleCueVoices(ctx);
      })
      .catch(() => undefined);
    return;
  }

  scheduleCueVoices(ctx);
}
