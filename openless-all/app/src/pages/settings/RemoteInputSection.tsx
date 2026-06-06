// 远程输入：在局域网用手机/平板浏览器打开一个录音页，语音实时流回电脑，复用
// 电脑现有的「录音→ASR→润色→光标落字」管线。放在「通用」标签页里，做成可折叠组
// （与「启动」一致，默认折叠）：启停开关、监听端口、访问网址（可一键复制，带配对码）、
// 配对码（可重置）、默认录音方式，以及证书/安全提示。

import { useEffect, useState, type CSSProperties } from 'react';
import { useTranslation } from 'react-i18next';
import { useHotkeySettings } from '../../state/HotkeySettingsContext';
import { Collapsible } from '../_atoms';
import { SettingRow, Toggle, inputStyle } from './shared';
import {
  getRemoteInputStatus,
  regenerateRemotePin,
  isTauri,
  type RemoteInputStatus,
} from '../../lib/ipc';

async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // 退路：隐藏 textarea + execCommand，兼容个别不支持 async clipboard 的环境。
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand('copy');
    } catch {
      /* ignore */
    }
    document.body.removeChild(ta);
  }
}

export function RemoteInputSection() {
  const { t } = useTranslation();
  const { prefs, updatePrefs } = useHotkeySettings();
  const [status, setStatus] = useState<RemoteInputStatus | null>(null);
  const [errorPort, setErrorPort] = useState<number | null>(null);
  const [copied, setCopied] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    const refresh = () =>
      getRemoteInputStatus()
        .then((s) => alive && setStatus(s))
        .catch(() => {});
    refresh();
    if (!isTauri) return;
    const unsubs: Array<() => void> = [];
    import('@tauri-apps/api/event').then(({ listen }) => {
      listen('remote-input:running', () => {
        setErrorPort(null);
        refresh();
      }).then((u) => unsubs.push(u));
      listen('remote-input:error', (e) => {
        const p = e.payload as { port?: number } | null;
        if (alive) setErrorPort(p?.port ?? 0);
      }).then((u) => unsubs.push(u));
    });
    return () => {
      alive = false;
      unsubs.forEach((u) => u());
    };
  }, []);

  if (!prefs) return null;
  const enabled = prefs.remoteInputEnabled;
  const mode = prefs.remoteInputDefaultMode ?? 'toggle';

  const doCopy = async (url: string, pin: string) => {
    await copyText(`${url}\n${t('settings.remoteInput.pinLabel')}：${pin}`);
    setCopied(url);
    window.setTimeout(() => setCopied((c) => (c === url ? null : c)), 1500);
  };

  const smallBtn: CSSProperties = {
    padding: '4px 10px',
    borderRadius: 8,
    fontSize: 12,
    cursor: 'pointer',
    border: '0.5px solid var(--ol-line-strong)',
    background: 'var(--ol-surface-2)',
    color: 'var(--ol-ink)',
    flexShrink: 0,
  };

  return (
    <Collapsible title={t('settings.remoteInput.title')}>
      <SettingRow
        label={t('settings.remoteInput.enableLabel')}
        desc={t('settings.remoteInput.enableDesc')}
      >
        <Toggle
          on={enabled}
          onToggle={(v) => updatePrefs({ ...prefs, remoteInputEnabled: v })}
        />
      </SettingRow>

      <SettingRow label={t('settings.remoteInput.portLabel')}>
        <input
          type="number"
          min={1024}
          max={65535}
          style={{ ...inputStyle, maxWidth: 140 }}
          value={prefs.remoteInputPort}
          onChange={(e) =>
            updatePrefs({
              ...prefs,
              remoteInputPort: Math.max(
                1,
                Math.min(65535, Number(e.currentTarget.value) || 0),
              ),
            })
          }
        />
      </SettingRow>

      <SettingRow label={t('settings.remoteInput.defaultModeLabel')}>
        <div style={{ display: 'flex', gap: 6 }}>
          {(['toggle', 'hold'] as const).map((m) => (
            <button
              key={m}
              onClick={() =>
                updatePrefs({ ...prefs, remoteInputDefaultMode: m })
              }
              style={{
                padding: '5px 12px',
                borderRadius: 8,
                fontSize: 12.5,
                cursor: 'pointer',
                border: '0.5px solid var(--ol-line-strong)',
                background:
                  mode === m ? 'var(--ol-blue)' : 'var(--ol-surface-2)',
                color: mode === m ? '#fff' : 'var(--ol-ink)',
              }}
            >
              {t(
                m === 'toggle'
                  ? 'settings.remoteInput.modeToggle'
                  : 'settings.remoteInput.modeHold',
              )}
            </button>
          ))}
        </div>
      </SettingRow>

      {enabled && status?.running && (
        <>
          <SettingRow label={t('settings.remoteInput.urlLabel')}>
            <div
              style={{
                display: 'flex',
                flexDirection: 'column',
                gap: 6,
                minWidth: 0,
              }}
            >
              {(status.urls.length
                ? status.urls
                : [`https://localhost:${status.port}`]
              ).map((u) => (
                <div
                  key={u}
                  style={{ display: 'flex', alignItems: 'center', gap: 8 }}
                >
                  <span
                    style={{
                      fontFamily: 'monospace',
                      fontSize: 12.5,
                      color: 'var(--ol-ink-2)',
                      wordBreak: 'break-all',
                    }}
                  >
                    {u}
                  </span>
                  <button
                    onClick={() => doCopy(u, status.pin)}
                    title={t('settings.remoteInput.urlLabel')}
                    style={smallBtn}
                  >
                    {copied === u ? '✓' : '⧉'}
                  </button>
                </div>
              ))}
            </div>
          </SettingRow>

          <SettingRow label={t('settings.remoteInput.pinLabel')}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
              <code
                style={{ fontSize: 15, letterSpacing: 2, fontWeight: 600 }}
              >
                {status.pin}
              </code>
              <button
                onClick={async () => {
                  await regenerateRemotePin();
                  const s = await getRemoteInputStatus();
                  setStatus(s);
                }}
                style={smallBtn}
              >
                {t('settings.remoteInput.regeneratePin')}
              </button>
            </div>
          </SettingRow>
        </>
      )}

      {enabled && errorPort != null && (
        <div style={{ fontSize: 12, color: '#d9534f', marginTop: 8 }}>
          {t('settings.remoteInput.portInUse', { port: errorPort })}
        </div>
      )}

      <div
        style={{
          fontSize: 11.5,
          color: 'var(--ol-ink-4)',
          marginTop: 10,
          lineHeight: 1.6,
        }}
      >
        {t('settings.remoteInput.securityHint')}
        <br />
        {t('settings.remoteInput.certHint')}
      </div>
    </Collapsible>
  );
}
