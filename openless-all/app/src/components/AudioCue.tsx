// 录音提示音：监听 capsule:state 事件，在"开始录音"边沿播放合成提示音。
// 独立组件，不依赖胶囊窗口显示——Linux 上胶囊隐藏也能正常工作。
// 全平台通用，在 FloatingShellBody 中渲染。

import { useEffect, useRef } from 'react';
import { isTauri } from '../lib/ipc';
import { playRecordStartCue, primeAudioCue, stopAudioCue } from '../lib/audioCue';
import type { CapsuleState, UserPreferences } from '../lib/types';

interface CapsulePayload {
  state: CapsuleState;
  level?: number;
  message?: string | null;
  insertedChars?: number | null;
  translation?: boolean;
}

export function AudioCueListener() {
  const audioCueEnabledRef = useRef<boolean>(true);
  const prevStateRef = useRef<CapsuleState>('idle' as CapsuleState);

  // 读取设置（默认开启）
  useEffect(() => {
    if (!isTauri) return;
    let cancelled = false;
    (async () => {
      try {
        const { getSettings } = await import('../lib/ipc');
        const prefs = await getSettings();
        if (!cancelled) audioCueEnabledRef.current = prefs.audioCueOnRecord !== false;
      } catch {
        // 读取失败保持默认 true
      }
    })();
    return () => { cancelled = true; };
  }, []);

  // 监听设置变更
  useEffect(() => {
    if (!isTauri) return;
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    (async () => {
      const { listen } = await import('@tauri-apps/api/event');
      listen<UserPreferences>('prefs:changed', (event) => {
        const next = event.payload;
        if (next) audioCueEnabledRef.current = next.audioCueOnRecord !== false;
      }).then(fn => { if (!cancelled) unlisten = fn; }).catch(() => {});
    })();
    return () => { cancelled = true; unlisten?.(); };
  }, []);

  // 预热 AudioContext
  useEffect(() => {
    if (!isTauri) return;
    primeAudioCue();
  }, []);

  // 监听 capsule 状态边沿
  useEffect(() => {
    if (!isTauri) return;
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    (async () => {
      const { listen } = await import('@tauri-apps/api/event');
      listen<CapsulePayload>('capsule:state', (event) => {
        const state = event.payload.state;
        const prev = prevStateRef.current;
        prevStateRef.current = state;
        if (state === 'recording' && prev !== 'recording') {
          if (audioCueEnabledRef.current) playRecordStartCue();
        } else if (state !== 'recording' && prev === 'recording') {
          stopAudioCue();
        }
      }).then(fn => { if (!cancelled) unlisten = fn; }).catch(() => {});
    })();
    return () => { cancelled = true; unlisten?.(); };
  }, []);

  return null;
}
