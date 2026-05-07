// SelectionAsk.tsx — 独立的"划词追问"页（issue #118 / PR #119 配置 UI 拆分版）。
// 功能：用户在任意 app 选中一段文字 → 按 hotkey → 浮窗弹出 + 进入语音录音 →
// 用户口述提问 → ASR + 选区 + 提问 一起送 LLM → 答案以 markdown 显示在浮窗。
//
// 这一页把原本散在 Settings → 录音 里的两条配置（hotkey 预设 / 保存 Q&A 历史）
// 集中起来 + 加完整使用指南，跟"翻译"页平级。

import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, PageHeader } from './_atoms';
import { useHotkeySettings } from '../state/HotkeySettingsContext';
import { setQaHotkey } from '../lib/ipc';
import { defaultQaShortcut, formatComboLabel } from '../lib/hotkey';
import { ShortcutRecorder } from '../components/ShortcutRecorder';
import type { QaHotkeyBinding, UserPreferences } from '../lib/types';

type SaveState = 'idle' | 'saving' | 'saved' | 'failed';

export function SelectionAsk() {
  const { t } = useTranslation();
  const { prefs, refresh, updatePrefs: savePrefs } = useHotkeySettings();
  const [saveState, setSaveState] = useState<SaveState>('idle');
  const [saveMessage, setSaveMessage] = useState('');
  const statusTimer = useRef<number | null>(null);
  const defaultQaHotkey = defaultQaShortcut();
  const defaultHotkeyLabel = formatComboLabel(defaultQaHotkey);
  const recordHotkeyLabel = prefs ? formatComboLabel(prefs.dictationHotkey) : '快捷键';

  useEffect(() => () => {
    if (statusTimer.current !== null) window.clearTimeout(statusTimer.current);
  }, []);

  const showSaveStatus = (state: SaveState, message: string, temporary = false) => {
    if (statusTimer.current !== null) {
      window.clearTimeout(statusTimer.current);
      statusTimer.current = null;
    }
    setSaveState(state);
    setSaveMessage(message);
    if (temporary) {
      statusTimer.current = window.setTimeout(() => {
        setSaveState('idle');
        setSaveMessage('');
        statusTimer.current = null;
      }, 1600);
    }
  };

  const persistPrefs = async (
    resolveNext: (current: UserPreferences) => UserPreferences,
    failureMessage: string,
  ) => {
    try {
      await savePrefs(resolveNext);
      showSaveStatus('saved', t('common.saved'), true);
      return true;
    } catch (error) {
      console.error('[selection-ask] failed to save preferences', error);
      showSaveStatus('failed', failureMessage);
      await refresh().catch(refreshError => {
        console.warn('[selection-ask] failed to refresh preferences after save error', refreshError);
      });
      return false;
    }
  };

  const saveQaHotkey = async (nextHotkey: QaHotkeyBinding | null) => {
    showSaveStatus('saving', t('common.saving'));
    try {
      await setQaHotkey(nextHotkey);
    } catch (error) {
      console.error('[selection-ask] failed to register QA hotkey', error);
      showSaveStatus('failed', t('selectionAsk.save.hotkeyRegisterFailed'));
      await refresh().catch(refreshError => {
        console.warn('[selection-ask] failed to refresh preferences after hotkey error', refreshError);
      });
      return;
    }
    await persistPrefs(
      current => ({ ...current, qaHotkey: nextHotkey }),
      t('selectionAsk.save.hotkeySaveFailed'),
    );
  };

  if (!prefs) {
    return (
      <>
        <PageHeader
          kicker={t('selectionAsk.kicker')}
          title={t('selectionAsk.title')}
          desc={t('selectionAsk.desc', {
            hotkey: defaultHotkeyLabel,
            recordHotkey: recordHotkeyLabel,
          })}
        />
        <Card>
          <div style={{ fontSize: 12, color: 'var(--ol-ink-4)' }}>{t('common.loading')}</div>
        </Card>
      </>
    );
  }

  const onSaveHistoryChange = (qaSaveHistory: boolean) => {
    showSaveStatus('saving', t('common.saving'));
    void persistPrefs(
      current => ({ ...current, qaSaveHistory }),
      t('selectionAsk.save.historySaveFailed'),
    );
  };

  const enabled = prefs.qaHotkey !== null;
  const currentLabel = prefs.qaHotkey ? formatComboLabel(prefs.qaHotkey) : defaultHotkeyLabel;
  const saving = saveState === 'saving';

  return (
    <>
      <PageHeader
        kicker={t('selectionAsk.kicker')}
        title={t('selectionAsk.title')}
        desc={t('selectionAsk.desc', {
          hotkey: enabled ? currentLabel : defaultHotkeyLabel,
          recordHotkey: recordHotkeyLabel,
        })}
      />

      <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
        {saveState !== 'idle' && (
          <div
            role={saveState === 'failed' ? 'alert' : 'status'}
            style={{
              padding: '8px 12px',
              borderRadius: 10,
              border: saveState === 'failed'
                ? '0.5px solid rgba(239,68,68,0.22)'
                : '0.5px solid rgba(37,99,235,0.16)',
              background: saveState === 'failed' ? 'rgba(239,68,68,0.07)' : 'rgba(37,99,235,0.06)',
              color: saveState === 'failed' ? 'var(--ol-red, #ef4444)' : 'var(--ol-blue)',
              fontSize: 11.5,
              lineHeight: 1.5,
            }}
          >
            {saveMessage}
          </div>
        )}

        {/* 1. 触发快捷键 */}
        <Card>
          <CardHeaderToggle
            title={t('selectionAsk.hotkey.title')}
            checked={enabled}
            disabled={saving}
            onToggle={() => {
              const nextHotkey = enabled ? null : defaultQaHotkey;
              void saveQaHotkey(nextHotkey);
            }}
          />
          <div style={{ fontSize: 11.5, color: 'var(--ol-ink-4)', marginBottom: prefs.qaHotkey ? 12 : 0, lineHeight: 1.55 }}>
            {t('selectionAsk.hotkey.desc', { recordHotkey: recordHotkeyLabel })}
          </div>
          {prefs.qaHotkey && (
            <ShortcutRecorder
              value={prefs.qaHotkey}
              onSave={saveQaHotkey}
            />
          )}
        </Card>

        {/* 2. 历史保存 */}
        <Card>
          <CardHeaderToggle
            title={t('selectionAsk.history.title')}
            checked={prefs.qaSaveHistory}
            disabled={saving}
            onToggle={() => onSaveHistoryChange(!prefs.qaSaveHistory)}
          />
          <div style={{ fontSize: 11.5, color: 'var(--ol-ink-4)', lineHeight: 1.55 }}>
            {t('selectionAsk.history.desc')}
          </div>
        </Card>

        {/* 3. 使用方法 */}
        <Card>
          <div style={{ fontSize: 13, fontWeight: 600, marginBottom: 10 }}>{t('selectionAsk.howto.title')}</div>
          <ol style={{ margin: 0, paddingLeft: 18, fontSize: 12.5, color: 'var(--ol-ink-2)', lineHeight: 1.7 }}>
            <li>{t('selectionAsk.howto.step1', { hotkey: enabled ? currentLabel : defaultHotkeyLabel })}</li>
            <li>{t('selectionAsk.howto.step2')}</li>
            <li>{t('selectionAsk.howto.step3', { recordHotkey: recordHotkeyLabel })}</li>
            <li>{t('selectionAsk.howto.step4', { recordHotkey: recordHotkeyLabel })}</li>
            <li>{t('selectionAsk.howto.step5', { hotkey: enabled ? currentLabel : defaultHotkeyLabel })}</li>
          </ol>

          <div
            style={{
              marginTop: 14,
              padding: '10px 12px',
              borderRadius: 10,
              background: 'rgba(37,99,235,0.06)',
              border: '0.5px solid rgba(37,99,235,0.15)',
              fontSize: 11.5,
              color: 'var(--ol-ink-2)',
              lineHeight: 1.55,
            }}
          >
            <div style={{ fontWeight: 600, color: 'var(--ol-blue)', marginBottom: 4 }}>{t('selectionAsk.howto.windowTitle')}</div>
            {t('selectionAsk.howto.windowDesc')}
          </div>

          <div
            style={{
              marginTop: 10,
              padding: '10px 12px',
              borderRadius: 10,
              background: 'rgba(0,0,0,0.04)',
              fontSize: 11.5,
              color: 'var(--ol-ink-3)',
              lineHeight: 1.55,
            }}
          >
            <div style={{ fontWeight: 600, color: 'var(--ol-ink-2)', marginBottom: 4 }}>{t('selectionAsk.howto.privacyTitle')}</div>
            {t('selectionAsk.howto.privacyDesc')}
          </div>
        </Card>
      </div>
    </>
  );
}
// 卡片标题行右侧开关：与 Style 页面顶栏的 36×20 toggle 同款，保持全局视觉一致。
function CardHeaderToggle({
  title,
  checked,
  disabled = false,
  onToggle,
}: {
  title: string;
  checked: boolean;
  disabled?: boolean;
  onToggle: () => void;
}) {
  return (
    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 6 }}>
      <div style={{ fontSize: 13, fontWeight: 600 }}>{title}</div>
      <button
        onClick={onToggle}
        aria-pressed={checked}
        disabled={disabled}
        style={{
          position: 'relative',
          width: 36,
          height: 20,
          borderRadius: 999,
          border: 0,
          background: checked ? 'var(--ol-blue)' : 'rgba(0,0,0,0.15)',
          cursor: disabled ? 'not-allowed' : 'default',
          opacity: disabled ? 0.68 : 1,
          transition: 'background 0.16s var(--ol-motion-quick)',
          padding: 0,
        }}
      >
        <span
          style={{
            position: 'absolute',
            top: 2,
            left: checked ? 18 : 2,
            width: 16,
            height: 16,
            borderRadius: 999,
            background: '#fff',
            boxShadow: '0 1px 2px rgba(0,0,0,.2)',
            transition: 'left .16s var(--ol-motion-spring)',
          }}
        />
      </button>
    </div>
  );
}
