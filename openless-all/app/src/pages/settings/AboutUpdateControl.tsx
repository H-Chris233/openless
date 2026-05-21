// 关于面板里嵌入的"检查更新"控件。
// 注：原 Settings 内的 About tab 已并入 SettingsModal 的 AboutMini；这里只是版本号 + 检查按钮。

import { useTranslation } from 'react-i18next';
import { isDialogStatus, UpdateDialog, useAutoUpdate } from '../../components/AutoUpdate';
import { APP_VERSION_LABEL } from '../../lib/appVersion';
import { Btn } from '../_atoms';

export function AboutUpdateControl({ tagline }: { tagline: string }) {
  const { t } = useTranslation();
  const u = useAutoUpdate();
  return (
    <>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginTop: 2 }}>
        <span style={{ fontSize: 12, color: 'var(--ol-ink-3)' }}>{tagline} 路 {APP_VERSION_LABEL}</span>
        <Btn variant="ghost" size="sm" onClick={u.checkForUpdates} disabled={u.checking || u.busy}>
          {u.checking ? t('settings.about.checkingUpdate') : t('settings.about.checkUpdateBtn')}
        </Btn>
      </div>
      {u.status === 'none' && (
        <div style={{ fontSize: 11, color: 'var(--ol-ink-4)', marginTop: 4 }}>
          {t('settings.about.upToDate')}
        </div>
      )}
      {u.status === 'error' && (
        <div style={{ marginTop: 4 }}>
          <div style={{ fontSize: 11, color: 'var(--ol-err)' }}>
            {t('settings.about.updateError')}
          </div>
          {u.errorMessage && (
            <div style={{ fontSize: 10.5, color: 'var(--ol-ink-4)', marginTop: 2, wordBreak: 'break-word' }}>
              {u.errorMessage}
            </div>
          )}
          <Btn variant="ghost" size="sm" onClick={u.checkForUpdates} style={{ marginTop: 4 }}>
            {t('settings.about.retryBtn') ?? t('common.retry') ?? '重试'}
          </Btn>
        </div>
      )}
      {isDialogStatus(u.status) && (
        <UpdateDialog
          status={u.status}
          version={u.version}
          progress={u.progress}
          downloaded={u.downloaded}
          contentLength={u.contentLength}
          onInstall={u.installUpdate}
          onClose={u.dismissDialog}
        />
      )}
    </>
  );
}
