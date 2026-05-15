// Marketplace.tsx — Style Pack Marketplace 浏览面板。
//
// Phase A 目标（goal 1.a-e）：
//   (a) 后端验证 — 通过 marketplace_* IPC 跟后端通信
//   (b) 上传与拉取功能 — Install / Upload 按钮
//   (c) 单独弹窗界面 — modal-style detail 卡片
//   (d) 搜索框 — 顶部 input + server-side ?q=
//   (e) 按排名自动推荐 — 默认 sort=popular
//
// 后端 URL 走 prefs.marketplaceBaseUrl，dev 模式默认 http://127.0.0.1:8090；
// 用户在 Settings 填生产 URL 后客户端自动切换。
// dev 上传需要 prefs.marketplaceDevLogin（GitHub login 风格）—— 空时上传按钮 disabled。

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Icon } from '../components/Icon';
import {
  fetchMarketplaceDetail,
  installMarketplacePack,
  likeMarketplacePack,
  listMarketplace,
  listStylePacks,
  uploadMarketplacePack,
} from '../lib/ipc';
import { useHotkeySettings } from '../state/HotkeySettingsContext';
import type { MarketplaceDetail, MarketplaceListItem, StylePack } from '../lib/types';
import { Btn, Card, PageHeader, Pill } from './_atoms';

type SortMode = 'popular' | 'new';

