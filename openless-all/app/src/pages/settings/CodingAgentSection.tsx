// 高级 → Less Computer 配置：启用开关、后端（Claude / OpenCode）、模型 / 权限模式 / 工作目录。
// 「按住说话键」在 通用 → 快捷键 里配置（见 ShortcutsSection），这里不再重复。
// 配置经 UserPreferences 持久化；启用后 coordinator 才注册热键。

import { useTranslation } from 'react-i18next'
import { detectOS } from '../../components/WindowChrome'
import type { CodingAgentPermissionMode, CodingAgentProviderId } from '../../lib/types'
import { useHotkeySettings } from '../../state/HotkeySettingsContext'
import { Card } from '../_atoms'
import { SectionDesc, SectionTitle, SettingRow, Toggle, inputStyle } from './shared'

const PERMISSION_MODES: CodingAgentPermissionMode[] = [
  'acceptEdits',
  'plan',
  'default',
  'bypassPermissions',
]

export function CodingAgentSection() {
  const { t } = useTranslation()
  const { prefs, updatePrefs: savePrefs } = useHotkeySettings()
  const os = detectOS()

  if (os === 'win') return null

  if (!prefs) {
    return (
      <Card>
        <div style={{ fontSize: 12, color: 'var(--ol-ink-4)' }}>{t('common.loading')}</div>
      </Card>
    )
  }

  const enabled = prefs.codingAgentEnabled

  return (
    <Card>
      <SectionTitle>{t('settings.codingAgent.title')}</SectionTitle>
      <SectionDesc>{t('settings.codingAgent.desc')}</SectionDesc>

      <SettingRow label={t('settings.codingAgent.enable')} desc={t('settings.codingAgent.hotkeyHint')}>
        <Toggle
          on={enabled}
          onToggle={next => void savePrefs({ ...prefs, codingAgentEnabled: next })}
        />
      </SettingRow>

      {enabled && (
        <>
          {/* 「按住说话键」配置已挪到 通用 → 快捷键，避免和这里重复。本区只留后端/模型等高级项。 */}
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

          <SettingRow label={t('settings.codingAgent.model')} desc={t('settings.codingAgent.modelHint')}>
            <select
              value={prefs.codingAgentModel ?? ''}
              onChange={e => {
                const v = e.target.value
                void savePrefs({ ...prefs, codingAgentModel: v === '' ? null : v })
              }}
              style={{ ...inputStyle, maxWidth: 240, cursor: 'pointer' }}
            >
              <option value="">{t('settings.codingAgent.modelDefault')}</option>
              <option value="haiku">Haiku</option>
              <option value="sonnet">Sonnet</option>
              <option value="opus">Opus</option>
            </select>
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
