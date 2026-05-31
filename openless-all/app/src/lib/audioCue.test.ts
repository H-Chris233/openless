// audioCue 纯函数单测，沿用仓库现有 .test.ts 的轻量自执行断言风格
// （无独立 runner —— 在 tsc 类型检查下编译，必要时可用 tsx 直接跑）。
// 播放/停止依赖 Web Audio 运行时，不在此单测覆盖；这里只钉住可被回归的音符规划。

import { cueTotalDurationMs, recordStartCueTones, type CueTone } from './audioCue';

function assert(cond: boolean, name: string) {
  if (!cond) throw new Error(`assertion failed: ${name}`);
}

function assertEqual<T>(actual: T, expected: T, name: string) {
  if (actual !== expected) {
    throw new Error(`${name}: expected ${String(expected)}, got ${String(actual)}`);
  }
}

{
  const tones = recordStartCueTones();
  assertEqual(tones.length, 2, 'start cue is a two-tone chime');
  assert(
    tones.every(t => t.freq > 0 && t.durationMs > 0),
    'every tone has positive frequency and duration',
  );
  // 指数包络 ramp 不能到 0，峰值必须严格为正，否则 exponentialRampToValueAtTime 抛错。
  assert(
    tones.every(t => t.peakGain > 0 && t.peakGain <= 1),
    'every tone peak gain is within (0, 1]',
  );
  // 第二个音上升（小三度），听感是「叮咚」而非平铺两声。
  assert(tones[1].freq > tones[0].freq, 'second tone rises in pitch');
  // 两音交叠：第二个音在第一个音结束前起，连成一段。
  assert(
    tones[1].startMs < tones[0].startMs + tones[0].durationMs,
    'tones overlap into a single chime',
  );
}

{
  const flat: CueTone[] = [
    { freq: 440, startMs: 0, durationMs: 100, peakGain: 0.2 },
    { freq: 880, startMs: 90, durationMs: 170, peakGain: 0.2 },
  ];
  assertEqual(cueTotalDurationMs(flat), 260, 'total duration is last tone end (90 + 170)');
  assertEqual(cueTotalDurationMs([]), 0, 'empty cue has zero duration');
  assert(cueTotalDurationMs(recordStartCueTones()) > 0, 'start cue has positive total duration');
}

// 静默成功难以与「没跑」区分；直接 tsx 跑时给个明确通过信号。
console.log('[audioCue.test] all assertions passed');
