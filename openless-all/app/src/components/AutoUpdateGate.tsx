// 主窗口启动 + 后台每 60 分钟自动调一次 plugin-updater check。
// 受 prefs.autoUpdateCheck 开关控制；关闭时只走 Settings → 关于 的手动按钮。
// 找到新版本时直接挂 UpdateDialog；不弹自定义通知，沿用既有 dialog 视觉。

import { useEffect } from 'react';
import { isDialogStatus, UpdateDialog, useAutoUpdate } from './AutoUpdate';
import { useHotkeySettings } from '../state/HotkeySettingsContext';

const AUTO_CHECK_INTERVAL_MS = 60 * 60 * 1000;
const STARTUP_DELAY_MS = 4_000;

export function AutoUpdateGate() {
  const { prefs } = useHotkeySettings();
  const u = useAutoUpdate();
  const enabled = prefs?.autoUpdateCheck ?? true;

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;

    const tick = () => {
      if (cancelled) return;
      if (u.checking || u.busy || isDialogStatus(u.status)) return;
      void u.checkForUpdates().catch(error => {
        console.warn('[auto-update] background check failed', error);
      });
    };

    const startupTimer = window.setTimeout(tick, STARTUP_DELAY_MS);
    const intervalTimer = window.setInterval(tick, AUTO_CHECK_INTERVAL_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(startupTimer);
      window.clearInterval(intervalTimer);
    };
    // checkForUpdates / status 故意不放依赖：tick 内部已经做了忙碌态短路，
    // 把 hook 返回值塞进依赖会让 interval 在每次 status 变化时重建，反而漏 tick。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [enabled]);

  if (!isDialogStatus(u.status)) return null;
  return (
    <UpdateDialog
      status={u.status}
      version={u.version}
      progress={u.progress}
      downloaded={u.downloaded}
      contentLength={u.contentLength}
      onInstall={u.installUpdate}
      onClose={u.dismissDialog}
    />
  );
}
