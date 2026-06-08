// 关于 → 版本信息 / 检查更新 / 字体大小 / 文档链接。
// 「个性化」原本是独立 tab，但只剩字体大小一项、整页太空，遂并入「关于」。
// 「加入 Beta 渠道」已挪到「高级」页底部（见 BetaChannelSection），这里图标旁
// 只保留查正式版的「检查更新」按钮。

import { useEffect, useRef, useState, type CSSProperties } from 'react';
import { useTranslation } from 'react-i18next';
import { Icon } from '../../components/Icon';
import { Row } from '../../components/ui/Row';
import { openExternal } from '../../lib/ipc';
import { APP_VERSION_LABEL } from '../../lib/appVersion';
import { Card } from '../_atoms';
import { SectionTitle } from './shared';
import { CheckUpdateButton } from './CheckUpdateButton';

export function AboutSection() {
  const { t } = useTranslation();
  const [qqCopied, setQqCopied] = useState(false);
  const qqCopiedRef = useRef<number | null>(null);

  useEffect(() => () => {
    if (qqCopiedRef.current) clearTimeout(qqCopiedRef.current);
  }, []);

  const copyQq = () => {
    navigator.clipboard?.writeText('1078960553');
    setQqCopied(true);
    if (qqCopiedRef.current) clearTimeout(qqCopiedRef.current);
    qqCopiedRef.current = window.setTimeout(() => setQqCopied(false), 1500);
  };

  return (
    <>
      {/* ─── 版本信息 + 检查更新（正式版）─────────────────────────────── */}
      <Card>
        <div style={{ display: 'flex', alignItems: 'center', gap: 14 }}>
          <img
            src="AppIcon.png"
            alt=""
            style={{ width: 56, height: 56, borderRadius: 13, boxShadow: '0 4px 10px rgba(0,0,0,.10), 0 0 0 0.5px rgba(0,0,0,.06)' }}
          />
          <div style={{ flex: 1, minWidth: 0 }}>
            <div style={{ fontSize: 17, fontWeight: 600 }}>OpenLess</div>
            <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 2 }}>
              {t('modal.about.tagline')} · {APP_VERSION_LABEL}
            </div>
          </div>
          {/* 图标右上方：查正式版的检查更新按钮。Beta 渠道在「高级」页。 */}
          <CheckUpdateButton channel="stable" />
        </div>
      </Card>

      {/* 个性化（字体大小）已按需求移除（页面瘦身）。 */}

      {/* ─── 文档链接 ─────────────────────────────────────────────── */}
      <Card>
        <SectionTitle>{t('settings.about.linksTitle')}</SectionTitle>
        <Row label={t('modal.about.source')}>
          <button style={btnGhost} onClick={() => openExternal('https://github.com/appergb/openless')}>
            GitHub
          </button>
        </Row>
        <Row label={t('modal.about.docs')}>
          <button style={btnGhost} onClick={() => openExternal('https://github.com/appergb/openless#readme')}>
            {t('modal.about.docsBtn')}
          </button>
        </Row>
        <Row label={t('modal.about.feedback')}>
          <button style={btnGhost} onClick={() => openExternal('https://github.com/appergb/openless/issues')}>
            {t('modal.about.feedbackBtn')}
          </button>
        </Row>
        <Row label={t('modal.about.qq')}>
          <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
            <kbd style={{
              padding: '4px 10px', fontSize: 12, fontFamily: 'var(--ol-font-mono)',
              borderRadius: 6, background: 'var(--ol-surface-2)',
              border: '0.5px solid var(--ol-line-strong)',
              boxShadow: '0 1px 0 rgba(0,0,0,0.04)',
              color: 'var(--ol-ink-2)',
            }}>1078960553</kbd>
            <button onClick={copyQq} title={t('modal.about.copyQq')} style={btnGhost}>
              <Icon name="copy" size={14} />
            </button>
            {qqCopied && <span style={{ fontSize: 11, color: 'var(--ol-ok)', whiteSpace: 'nowrap' }}>{t('common.copied')}</span>}
          </div>
        </Row>
      </Card>
    </>
  );
}

const btnGhost: CSSProperties = {
  padding: '5px 10px', fontSize: 12, borderRadius: 6,
  border: '0.5px solid var(--ol-line-strong)',
  background: '#fff', color: 'var(--ol-ink-2)',
  cursor: 'default', fontFamily: 'inherit',
  transition: 'background 0.16s var(--ol-motion-quick), border-color 0.16s var(--ol-motion-quick)',
};
