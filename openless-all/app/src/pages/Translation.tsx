// Translation.tsx — 独立的"翻译"页，从 Settings → 录音 中拆出来。
// 用户在这里：
//   - 勾选自己的工作语言（多选，用作 LLM polish/translate prompt 的前提）
//   - 选一个翻译目标语言（单选；选"不启用"则 Shift 不触发翻译）
//   - 看完整使用说明（怎么触发、按钮位置、胶囊显示）

import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, PageHeader } from './_atoms';
import { SUPPORTED_LANGUAGES } from '../lib/types';
import { useHotkeySettings } from '../state/HotkeySettingsContext';
import { formatComboLabel } from '../lib/hotkey';
import { ShortcutRecorder } from '../components/ShortcutRecorder';
import { setTranslationHotkey } from '../lib/ipc';
import type { UserPreferences, ShortcutBinding } from '../lib/types';

type SaveState = 'idle' | 'saving' | 'saved' | 'failed';

export function Translation() {
  const { t } = useTranslation();
  const { prefs, loading, error, refresh, updatePrefs: savePrefs } = useHotkeySettings();
  const [saveState, setSaveState] = useState<SaveState>('idle');
  const [saveMessage, setSaveMessage] = useState('');
  const statusTimer = useRef<number | null>(null);

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
    showSaveStatus('saving', t('common.saving'));
    try {
      await savePrefs(resolveNext);
      showSaveStatus('saved', t('common.saved'), true);
    } catch (error) {
      console.error('[translation] failed to save preferences', error);
      showSaveStatus('failed', failureMessage);
      await refresh().catch(refreshError => {
        console.warn('[translation] failed to refresh preferences after save error', refreshError);
      });
    }
  };

  if (!prefs) {
    return (
      <>
        <PageHeader
          kicker={t('translation.kicker')}
          title={t('translation.title')}
          desc={t('translation.desc')}
        />
        <Card>
          {error ? (
            <div role="alert" style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
              <div style={{ fontSize: 12, color: 'var(--ol-red, #ef4444)', lineHeight: 1.5 }}>
                {t('common.settingsLoadFailed')}：{error}
              </div>
              <button
                type="button"
                onClick={() => { void refresh(); }}
                disabled={loading}
                style={{
                  alignSelf: 'flex-start',
                  padding: '6px 12px',
                  borderRadius: 999,
                  border: 0,
                  background: 'var(--ol-blue)',
                  color: '#fff',
                  fontSize: 12,
                  fontWeight: 600,
                  cursor: loading ? 'not-allowed' : 'default',
                  opacity: loading ? 0.64 : 1,
                }}
              >
                {loading ? t('common.loading') : t('common.retry')}
              </button>
            </div>
          ) : (
            <div style={{ fontSize: 12, color: 'var(--ol-ink-4)' }}>{t('common.loading')}</div>
          )}
        </Card>
      </>
    );
  }

  const onWorkingLanguagesChange = (workingLanguages: string[]) => {
    void persistPrefs(
      current => ({ ...current, workingLanguages }),
      t('translation.save.workingFailed'),
    );
  };
  const toggleWorkingLanguage = (lang: string) => {
    const next = prefs.workingLanguages.includes(lang)
      ? prefs.workingLanguages.filter(l => l !== lang)
      : [...prefs.workingLanguages, lang];
    onWorkingLanguagesChange(next);
  };
  const onTargetChange = (translationTargetLanguage: string) => {
    void persistPrefs(
      current => ({ ...current, translationTargetLanguage }),
      t('translation.save.targetFailed'),
    );
  };
  const onTranslationHotkeySave = async (binding: ShortcutBinding) => {
    showSaveStatus('saving', t('common.saving'));
    try {
      await setTranslationHotkey(binding);
    } catch (error) {
      console.error('[translation] failed to register translation hotkey', error);
      showSaveStatus('failed', t('translation.save.hotkeyRegisterFailed'));
      return;
    }
    await persistPrefs(
      current => ({ ...current, translationHotkey: binding }),
      t('translation.save.hotkeySaveFailed'),
    );
  };

  const triggerLabel = formatComboLabel(prefs.dictationHotkey);
  const translationHotkeyLabel = formatComboLabel(prefs.translationHotkey);
  const enabled = prefs.translationTargetLanguage.trim() !== '';

  return (
    <>
      <PageHeader
        kicker={t('translation.kicker')}
        title={t('translation.title')}
        desc={t('translation.desc')}
      />

      <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
        {error && (
          <div
            role="alert"
            style={{
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'space-between',
              gap: 10,
              padding: '8px 12px',
              borderRadius: 10,
              border: '0.5px solid rgba(239,68,68,0.22)',
              background: 'rgba(239,68,68,0.07)',
              color: 'var(--ol-red, #ef4444)',
              fontSize: 11.5,
              lineHeight: 1.5,
            }}
          >
            <span>{t('common.settingsLoadFailed')}：{error}</span>
            <button
              type="button"
              onClick={() => { void refresh(); }}
              disabled={loading}
              style={{
                flex: '0 0 auto',
                border: 0,
                borderRadius: 999,
                background: 'rgba(239,68,68,0.12)',
                color: 'inherit',
                padding: '4px 10px',
                fontSize: 11,
                fontWeight: 600,
                cursor: loading ? 'not-allowed' : 'default',
                opacity: loading ? 0.64 : 1,
              }}
            >
              {loading ? t('common.loading') : t('common.retry')}
            </button>
          </div>
        )}

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

        {/* 1. 工作语言 */}
        <Card>
          <div style={{ fontSize: 13, fontWeight: 600, marginBottom: 4 }}>{t('translation.working.title')}</div>
          <div style={{ fontSize: 11.5, color: 'var(--ol-ink-4)', marginBottom: 12, lineHeight: 1.55 }}>
            {t('translation.working.desc')}
          </div>
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
            {SUPPORTED_LANGUAGES.map(lang => {
              const checked = prefs.workingLanguages.includes(lang);
              return (
                <button
                  key={lang}
                  onClick={() => toggleWorkingLanguage(lang)}
                  style={{
                    padding: '6px 12px',
                    fontSize: 12.5,
                    fontWeight: checked ? 600 : 500,
                    border: 0,
                    borderRadius: 999,
                    background: checked ? 'var(--ol-blue)' : 'rgba(0,0,0,0.05)',
                    color: checked ? '#fff' : 'var(--ol-ink-2)',
                    cursor: 'default',
                    fontFamily: 'inherit',
                    transition: 'background 0.12s ease-out, color 0.12s ease-out',
                  }}
                >
                  {lang}
                </button>
              );
            })}
          </div>
        </Card>

        {/* 2. 翻译目标语言 */}
        <Card>
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 4 }}>
            <div style={{ fontSize: 13, fontWeight: 600 }}>{t('translation.target.title')}</div>
            <span
              style={{
                padding: '2px 8px',
                fontSize: 10.5,
                fontWeight: 600,
                letterSpacing: '0.04em',
                borderRadius: 999,
                background: enabled ? 'rgba(37,99,235,0.10)' : 'rgba(0,0,0,0.05)',
                color: enabled ? 'var(--ol-blue)' : 'var(--ol-ink-4)',
                textTransform: 'uppercase',
              }}
            >
              {enabled ? t('translation.statusEnabled') : t('translation.statusDisabled')}
            </span>
          </div>
          <div style={{ fontSize: 11.5, color: 'var(--ol-ink-4)', marginBottom: 12, lineHeight: 1.55 }}>
            {t('translation.target.desc')}
          </div>
          <select
            value={prefs.translationTargetLanguage}
            onChange={e => onTargetChange(e.target.value)}
            style={{
              width: '100%',
              maxWidth: 360,
              height: 32,
              padding: '0 10px',
              fontSize: 13,
              border: '0.5px solid var(--ol-line-strong)',
              borderRadius: 8,
              background: '#fff',
              color: 'var(--ol-ink)',
              fontFamily: 'inherit',
              cursor: 'default',
            }}
          >
            <option value="">{t('translation.target.disabled')}</option>
            {SUPPORTED_LANGUAGES.map(lang => (
              <option key={lang} value={lang}>{lang}</option>
            ))}
          </select>
        </Card>

        <Card>
          <div style={{ fontSize: 13, fontWeight: 600, marginBottom: 4 }}>{t('translation.hotkey.title', 'Translation shortcut')}</div>
          <div style={{ fontSize: 11.5, color: 'var(--ol-ink-4)', marginBottom: 12, lineHeight: 1.55 }}>
            {t('translation.hotkey.desc', 'Press this during recording to switch the current dictation into translation mode.')}
          </div>
          <ShortcutRecorder
            value={prefs.translationHotkey}
            onSave={onTranslationHotkeySave}
          />
        </Card>

        {/* 3. 使用方法 */}
        <Card>
          <div style={{ fontSize: 13, fontWeight: 600, marginBottom: 10 }}>{t('translation.howto.title')}</div>
          <ol style={{ margin: 0, paddingLeft: 18, fontSize: 12.5, color: 'var(--ol-ink-2)', lineHeight: 1.7 }}>
            <li>{t('translation.howto.step1', { trigger: triggerLabel })}</li>
            <li>{t('translation.howto.step2', { trigger: triggerLabel })}</li>
            <li>{t('translation.howto.step3', { shortcut: translationHotkeyLabel })}</li>
            <li>{t('translation.howto.step4')}</li>
            <li>{t('translation.howto.step5')}</li>
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
            <div style={{ fontWeight: 600, color: 'var(--ol-blue)', marginBottom: 4 }}>{t('translation.howto.indicatorTitle')}</div>
            {t('translation.howto.indicatorDesc')}
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
            <div style={{ fontWeight: 600, color: 'var(--ol-ink-2)', marginBottom: 4 }}>{t('translation.howto.fallbackTitle')}</div>
            {t('translation.howto.fallbackDesc')}
          </div>
        </Card>
      </div>
    </>
  );
}