export function Marketplace() {
  const { t } = useTranslation();
  const { prefs } = useHotkeySettings();

  const [items, setItems] = useState<MarketplaceListItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [query, setQuery] = useState('');
  const [debouncedQuery, setDebouncedQuery] = useState('');
  const [sort, setSort] = useState<SortMode>('popular');
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detail, setDetail] = useState<MarketplaceDetail | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [actionMsg, setActionMsg] = useState<{ kind: 'ok' | 'err'; text: string } | null>(null);

  const [showUpload, setShowUpload] = useState(false);
  const [localPacks, setLocalPacks] = useState<StylePack[]>([]);
  const canUpload = (prefs?.marketplaceDevLogin ?? '').trim().length > 0;

  // search 防抖 300ms
  useEffect(() => {
    const id = window.setTimeout(() => setDebouncedQuery(query), 300);
    return () => window.clearTimeout(id);
  }, [query]);

  // 单调递增 seq 防 stale 响应覆盖：用户快速改 query / 切换 pack 时旧请求 response
  // 可能晚于新请求到达，比较 seq 丢弃过期结果。
  const reqSeqRef = useRef(0);
  const detailSeqRef = useRef(0);
  const refresh = useCallback(async () => {
    const seq = ++reqSeqRef.current;
    setLoading(true);
    setLoadError(null);
    try {
      const list = await listMarketplace({ query: debouncedQuery, sort, limit: 50 });
      if (seq !== reqSeqRef.current) return; // stale response
      setItems(list);
    } catch (error) {
      if (seq !== reqSeqRef.current) return;
      console.error('[marketplace] list failed', error);
      setLoadError(errorMessage(error));
    } finally {
      if (seq === reqSeqRef.current) setLoading(false);
    }
  }, [debouncedQuery, sort]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const openDetail = async (id: string) => {
    const seq = ++detailSeqRef.current;
    setSelectedId(id);
    setDetail(null);
    setDetailLoading(true);
    try {
      const d = await fetchMarketplaceDetail(id);
      if (seq !== detailSeqRef.current) return; // stale: 用户已切到另一个 pack
      setDetail(d);
    } catch (error) {
      if (seq !== detailSeqRef.current) return;
      console.error('[marketplace] detail failed', error);
      setActionMsg({ kind: 'err', text: t('marketplace.errors.detail', { err: errorMessage(error) }) });
      setSelectedId(null);
    } finally {
      if (seq === detailSeqRef.current) setDetailLoading(false);
    }
  };

  const onInstall = async () => {
    if (!detail) return;
    try {
      await installMarketplacePack(detail.id);
      setActionMsg({ kind: 'ok', text: t('marketplace.installed', { name: detail.name }) });
      setSelectedId(null);
    } catch (error) {
      setActionMsg({ kind: 'err', text: t('marketplace.errors.install', { err: errorMessage(error) }) });
    }
  };

  const onLike = async () => {
    if (!detail) return;
    try {
      const r = await likeMarketplacePack(detail.id);
      setDetail({ ...detail, likeCount: r.likeCount });
      setItems(prev => prev.map(p => (p.id === detail.id ? { ...p, likeCount: r.likeCount } : p)));
    } catch (error) {
      setActionMsg({ kind: 'err', text: t('marketplace.errors.like', { err: errorMessage(error) }) });
    }
  };

  const openUploadPicker = async () => {
    try {
      const packs = await listStylePacks();
      setLocalPacks(packs);
      setShowUpload(true);
    } catch (error) {
      setActionMsg({ kind: 'err', text: t('marketplace.errors.loadLocal', { err: errorMessage(error) }) });
    }
  };

  const onUpload = async (packId: string) => {
    try {
      await uploadMarketplacePack(packId);
      setActionMsg({ kind: 'ok', text: t('marketplace.uploaded') });
      setShowUpload(false);
    } catch (error) {
      setActionMsg({ kind: 'err', text: t('marketplace.errors.upload', { err: errorMessage(error) }) });
    }
  };

  const sortPills = useMemo<Array<{ id: SortMode; label: string }>>(
    () => [
      { id: 'popular', label: t('marketplace.sortPopular') },
      { id: 'new', label: t('marketplace.sortNew') },
    ],
    [t],
  );

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', minHeight: 0 }}>
      <PageHeader
        kicker={t('marketplace.kicker')}
        title={t('marketplace.title')}
        desc={t('marketplace.desc')}
        right={
          <div style={{ display: 'flex', gap: 8 }}>
            <Btn icon="refresh" variant="ghost" size="sm" onClick={() => void refresh()}>
              {t('common.refresh')}
            </Btn>
            <span title={canUpload ? '' : t('marketplace.uploadDisabledHint')}>
              <Btn
                icon="cloud"
                variant="blue"
                size="sm"
                onClick={() => void openUploadPicker()}
                disabled={!canUpload}
              >
                {t('marketplace.uploadBtn')}
              </Btn>
            </span>
          </div>
        }
      />

      {/* 顶部搜索 + 排序 */}
      <div
        style={{
          display: 'flex',
          gap: 10,
          alignItems: 'center',
          padding: '4px 0 14px',
        }}
      >
        <div
          style={{
            flex: 1,
            display: 'flex',
            alignItems: 'center',
            gap: 6,
            padding: '6px 10px',
            border: '0.5px solid var(--ol-line-strong)',
            borderRadius: 10,
            background: 'var(--ol-surface)',
          }}
        >
          <Icon name="search" size={14} stroke="var(--ol-ink-3)" />
          <input
            type="search"
            placeholder={t('marketplace.searchPlaceholder')}
            value={query}
            onChange={e => setQuery(e.target.value)}
            style={{
              flex: 1,
              outline: 'none',
              border: 0,
              background: 'transparent',
              fontSize: 13,
              color: 'var(--ol-ink-1)',
            }}
          />
        </div>
        <div style={{ display: 'flex', gap: 4 }}>
          {sortPills.map(p => (
            <button
              key={p.id}
              onClick={() => setSort(p.id)}
              style={{
                padding: '6px 10px',
                fontSize: 12,
                border: '0.5px solid var(--ol-line-strong)',
                borderRadius: 8,
                cursor: 'pointer',
                background: sort === p.id ? 'var(--ol-blue-soft)' : 'var(--ol-surface)',
                color: sort === p.id ? 'var(--ol-blue)' : 'var(--ol-ink-2)',
              }}
            >
              {p.label}
            </button>
          ))}
        </div>
      </div>

      {actionMsg && (
        <div
          style={{
            marginBottom: 12,
            padding: '8px 12px',
            borderRadius: 8,
            fontSize: 12,
            background: actionMsg.kind === 'ok' ? 'var(--ol-blue-soft)' : 'rgba(220,38,38,0.08)',
            color: actionMsg.kind === 'ok' ? 'var(--ol-blue)' : 'var(--ol-err)',
            border: '0.5px solid',
            borderColor: actionMsg.kind === 'ok' ? 'var(--ol-blue)' : 'var(--ol-err)',
          }}
        >
          {actionMsg.text}
        </div>
      )}

      {loadError && (
        <Card padding={16} style={{ marginBottom: 12, borderColor: 'var(--ol-err)' }}>
          <div style={{ fontSize: 12, color: 'var(--ol-err)' }}>
            {t('marketplace.loadFailed', { err: loadError })}
          </div>
        </Card>
      )}

      {/* 卡片列表 */}
      <div style={{ flex: 1, overflow: 'auto' }} className="ol-thinscroll">
        {loading ? (
          <div style={{ padding: 32, textAlign: 'center', color: 'var(--ol-ink-4)', fontSize: 13 }}>
            {t('common.loading')}
          </div>
        ) : items.length === 0 ? (
          <Card padding={28} style={{ textAlign: 'center' }}>
            <div style={{ fontSize: 13, color: 'var(--ol-ink-3)', marginBottom: 6 }}>
              {t('marketplace.empty')}
            </div>
            <div style={{ fontSize: 11, color: 'var(--ol-ink-4)' }}>
              {t('marketplace.emptyHint')}
            </div>
          </Card>
        ) : (
          <div
            style={{
              display: 'grid',
              gridTemplateColumns: 'repeat(auto-fill, minmax(260px, 1fr))',
              gap: 12,
            }}
          >
            {items.map(p => (
              <button
                key={p.id}
                onClick={() => void openDetail(p.id)}
                style={{
                  textAlign: 'left',
                  padding: 14,
                  borderRadius: 12,
                  border: '0.5px solid var(--ol-line-strong)',
                  background: 'var(--ol-surface)',
                  cursor: 'pointer',
                  display: 'flex',
                  flexDirection: 'column',
                  gap: 6,
                }}
              >
                <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between', gap: 6 }}>
                  <span style={{ fontSize: 14, fontWeight: 600, color: 'var(--ol-ink-1)' }}>{p.name}</span>
                  <span style={{ fontSize: 10, color: 'var(--ol-ink-4)', fontFamily: 'var(--ol-font-mono)' }}>
                    v{p.version}
                  </span>
                </div>
                <div
                  style={{
                    fontSize: 12,
                    color: 'var(--ol-ink-3)',
                    lineHeight: 1.5,
                    display: '-webkit-box',
                    WebkitLineClamp: 2,
                    WebkitBoxOrient: 'vertical',
                    overflow: 'hidden',
                    minHeight: 36,
                  }}
                >
                  {p.description || t('marketplace.noDescription')}
                </div>
                <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', marginTop: 2 }}>
                  <Pill size="sm" tone="outline">{p.baseMode}</Pill>
                  {p.tags.slice(0, 2).map(tag => (
                    <Pill key={tag} size="sm" tone="default">{tag}</Pill>
                  ))}
                </div>
                <div
                  style={{
                    display: 'flex',
                    justifyContent: 'space-between',
                    fontSize: 11,
                    color: 'var(--ol-ink-4)',
                    marginTop: 4,
                  }}
                >
                  <span>by {p.authorLogin}</span>
                  <span>
                    ❤ {p.likeCount} · ↓ {p.downloadCount}
                  </span>
                </div>
              </button>
            ))}
          </div>
        )}
      </div>

      {/* 详情弹窗 */}
      {selectedId && (
        <Modal onClose={() => setSelectedId(null)}>
          {detailLoading || !detail ? (
            <div style={{ padding: 32, textAlign: 'center', color: 'var(--ol-ink-4)', fontSize: 13 }}>
              {t('common.loading')}
            </div>
          ) : (
            <>
              <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, marginBottom: 6 }}>
                <h2 style={{ margin: 0, fontSize: 18, fontWeight: 650 }}>{detail.name}</h2>
                <Pill size="sm" tone="outline">{detail.baseMode}</Pill>
                <span style={{ fontSize: 11, color: 'var(--ol-ink-4)', fontFamily: 'var(--ol-font-mono)' }}>
                  v{detail.version}
                </span>
              </div>
              <div style={{ fontSize: 11, color: 'var(--ol-ink-4)', marginBottom: 12 }}>
                by {detail.authorLogin} · ❤ {detail.likeCount} · ↓ {detail.downloadCount}
              </div>
              {detail.description && (
                <div style={{ fontSize: 13, color: 'var(--ol-ink-2)', lineHeight: 1.6, marginBottom: 14 }}>
                  {detail.description}
                </div>
              )}
              <div
                style={{
                  padding: 12,
                  border: '0.5px solid var(--ol-line)',
                  borderRadius: 10,
                  background: 'var(--ol-surface-2)',
                  marginBottom: 14,
                  maxHeight: 280,
                  overflow: 'auto',
                  fontSize: 12,
                  fontFamily: 'var(--ol-font-mono)',
                  whiteSpace: 'pre-wrap',
                  color: 'var(--ol-ink-2)',
                }}
              >
                {detail.prompt}
              </div>
              <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
                <Btn variant="ghost" size="sm" onClick={() => void onLike()}>
                  ❤ {t('marketplace.likeBtn')}
                </Btn>
                <Btn variant="ghost" size="sm" onClick={() => setSelectedId(null)}>
                  {t('common.cancel')}
                </Btn>
                <Btn variant="blue" size="sm" onClick={() => void onInstall()}>
                  {t('marketplace.installBtn')}
                </Btn>
              </div>
            </>
          )}
        </Modal>
      )}

      {/* 上传选包器 */}
      {showUpload && (
        <Modal onClose={() => setShowUpload(false)}>
          <h2 style={{ margin: '0 0 12px', fontSize: 16, fontWeight: 650 }}>{t('marketplace.uploadTitle')}</h2>
          <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginBottom: 12 }}>
            {t('marketplace.uploadHint', { login: prefs?.marketplaceDevLogin ?? '' })}
          </div>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 8, maxHeight: 360, overflow: 'auto' }}>
            {localPacks.length === 0 ? (
              <div style={{ fontSize: 12, color: 'var(--ol-ink-4)', textAlign: 'center', padding: 20 }}>
                {t('marketplace.uploadNoLocal')}
              </div>
            ) : (
              localPacks.map(p => (
                <button
                  key={p.id}
                  onClick={() => void onUpload(p.id)}
                  style={{
                    textAlign: 'left',
                    padding: 10,
                    border: '0.5px solid var(--ol-line-strong)',
                    borderRadius: 8,
                    background: 'var(--ol-surface)',
                    cursor: 'pointer',
                  }}
                >
                  <div style={{ fontSize: 13, fontWeight: 600 }}>{p.name}</div>
                  <div style={{ fontSize: 11, color: 'var(--ol-ink-4)', marginTop: 2 }}>
                    {p.description || t('marketplace.noDescription')}
                  </div>
                </button>
              ))
            )}
          </div>
          <div style={{ display: 'flex', justifyContent: 'flex-end', marginTop: 14 }}>
            <Btn variant="ghost" size="sm" onClick={() => setShowUpload(false)}>
              {t('common.cancel')}
            </Btn>
          </div>
        </Modal>
      )}
    </div>
  );
}

function Modal({ children, onClose }: { children: React.ReactNode; onClose: () => void }) {
  return (
    <div
      onClick={onClose}
      style={{
        position: 'fixed',
        inset: 0,
        background: 'rgba(0,0,0,0.22)',
        display: 'grid',
        placeItems: 'center',
        zIndex: 50,
        padding: 20,
      }}
    >
      <div
        onClick={e => e.stopPropagation()}
        style={{
          width: 'min(560px, 100%)',
          maxHeight: '85vh',
          overflow: 'auto',
          borderRadius: 16,
          background: 'var(--ol-surface)',
          border: '0.5px solid var(--ol-line-strong)',
          boxShadow: '0 18px 42px rgba(0,0,0,0.18)',
          padding: 22,
        }}
      >
        {children}
      </div>
    </div>
  );
}

function errorMessage(error: unknown): string {
  if (typeof error === 'string') return error;
  if (error instanceof Error) return error.message;
  return String(error);
}
