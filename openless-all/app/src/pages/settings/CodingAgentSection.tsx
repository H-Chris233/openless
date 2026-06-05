// 高级 → 语音 Agent（快速 Agent）配置：启用开关、后端（Claude / OpenCode）、
// 两个全局热键、模型 / 权限模式 / 工作目录。配置经 UserPreferences 持久化；
// 热键的实时注册与触发链路由 coordinator 接线（随后续版本生效）。

import { useTranslation } from 'react-i18next'
import type { CodingAgentPermissionMode, CodingAgentProviderId, ShortcutBinding } from '../../lib/types'
import { useHotkeySettings } from '../../state/HotkeySettingsContext'
import { ShortcutRecorder } from '../../components/ShortcutRecorder'
import { Btn, Card } from '../_atoms'
import { SectionDesc, SectionTitle, SettingRow, Toggle, inputStyle } from './shared'

const PERMISSION_MODES: CodingAgentPermissionMode[] = [
  'acceptEdits',
  'plan',
  'default',
  'bypassPermissions',
]

const DEFAULT_PANEL_HOTKEY: ShortcutBinding = { primary: 'Enter', modifiers: ['cmd', 'shift'] }
const DEFAULT_QUICK_HOTKEY: ShortcutBinding = { primary: 'J', modifiers: ['cmd', 'shift'] }

export function CodingAgentSection() {
  const { t } = useTranslation()
  const { prefs, updatePrefs: savePrefs } = useHotkeySettings()

  if (!prefs) {
    return (
      <Card>
        <div style={{ fontSize: 12, color: 'var(--ol-ink-4)' }}>{t('common.loading')}</div>
      </Card>
    )
  }

  const enabled = prefs.codingAgentEnabled

  const setHotkey = (
    key: 'codingAgentPanelHotkey' | 'codingAgentQuickHotkey',
    binding: ShortcutBinding | null,
  ) => savePrefs({ ...prefs, [key]: binding })

  const hotkeyRow = (
    key: 'codingAgentPanelHotkey' | 'codingAgentQuickHotkey',
    label: string,
    desc: string,
    fallback: ShortcutBinding,
  ) => {
    const value = prefs[key]
    return (
      <SettingRow label={label} desc={desc}>
        {value ? (
          <div style={{ display: 'flex', flexDirection: 'column', gap: 6, width: '100%' }}>
            <ShortcutRecorder value={value} alignRecordButton onSave={b => setHotkey(key, b)} />
            <button
              onClick={() => void setHotkey(key, null)}
              style={{
                alignSelf: 'flex-start',
                fontSize: 11,
                padding: '3px 10px',
                background: 'transparent',
                color: 'var(--ol-ink-4)',
                border: '0.5px solid var(--ol-line-strong)',
                borderRadius: 6,
                fontFamily: 'inherit',
                cursor: 'default',
              }}
            >
              {t('settings.shortcuts.disable', 'Disable')}
            </button>
          </div>
        ) : (
          <Btn variant="blue" size="sm" onClick={() => void setHotkey(key, fallback)}>
            {t('settings.shortcuts.enable', 'Enable')}
          </Btn>
        )}
      </SettingRow>
    )
  }

  return (
    <Card>
      <SectionTitle>{t('settings.codingAgent.title')}</SectionTitle>
      <SectionDesc>{t('settings.codingAgent.desc')}</SectionDesc>

      <SettingRow label={t('settings.codingAgent.enable')} desc={t('settings.codingAgent.comingSoonNote')}>
        <Toggle
          on={enabled}
          onToggle={next => void savePrefs({ ...prefs, codingAgentEnabled: next })}
        />
      </SettingRow>

      {enabled && (
        <>
          <SettingRow label={t('settings.codingAgent.provider')}>
            <select
              value={prefs.codingAgentProvider}
              onChange={e =>
                void savePrefs({
                  ...prefs,
                  codingAgentProvider: e.target.value as CodingAgentProviderId,
                })
              }
              style={{ ...inputStyle, maxWidth: 240, cursor: 'pointer' }}
            >
              <option value="claude-code-cli">Claude Code</option>
              <option value="opencode-cli">{t('settings.codingAgent.providerOpenCodeSoon')}</option>
            </select>
          </SettingRow>

          {hotkeyRow(
            'codingAgentPanelHotkey',
            t('settings.codingAgent.panelHotkey'),
            t('settings.codingAgent.panelHotkeyDesc'),
            DEFAULT_PANEL_HOTKEY,
          )}
          {hotkeyRow(
            'codingAgentQuickHotkey',
            t('settings.codingAgent.quickHotkey'),
            t('settings.codingAgent.quickHotkeyDesc'),
            DEFAULT_QUICK_HOTKEY,
          )}

          <SettingRow label={t('settings.codingConsole.permissionMode')}>
            <select
              value={prefs.codingAgentPermissionMode}
              onChange={e =>
                void savePrefs({
                  ...prefs,
                  codingAgentPermissionMode: e.target.value as CodingAgentPermissionMode,
                })
              }
              style={{ ...inputStyle, maxWidth: 240, cursor: 'pointer' }}
            >
              {PERMISSION_MODES.map(m => (
                <option key={m} value={m}>
                  {t(`settings.codingConsole.mode.${m}`)}
                </option>
              ))}
            </select>
          </SettingRow>

          <SettingRow label={t('settings.codingAgent.model')}>
            <input
              type="text"
              value={prefs.codingAgentModel ?? ''}
              placeholder={t('settings.codingAgent.modelPlaceholder')}
              spellCheck={false}
              onChange={e => {
                const v = e.target.value.trim()
                void savePrefs({ ...prefs, codingAgentModel: v === '' ? null : v })
              }}
              style={inputStyle}
            />
          </SettingRow>

          <SettingRow label={t('settings.codingConsole.workdir')} desc={t('settings.codingConsole.workdirDesc')}>
            <input
              type="text"
              value={prefs.codingAgentWorkdir ?? ''}
              placeholder={t('settings.codingConsole.workdirPlaceholder')}
              spellCheck={false}
              onChange={e => {
                const v = e.target.value.trim()
                void savePrefs({ ...prefs, codingAgentWorkdir: v === '' ? null : v })
              }}
              style={inputStyle}
            />
          </SettingRow>
        </>
      )}
    </Card>
  )
}
