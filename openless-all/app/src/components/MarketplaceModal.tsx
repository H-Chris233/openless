// MarketplaceModal.tsx — 风格市场弹窗。
// 跟 SettingsModal 同款 backdrop + 居中卡片。内容直接复用 <Marketplace />。
// 入口在 Style 页面「风格包」标题右侧，由 Style.tsx 控制 open/close。
// 顶部 pill 显示当前「登录身份」（dev 模式 = marketplaceDevLogin），未填时引导跳 Settings。

import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Icon } from './Icon';
import { Marketplace } from '../pages/Marketplace';
import { useHotkeySettings } from '../state/HotkeySettingsContext';

interface MarketplaceModalProps {
  onClose: () => void;
}

export function MarketplaceModal({ onClose }: MarketplaceModalProps) {
  const { t } = useTranslation();
  const { prefs } = useHotkeySettings();
  const login = (prefs?.marketplaceDevLogin ?? '').trim();
  const loggedIn = login.length > 0;
  // Esc 关闭
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [onClose]);

  return (
    <div
      onClick={onClose}
      style={{
        position: 'absolute', inset: 0,
        background: 'var(--ol-overlay-bg)',
        backdropFilter: 'blur(8px) saturate(140%)',
        WebkitBackdropFilter: 'blur(8px) saturate(140%)',
        display: 'flex', alignItems: 'center', justifyContent: 'center',
        padding: 28,
        zIndex: 50,
        animation: 'ol-modal-backdrop-in 0.18s var(--ol-motion-soft)',
      }}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        style={{
          width: '100%', maxWidth: 1080, height: '100%', maxHeight: 720,
          background: 'var(--ol-style-editor-bg)',
          borderRadius: 24,
          border: '1px solid var(--ol-style-editor-border)',
          boxShadow: 'var(--ol-style-editor-shadow)',
          overflow: 'hidden',
          position: 'relative',
          display: 'flex', flexDirection: 'column',
          animation: 'ol-modal-card-in 0.24s var(--ol-motion-spring)',
        }}
      >
        <div
          style={{
            position: 'absolute', top: 16, left: 24, zIndex: 3,
            display: 'inline-flex', alignItems: 'center', gap: 8,
          }}
        >
          <button
            type="button"
            tabIndex={-1}
            title={loggedIn ? t('marketplace.modal.loggedIn') : t('marketplace.modal.notLoggedIn')}
            style={{
              display: 'inline-flex', alignItems: 'center', gap: 6,
              padding: '4px 10px', borderRadius: 999,
              border: loggedIn ? '1px solid var(--ol-style-subtle-border)' : '1px solid rgba(239,68,68,0.32)',
              background: loggedIn ? 'var(--ol-style-subtle-bg)' : 'rgba(239,68,68,0.10)',
              color: loggedIn ? 'var(--ol-ink-2)' : 'var(--ol-red, #ef4444)',
              fontSize: 11.5, fontWeight: 500,
              cursor: 'default',
              boxShadow: 'var(--ol-style-subtle-shadow)',
              backdropFilter: 'blur(8px) saturate(140%)',
              WebkitBackdropFilter: 'blur(8px) saturate(140%)',
            }}
          >
            <Icon name="user" size={11} />
            <span>{loggedIn ? `@${login}` : t('marketplace.modal.notLoggedInLabel')}</span>
          </button>
        </div>

        <button
          onClick={onClose}
          style={{
            position: 'absolute', top: 16, right: 16, zIndex: 3,
            width: 32, height: 32,
            border: '1px solid var(--ol-style-subtle-border)',
            borderRadius: 8,
            background: 'var(--ol-style-subtle-bg)',
            color: 'var(--ol-ink-2)',
            display: 'inline-flex', alignItems: 'center', justifyContent: 'center',
            cursor: 'default',
            boxShadow: 'var(--ol-style-subtle-shadow)',
            backdropFilter: 'blur(8px) saturate(140%)',
            WebkitBackdropFilter: 'blur(8px) saturate(140%)',
            transition: 'background 0.16s var(--ol-motion-quick), color 0.16s var(--ol-motion-quick)',
          }}
          onMouseEnter={(e) => {
            (e.currentTarget as HTMLButtonElement).style.background = 'var(--ol-control-solid)';
            (e.currentTarget as HTMLButtonElement).style.color = 'var(--ol-ink)';
          }}
          onMouseLeave={(e) => {
            (e.currentTarget as HTMLButtonElement).style.background = 'var(--ol-style-subtle-bg)';
            (e.currentTarget as HTMLButtonElement).style.color = 'var(--ol-ink-2)';
          }}
          aria-label={t('common.close')}
          title={t('common.close')}
        >
          <Icon name="close" size={14} />
        </button>
        {/* paddingTop 64 给 X 按钮留位置 —— PageHeader 的 right 槽（刷新/上传）会下沉到 X 下方 */}
        <div style={{ flex: 1, minHeight: 0, overflow: 'auto', padding: '64px 32px 32px' }}>
          <Marketplace />
        </div>
      </div>
    </div>
  );
}
