// 跨页面共享的格式化工具。把原本散落在 Overview/History/LocalAsr/AutoUpdate/
// settings 各处重复（且行为略有差异）的 clamp / formatTime / formatDuration /
// formatBytes 收口到一处，调用方用参数区分各自需要的分支，行为保持不变。

import type { useTranslation } from 'react-i18next';

type TFn = ReturnType<typeof useTranslation>['t'];

/** 把 n 夹到 [min, max]。 */
export function clamp(n: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, n));
}

/**
 * 把 ISO 时间格式化为简短显示。
 * - 当天：`HH:MM`
 * - 非当天：默认 `M/D`；`withTimeOnOtherDays` 为 true 时为 `M/D HH:MM`。
 * 无效时间原样返回输入串。
 */
export function formatTime(iso: string, opts?: { withTimeOnOtherDays?: boolean }): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return iso;
  const now = new Date();
  const sameDay = d.toDateString() === now.toDateString();
  const pad = (n: number) => String(n).padStart(2, '0');
  if (sameDay) return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  if (opts?.withTimeOnOtherDays) {
    return `${d.getMonth() + 1}/${d.getDate()} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }
  return `${d.getMonth() + 1}/${d.getDate()}`;
}

/**
 * 把毫秒时长格式化。<=0 / null 返回占位符 `—`。
 * <60s 用 i18n `common.durationSeconds`；>=60s 默认用 i18n `common.durationMinutes`，
 * `minutesAsClock` 为 true 时改用 `M:SS` 时钟格式。
 */
export function formatDuration(
  ms: number | null,
  t: TFn,
  opts?: { minutesAsClock?: boolean }
): string {
  if (ms == null || ms <= 0) return '—';
  const sec = ms / 1000;
  if (sec < 60) return t('common.durationSeconds', { value: sec.toFixed(1) });
  if (opts?.minutesAsClock) {
    return `${Math.floor(sec / 60)}:${String(Math.floor(sec % 60)).padStart(2, '0')}`;
  }
  return t('common.durationMinutes', { value: (sec / 60).toFixed(1) });
}

/**
 * 把字节数格式化为人类可读尺寸。
 * - `maxUnit`：封顶单位（默认 GB）；'MB' 时不再升到 GB。
 * - `mbDigits`：MB 档小数位（AutoUpdate 用 1，LocalAsr 用 0）。
 * - `guard`：非有限或 <=0 时返回 `0 B`（AutoUpdate 下载进度需要）。
 */
export function formatBytes(
  n: number,
  opts?: { maxUnit?: 'MB' | 'GB'; mbDigits?: number; guard?: boolean }
): string {
  const maxUnit = opts?.maxUnit ?? 'GB';
  const mbDigits = opts?.mbDigits ?? 1;
  if (opts?.guard && (!Number.isFinite(n) || n <= 0)) return '0 B';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (maxUnit === 'MB' || n < 1024 * 1024 * 1024) {
    return `${(n / 1024 / 1024).toFixed(mbDigits)} MB`;
  }
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}
