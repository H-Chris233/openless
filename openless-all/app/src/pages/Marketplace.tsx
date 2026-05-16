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
  marketplaceDelete,
  marketplaceMyLikes,
  marketplaceMyPacks,
  uploadMarketplacePack,
} from '../lib/ipc';
import { useHotkeySettings } from '../state/HotkeySettingsContext';
import type { MarketplaceDetail, MarketplaceListItem, MarketplaceMyPackItem, StylePack } from '../lib/types';
import { Btn, Card, PageHeader, Pill } from './_atoms';

type SortMode = 'popular' | 'new' | 'liked';

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
  // leaving=true 触发右滑出动画；动画跑完再真正 setActionMsg(null) 卸载 DOM。
  const [actionLeaving, setActionLeaving] = useState(false);
  // 自动消失：ok 2.4s、err 4s 后切 leaving；leaving 持续 ~280ms 等动画结束。
  useEffect(() => {
    if (!actionMsg) return;
    setActionLeaving(false);
    const dwellMs = actionMsg.kind === 'ok' ? 2400 : 4000;
    const exitDelay = 280;
    const leaveId = window.setTimeout(() => setActionLeaving(true), dwellMs);
    const dropId = window.setTimeout(() => setActionMsg(null), dwellMs + exitDelay);
    return () => {
      window.clearTimeout(leaveId);
      window.clearTimeout(dropId);
    };
  }, [actionMsg]);
  const dismissActionMsg = () => {
    // 用户点击立即触发右滑出。
    setActionLeaving(true);
    window.setTimeout(() => setActionMsg(null), 280);
  };

  const [showUpload, setShowUpload] = useState(false);
  const [uploadOriginPackId, setUploadOriginPackId] = useState<string | null>(null);
  const [uploadTargetName, setUploadTargetName] = useState<string | null>(null);
  const [localPacks, setLocalPacks] = useState<StylePack[]>([]);
  const [myPacks, setMyPacks] = useState<MarketplaceMyPackItem[]>([]);
  // 「我的发布」改为弹框形态：showMyPacks 控制开关，myPacksQuery 是弹框内独立搜索词
  // （不与外层 marketplace 搜索 query 互相干扰）。
  const [showMyPacks, setShowMyPacks] = useState(false);
  const [myPacksQuery, setMyPacksQuery] = useState('');
  // 当前用户赞过的 pack id 集合 —— 用于红心渲染 + 「我赞过的」过滤。
  // 进入 marketplace 时拉一次；点星后本地 mutate。
  const [likedIds, setLikedIds] = useState<Set<string>>(new Set());
  const canUpload = (prefs?.marketplaceDevLogin ?? '').trim().length > 0;
  const currentLogin = (prefs?.marketplaceDevLogin ?? '').trim();
  // 「衍生自」只在 origin 作者 != 当前登录身份时显示 —— 自己的 pack 不要给自己挂衍生标签。
  const isDerivative = (originLogin: string | null | undefined): boolean =>
    !!originLogin && originLogin !== currentLogin;

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
      // backend 只认 popular/new —— 'liked' 走 popular 拉一批回来，前端再过滤。
      const serverSort: 'popular' | 'new' =
        sort === 'liked' ? 'popular' : sort;
      const list = await listMarketplace({ query: debouncedQuery, sort: serverSort, limit: 50 });
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

  const visibleItems = useMemo(() => {
    if (sort === 'liked') return items.filter(it => likedIds.has(it.id));
    return items;
  }, [items, sort, likedIds]);

  const visibleMyPacks = useMemo(() => {
    const q = myPacksQuery.trim().toLowerCase();
    if (!q) return myPacks;
    return myPacks.filter(pack =>
      pack.name.toLowerCase().includes(q)
      || pack.description.toLowerCase().includes(q)
      || pack.tags.some(tag => tag.toLowerCase().includes(q)),
    );
  }, [myPacks, myPacksQuery]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 拉一次「我赞过的」缓存，渲染红心 + 「我赞过的」过滤。登录身份变更时重拉。
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const ids = await marketplaceMyLikes();
        if (!cancelled) setLikedIds(new Set(ids));
      } catch (error) {
        console.warn('[marketplace] fetch my-likes failed', error);
      }
    })();
    return () => { cancelled = true; };
  }, [currentLogin]);

  const refreshMyPacks = useCallback(async () => {
    if (!currentLogin) {
      setMyPacks([]);
      return;
    }
    try {
      const packs = await marketplaceMyPacks();
      setMyPacks(packs);
    } catch (error) {
      console.warn('[marketplace] fetch my-packs failed', error);
      setActionMsg({ kind: 'err', text: `我的发布加载失败：${errorMessage(error)}` });
    }
  }, [currentLogin]);

  useEffect(() => {
    void refreshMyPacks();
  }, [refreshMyPacks]);

  // 弹框打开时刷新一次「我的发布」，避免显示陈旧数据。
  useEffect(() => {
    if (showMyPacks && currentLogin) {
      void refreshMyPacks();
    }
  }, [showMyPacks, currentLogin, refreshMyPacks]);

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
    const packId = detail.id;
    const prevLikedIds = likedIds;
    const prevLikeCount = detail.likeCount;
    const wasLiked = prevLikedIds.has(packId);
    // optimistic mutate：立即切红心 + 调计数，让用户感觉点击即生效。
    const optimisticCount = Math.max(0, prevLikeCount + (wasLiked ? -1 : 1));
    setLikedIds(prev => {
      const next = new Set(prev);
      if (wasLiked) next.delete(packId);
      else next.add(packId);
      return next;
    });
    setDetail(prev => (prev && prev.id === packId ? { ...prev, likeCount: optimisticCount } : prev));
    setItems(prev => prev.map(p => (p.id === packId ? { ...p, likeCount: optimisticCount } : p)));
    try {
      const r = await likeMarketplacePack(packId);
      // 服务端回来后以服务端 likeCount / alreadyLiked 为准校准（防止并发或本地 drift）。
      setDetail(prev => (prev && prev.id === packId ? { ...prev, likeCount: r.likeCount } : prev));
      setItems(prev => prev.map(p => (p.id === packId ? { ...p, likeCount: r.likeCount } : p)));
      setLikedIds(prev => {
        const next = new Set(prev);
        if (r.alreadyLiked) next.add(packId);
        else next.delete(packId);
        return next;
      });
    } catch (error) {
      // rollback 到点击前的状态
      setLikedIds(prevLikedIds);
      setDetail(prev => (prev && prev.id === packId ? { ...prev, likeCount: prevLikeCount } : prev));
      setItems(prev => prev.map(p => (p.id === packId ? { ...p, likeCount: prevLikeCount } : p)));
      setActionMsg({ kind: 'err', text: t('marketplace.errors.like', { err: errorMessage(error) }) });
    }
  };

  const openUploadPicker = async (originPackId: string | null = null, targetName: string | null = null) => {
    try {
      setUploadOriginPackId(originPackId);
      setUploadTargetName(targetName);
      const packs = await listStylePacks();
      // 内置 pack 是只读模板，不能上传；更新时把同名本地版本排到最前面。
      const target = (targetName ?? '').trim().toLowerCase();
      const editable = packs
        .filter(p => p.kind !== 'builtin')
        .sort((a, b) => {
          const aMatch = target.length > 0 && a.name.trim().toLowerCase() === target;
          const bMatch = target.length > 0 && b.name.trim().toLowerCase() === target;
          if (aMatch !== bMatch) return aMatch ? -1 : 1;
          return a.name.localeCompare(b.name);
        });
      setLocalPacks(editable);
      setShowUpload(true);
    } catch (error) {
      setActionMsg({ kind: 'err', text: t('marketplace.errors.loadLocal', { err: errorMessage(error) }) });
    }
  };

  const onDelete = async () => {
    if (!detail) return;
    if (detail.authorLogin !== currentLogin) return; // 只有作者能删
    // eslint-disable-next-line no-alert
    if (!window.confirm(`确认从风格市场撤回「${detail.name}」？本地副本不会被删除。`)) return;
    try {
      await marketplaceDelete(detail.id);
      setActionMsg({ kind: 'ok', text: '已从风格市场撤回' });
      setSelectedId(null);
      // 撤回后立即从列表里去掉，再请求一次确认
      setItems(prev => prev.filter(p => p.id !== detail.id));
      void refresh();
    } catch (error) {
      setActionMsg({ kind: 'err', text: `撤回失败：${errorMessage(error)}` });
    }
  };

  const onDeleteMine = async (pack: MarketplaceMyPackItem) => {
    if (pack.authorLogin !== currentLogin) return;
    // eslint-disable-next-line no-alert
    if (!window.confirm(`确认从风格市场撤回「${pack.name}」？本地副本不会被删除。`)) return;
    try {
      await marketplaceDelete(pack.id);
      setActionMsg({ kind: 'ok', text: '已从风格市场撤回' });
      setMyPacks(prev => prev.filter(p => p.id !== pack.id));
      setItems(prev => prev.filter(p => p.id !== pack.id));
      void refreshMyPacks();
    } catch (error) {
      setActionMsg({ kind: 'err', text: `撤回失败：${errorMessage(error)}` });
    }
  };

  const onUpload = async (packId: string) => {
    const localPack = localPacks.find(p => p.id === packId);
    try {
      const result = await uploadMarketplacePack(packId, uploadOriginPackId);
      // optimistic：拿到 200 立即把这条包推到「我的发布」最前面，状态置为后端返回值（通常 'pending'）。
      // 避免等 1.5s / 5s 的 polling 才看到——后续 polling 会用服务端真实数据覆盖。
      if (localPack && currentLogin) {
        const nowIso = new Date().toISOString();
        const optimistic: MarketplaceMyPackItem = {
          id: result.id,
          slug: '',
          name: localPack.name,
          description: localPack.description ?? '',
          authorLogin: currentLogin,
          version: localPack.version ?? '',
          baseMode: localPack.baseMode ?? 'structured',
          tags: localPack.tags ?? [],
          likeCount: 0,
          downloadCount: 0,
          publishedAt: nowIso,
          updatedAt: nowIso,
          originPackId: uploadOriginPackId ?? null,
          originAuthorLogin: null,
          state: result.state,
        };
        setMyPacks(prev => {
          const idx = prev.findIndex(p => p.id === result.id);
          if (idx >= 0) {
            // 原作者更新同 id：保留 likes/downloads 等服务端计数，覆盖元信息 + 重置 state 为 pending。
            const next = [...prev];
            next[idx] = {
              ...next[idx],
              name: optimistic.name,
              description: optimistic.description,
              version: optimistic.version,
              baseMode: optimistic.baseMode,
              tags: optimistic.tags,
              updatedAt: nowIso,
              state: result.state,
            };
            return next;
          }
          return [optimistic, ...prev];
        });
      }
      setActionMsg({ kind: 'ok', text: t('marketplace.uploaded') });
      setShowUpload(false);
      setUploadOriginPackId(null);
      setUploadTargetName(null);
      // 后续 polling 用服务端真实数据校准（审核状态可能 pending→approved/rejected）。
      window.setTimeout(() => { void refresh(); void refreshMyPacks(); }, 1500);
      window.setTimeout(() => { void refresh(); void refreshMyPacks(); }, 5000);
    } catch (error) {
      setActionMsg({ kind: 'err', text: t('marketplace.errors.upload', { err: errorMessage(error) }) });
    }
  };

  const sortPills = useMemo<Array<{ id: SortMode; label: string }>>(
    () => [
      { id: 'popular', label: t('marketplace.sortPopular') },
      { id: 'new', label: t('marketplace.sortNew') },
      { id: 'liked', label: '我赞过的' },
    ],
    [t],
  );

  const accountTitle = currentLogin ? `@${currentLogin}` : '未登录';

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', minHeight: 0, position: 'relative' }}>
      <PageHeader
        kicker={t('marketplace.kicker')}
        title={t('marketplace.title')}
        desc={t('marketplace.desc')}
        right={
          <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', justifyContent: 'flex-end' }}>
            <button
              type="button"
              onClick={() => setShowMyPacks(true)}
              title={currentLogin ? `查看 ${currentLogin} 的发布` : '先在 Settings → 风格市场 填写发布身份'}
              style={{
                display: 'inline-flex', alignItems: 'center', gap: 8,
                height: 30, padding: '0 12px', borderRadius: 9,
                border: '0.5px solid var(--ol-line-strong)',
                background: 'var(--ol-surface)',
                color: 'var(--ol-ink-2)',
                fontSize: 12, fontWeight: 650,
                cursor: 'pointer',
                boxShadow: '0 1px 2px rgba(15,17,22,0.04)',
              }}
            >
              <span style={{
                width: 18, height: 18, borderRadius: 999,
                display: 'inline-grid', placeItems: 'center',
                background: 'rgba(15,23,42,0.06)',
                fontSize: 10, fontWeight: 750,
              }}>
                {(currentLogin || '?').slice(0, 1).toUpperCase()}
              </span>
              <span>我的发布</span>
            </button>
            <Btn icon="refresh" variant="ghost" size="sm" onClick={() => void refresh()}>
              {t('common.refresh')}
            </Btn>
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
          role={actionMsg.kind === 'err' ? 'alert' : 'status'}
          onClick={dismissActionMsg}
          className={actionLeaving ? 'ol-marketplace-toast ol-marketplace-toast-leave' : 'ol-marketplace-toast'}
          style={{
            // 锚到风格市场 modal 内容区的右下角。
            position: 'absolute',
            right: 20,
            bottom: 20,
            maxWidth: 280,
            padding: '7px 12px',
            borderRadius: 10,
            fontSize: 11.5,
            lineHeight: 1.4,
            background: actionMsg.kind === 'ok' ? 'rgba(37,99,235,0.95)' : 'rgba(220,38,38,0.95)',
            color: '#fff',
            boxShadow: '0 10px 24px -8px rgba(15,17,22,.32), 0 0 0 0.5px rgba(0,0,0,.06)',
            backdropFilter: 'blur(8px) saturate(140%)',
            WebkitBackdropFilter: 'blur(8px) saturate(140%)',
            cursor: 'pointer',
            zIndex: 4,
            whiteSpace: 'nowrap',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
          }}
        >
          {actionMsg.text}
          <style>{`
            @keyframes ol-mkt-toast-in {
              from { opacity: 0; transform: translateX(120%); }
              to   { opacity: 1; transform: translateX(0); }
            }
            @keyframes ol-mkt-toast-out {
              from { opacity: 1; transform: translateX(0); }
              to   { opacity: 0; transform: translateX(120%); }
            }
            .ol-marketplace-toast {
              animation: ol-mkt-toast-in .26s cubic-bezier(.34,1.56,.64,1);
              will-change: transform, opacity;
            }
            .ol-marketplace-toast-leave {
              animation: ol-mkt-toast-out .26s cubic-bezier(.4,0,1,1) forwards;
            }
          `}</style>
        </div>
      )}

      {loadError && (
        <Card padding={16} style={{ marginBottom: 12, borderColor: 'var(--ol-err)' }}>
          <div style={{ fontSize: 12, color: 'var(--ol-err)' }}>
            {t('marketplace.loadFailed', { err: loadError })}
          </div>
        </Card>
      )}

      {/* 卡片列表 / 我的发布 */}
      <div style={{ flex: 1, overflow: 'auto' }} className="ol-thinscroll">
        {loading ? (
          <div style={{ padding: 32, textAlign: 'center', color: 'var(--ol-ink-4)', fontSize: 13 }}>
            {t('common.loading')}
          </div>
        ) : visibleItems.length === 0 ? (
          <Card padding={28} style={{ textAlign: 'center' }}>
            <div style={{ fontSize: 13, color: 'var(--ol-ink-3)', marginBottom: 6 }}>
              {sort === 'liked' && '你还没有赞过任何风格包'}
              {(sort === 'popular' || sort === 'new') && t('marketplace.empty')}
            </div>
            <div style={{ fontSize: 11, color: 'var(--ol-ink-4)' }}>
              {sort === 'liked' && '点开任一风格包，红色星星点亮后会出现在这里'}
              {(sort === 'popular' || sort === 'new') && t('marketplace.emptyHint')}
            </div>
          </Card>
        ) : (
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(260px, 1fr))', gap: 12 }}>
            {visibleItems.map(p => (
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
                  <span style={{ fontSize: 10, color: 'var(--ol-ink-4)', fontFamily: 'var(--ol-font-mono)' }}>v{p.version}</span>
                </div>
                <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', lineHeight: 1.5, display: '-webkit-box', WebkitLineClamp: 2, WebkitBoxOrient: 'vertical', overflow: 'hidden', minHeight: 36 }}>
                  {p.description || t('marketplace.noDescription')}
                </div>
                <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', marginTop: 2 }}>
                  <Pill size="sm" tone="outline">{p.baseMode}</Pill>
                  {isDerivative(p.originAuthorLogin) && (
                    <span title={`衍生自 @${p.originAuthorLogin}`}>
                      <Pill size="sm" tone="ok">衍生自 @{p.originAuthorLogin}</Pill>
                    </span>
                  )}
                  {p.tags.slice(0, 2).map(tag => <Pill key={tag} size="sm" tone="default">{tag}</Pill>)}
                </div>
                <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 11, color: 'var(--ol-ink-4)', marginTop: 4 }}>
                  <span style={{ fontWeight: 500, color: 'var(--ol-ink-3)' }}>@{p.authorLogin}</span>
                  <span>
                    <span style={{ color: likedIds.has(p.id) ? '#ef4444' : 'var(--ol-ink-4)' }}>{likedIds.has(p.id) ? '★' : '☆'}</span>
                    {' '}{p.likeCount} · ↓ {p.downloadCount}
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
              <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, marginBottom: 6, flexWrap: 'wrap' }}>
                <h2 style={{ margin: 0, fontSize: 18, fontWeight: 650 }}>{detail.name}</h2>
                <Pill size="sm" tone="outline">{detail.baseMode}</Pill>
                {isDerivative(detail.originAuthorLogin) && (
                  <span title={`衍生自 @${detail.originAuthorLogin}`}>
                    <Pill size="sm" tone="ok">衍生自 @{detail.originAuthorLogin}</Pill>
                  </span>
                )}
                <span style={{ fontSize: 11, color: 'var(--ol-ink-4)', fontFamily: 'var(--ol-font-mono)' }}>
                  v{detail.version}
                </span>
              </div>
              <div style={{ fontSize: 11, color: 'var(--ol-ink-4)', marginBottom: 12 }}>
                <span style={{ fontWeight: 500, color: 'var(--ol-ink-3)' }}>@{detail.authorLogin}</span>
                {' · '}
                <span style={{ color: likedIds.has(detail.id) ? '#ef4444' : 'var(--ol-ink-4)' }}>
                  {likedIds.has(detail.id) ? '★' : '☆'}
                </span>
                {' '}{detail.likeCount}{' · ↓ '}{detail.downloadCount}
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
              <div style={{ display: 'flex', justifyContent: 'space-between', gap: 8, alignItems: 'center' }}>
                <div>
                  {detail.authorLogin === currentLogin && currentLogin.length > 0 && (
                    <Btn variant="ghost" size="sm" onClick={() => void onDelete()}>
                      <span style={{ color: '#ef4444', marginRight: 4 }}>🗑</span>
                      撤回发布
                    </Btn>
                  )}
                </div>
                <div style={{ display: 'flex', gap: 8 }}>
                  <Btn variant="ghost" size="sm" onClick={() => void onLike()}>
                    <span
                      key={`star-${detail.id}-${likedIds.has(detail.id) ? 'on' : 'off'}`}
                      className="ol-star-pop"
                      style={{
                        color: likedIds.has(detail.id) ? '#ef4444' : 'inherit',
                        marginRight: 4,
                        display: 'inline-block',
                      }}
                    >
                      {likedIds.has(detail.id) ? '★' : '☆'}
                    </span>
                    {detail.likeCount}
                  </Btn>
                  <Btn variant="ghost" size="sm" onClick={() => setSelectedId(null)}>
                    {t('common.cancel')}
                  </Btn>
                  <Btn variant="blue" size="sm" onClick={() => void onInstall()}>
                    {t('marketplace.installBtn')}
                  </Btn>
                </div>
              </div>
              <style>{`
                @keyframes ol-heart-pop-keyframes {
                  0%   { transform: scale(1); }
                  35%  { transform: scale(1.45); }
                  60%  { transform: scale(.85); }
                  100% { transform: scale(1); }
                }
                .ol-star-pop { animation: ol-heart-pop-keyframes .32s var(--ol-motion-spring, cubic-bezier(.34,1.56,.64,1)); }
              `}</style>
            </>
          )}
        </Modal>
      )}

      {/* 上传选包器 */}
      {showUpload && (
        <Modal onClose={() => { setShowUpload(false); setUploadOriginPackId(null); setUploadTargetName(null); }}>
          <h2 style={{ margin: '0 0 12px', fontSize: 16, fontWeight: 650 }}>
            {uploadOriginPackId ? `更新「${uploadTargetName ?? '风格包'}」` : t('marketplace.uploadTitle')}
          </h2>
          <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginBottom: 12 }}>
            {uploadOriginPackId ? '请选择本地对应的新版本风格包；同名本地包会排在最上面。' : t('marketplace.uploadHint', { login: prefs?.marketplaceDevLogin ?? '' })}
          </div>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 8, maxHeight: 360, overflow: 'auto' }}>
            {localPacks.length === 0 ? (
              <div style={{ fontSize: 12, color: 'var(--ol-ink-4)', textAlign: 'center', padding: 20 }}>
                {t('marketplace.uploadNoLocal')}
              </div>
            ) : (
              localPacks.map(p => {
                const recommended = !!uploadTargetName && p.name.trim().toLowerCase() === uploadTargetName.trim().toLowerCase();
                return (
                  <button
                    key={p.id}
                    onClick={() => void onUpload(p.id)}
                    style={{
                      textAlign: 'left',
                      padding: 10,
                      border: recommended ? '0.5px solid var(--ol-blue)' : '0.5px solid var(--ol-line-strong)',
                      borderRadius: 8,
                      background: recommended ? 'var(--ol-blue-soft)' : 'var(--ol-surface)',
                      cursor: 'pointer',
                    }}
                  >
                    <div style={{ display: 'flex', alignItems: 'center', gap: 8, justifyContent: 'space-between' }}>
                      <div style={{ fontSize: 13, fontWeight: 600 }}>{p.name}</div>
                      {recommended && <Pill size="sm" tone="blue">建议更新</Pill>}
                    </div>
                    <div style={{ fontSize: 11, color: 'var(--ol-ink-4)', marginTop: 2 }}>
                      {p.description || t('marketplace.noDescription')}
                    </div>
                  </button>
                );
              })
            )}
          </div>
          <div style={{ display: 'flex', justifyContent: 'flex-end', marginTop: 14 }}>
            <Btn variant="ghost" size="sm" onClick={() => { setShowUpload(false); setUploadOriginPackId(null); setUploadTargetName(null); }}>
              {t('common.cancel')}
            </Btn>
          </div>
        </Modal>
      )}

      {/* 我的发布 · 弹框形态（叠在风格市场页面之上）*/}
      {showMyPacks && (
        <Modal onClose={() => setShowMyPacks(false)}>
          {/* 标题 + 关闭 */}
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 14, gap: 12 }}>
            <h2 style={{ margin: 0, fontSize: 16, fontWeight: 650 }}>我的发布</h2>
          </div>

          {/* 账户信息 */}
          <div
            style={{
              display: 'flex', alignItems: 'center', gap: 10,
              padding: '10px 12px',
              border: '0.5px solid var(--ol-line-strong)',
              borderRadius: 10,
              background: 'var(--ol-surface-2)',
              marginBottom: 12,
            }}
          >
            <span style={{
              width: 28, height: 28, borderRadius: 999,
              display: 'inline-grid', placeItems: 'center',
              background: 'var(--ol-blue-soft)', color: 'var(--ol-blue)',
              fontSize: 12, fontWeight: 750,
            }}>
              {(currentLogin || '?').slice(0, 1).toUpperCase()}
            </span>
            <div style={{ minWidth: 0, flex: 1 }}>
              <div style={{ fontSize: 13, fontWeight: 650, color: 'var(--ol-ink)' }}>{accountTitle}</div>
              <div style={{ fontSize: 11.5, color: 'var(--ol-ink-3)', marginTop: 2 }}>
                {currentLogin
                  ? `已发布 ${myPacks.length} 个风格包${myPacks.filter(p => p.state === 'pending').length > 0 ? ` · ${myPacks.filter(p => p.state === 'pending').length} 个审核中` : ''}`
                  : '请先在 Settings → 风格市场 填写发布身份'}
              </div>
            </div>
          </div>

          {/* 搜索（左上）+ 刷新 + 上传 */}
          <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginBottom: 12 }}>
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
                placeholder="搜索名称、标签"
                value={myPacksQuery}
                onChange={e => setMyPacksQuery(e.target.value)}
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
            <Btn icon="refresh" variant="ghost" size="sm" onClick={() => void refreshMyPacks()} disabled={!currentLogin}>
              {t('common.refresh')}
            </Btn>
            <span title={canUpload ? '' : t('marketplace.uploadDisabledHint')}>
              <Btn icon="cloud" variant="blue" size="sm" onClick={() => void openUploadPicker()} disabled={!canUpload}>
                {t('marketplace.uploadBtn')}
              </Btn>
            </span>
          </div>

          {/* 包列表 */}
          {visibleMyPacks.length === 0 ? (
            <div style={{ padding: '32px 12px', textAlign: 'center' }}>
              <div style={{ fontSize: 13, color: 'var(--ol-ink-3)', marginBottom: 6 }}>
                {currentLogin
                  ? (myPacks.length === 0 ? '你还没有发布过风格包' : '没有匹配的风格包')
                  : '请先在 Settings → 风格市场 填写发布身份'}
              </div>
              {currentLogin && myPacks.length === 0 && (
                <div style={{ fontSize: 11, color: 'var(--ol-ink-4)' }}>
                  在「风格」页面编辑后点「发布到风格市场」，或点击右上角上传本地风格包。
                </div>
              )}
            </div>
          ) : (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
              {visibleMyPacks.map(pack => (
                <div
                  key={pack.id}
                  style={{
                    padding: 14,
                    borderRadius: 12,
                    border: '0.5px solid var(--ol-line-strong)',
                    background: 'var(--ol-surface)',
                    display: 'flex',
                    flexDirection: 'column',
                    gap: 8,
                  }}
                >
                  <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', gap: 8 }}>
                    <div style={{ minWidth: 0 }}>
                      <div style={{ fontSize: 14, fontWeight: 650, color: 'var(--ol-ink)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{pack.name}</div>
                      <div style={{ fontSize: 11, color: 'var(--ol-ink-4)', marginTop: 3 }}>v{pack.version} · {new Date(pack.updatedAt).toLocaleDateString()}</div>
                    </div>
                    <Pill size="sm" tone={pack.state === 'approved' ? 'ok' : 'outline'} style={pack.state === 'rejected' || pack.state === 'withdrawn' ? { color: '#ef4444', borderColor: 'rgba(239,68,68,0.28)' } : undefined}>{statusLabel(pack.state)}</Pill>
                  </div>
                  {pack.description && (
                    <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', lineHeight: 1.5 }}>{pack.description}</div>
                  )}
                  <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
                    <Pill size="sm" tone="outline">{pack.baseMode}</Pill>
                    {pack.tags.slice(0, 3).map(tag => <Pill key={tag} size="sm" tone="default">{tag}</Pill>)}
                  </div>
                  <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: 8, marginTop: 2 }}>
                    <span style={{ fontSize: 11, color: 'var(--ol-ink-4)' }}>★ {pack.likeCount} · ↓ {pack.downloadCount}</span>
                    <div style={{ display: 'flex', gap: 6 }}>
                      <Btn variant="ghost" size="sm" onClick={() => void openUploadPicker(pack.id, pack.name)} disabled={!canUpload}>
                        更新
                      </Btn>
                      {pack.state !== 'withdrawn' && (
                        <Btn variant="ghost" size="sm" onClick={() => void onDeleteMine(pack)}>
                          <span style={{ color: '#ef4444' }}>下架</span>
                        </Btn>
                      )}
                    </div>
                  </div>
                </div>
              ))}
            </div>
          )}
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

function statusLabel(state: string): string {
  switch (state) {
    case 'pending': return '审核中';
    case 'approved': return '已上架';
    case 'rejected': return '未通过';
    case 'withdrawn': return '已下架';
    case 'superseded': return '已被新版替换';
    default: return state || '未知';
  }
}

function errorMessage(error: unknown): string {
  if (typeof error === 'string') return error;
  if (error instanceof Error) return error.message;
  return String(error);
}
