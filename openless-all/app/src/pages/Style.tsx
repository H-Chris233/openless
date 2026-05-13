import { type CSSProperties, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  deleteStylePack,
  exportStylePackToZip,
  importStylePackFromZip,
  isTauri,
  listStylePacks,
  resetBuiltinStylePack,
  saveStylePack,
  setActiveStylePack,
  setStylePackEnabled,
} from '../lib/ipc';
import type { PolishMode, StylePack, StylePackExample } from '../lib/types';
import { Btn, Card, PageHeader, Pill } from './_atoms';
import { Icon } from '../components/Icon';

type BusyAction =
  | 'loading'
  | 'saving'
  | 'importing'
  | 'exporting'
  | 'activating'
  | 'toggling'
  | 'resetting'
  | 'deleting'
  | null;

function clonePack(pack: StylePack): StylePack {
  return {
    ...pack,
    tags: [...pack.tags],
    examples: pack.examples.map(example => ({ ...example })),
  };
}

function editableFingerprint(pack: StylePack | null): string {
  if (!pack) return '';
  return JSON.stringify({
    name: pack.name,
    description: pack.description,
    author: pack.author ?? '',
    version: pack.version,
    prompt: pack.prompt,
    examples: pack.examples,
    tags: pack.tags,
    recommendedModel: pack.recommendedModel ?? '',
    compatibleAppVersion: pack.compatibleAppVersion ?? '',
  });
}

function blankExample(): StylePackExample {
  return {
    title: '',
    input: '',
    output: '',
  };
}

function modeTone(mode: PolishMode): 'default' | 'blue' | 'ok' | 'outline' | 'dark' {
  if (mode === 'raw') return 'outline';
  if (mode === 'light') return 'blue';
  if (mode === 'structured') return 'ok';
  return 'dark';
}

function sanitizeZipFileName(name: string) {
  const trimmed = name.trim() || 'style-pack';
  return trimmed.replace(/[<>:"/\\|?*]+/g, '-').replace(/\s+/g, '-').toLowerCase();
}

export function Style() {
  const { t, i18n } = useTranslation();
  const isEnglish = i18n.language.toLowerCase().startsWith('en');
  const store = {
    kicker: isEnglish ? 'STYLE PACKS' : 'STYLE PACKS',
    title: isEnglish ? 'Style Packs' : '风格包',
    desc: isEnglish
      ? 'Manage output styles as pluggable Style Packs. Built-in and imported packs share the same runtime prompt, ZIP exchange format, and repolish path.'
      : '把输出风格作为可插拔的 Style Pack 来管理。内置风格和导入风格共用同一套运行时 prompt、ZIP 交换格式和 repolish 链路。',
    loadFailed: (message: string) => (isEnglish ? `Failed to load style packs: ${message}` : `加载风格包失败：${message}`),
    importZip: isEnglish ? 'Import ZIP' : '导入 ZIP',
    exportZip: isEnglish ? 'Export ZIP' : '导出 ZIP',
    builtin: isEnglish ? 'Built-in' : '内置',
    imported: isEnglish ? 'Imported' : '导入',
    active: isEnglish ? 'Active' : '当前启用',
    enabled: isEnglish ? 'Enabled' : '已启用',
    disabled: isEnglish ? 'Disabled' : '已停用',
    activate: isEnglish ? 'Activate' : '启用此风格',
    enable: isEnglish ? 'Enable' : '启用',
    disable: isEnglish ? 'Disable' : '停用',
    details: isEnglish ? 'Details' : '查看详情',
    unsaved: isEnglish ? 'Unsaved' : '未保存',
    listTitle: isEnglish ? 'Local Packs' : '本地风格包',
    listDesc: isEnglish
      ? 'Manage styles like products in a local store. Pick a card, then edit the full prompt, examples, and metadata on the right.'
      : '像管理商品一样管理你的风格包。选择卡片后，在右侧编辑完整 prompt、示例和元信息。',
    listCount: (count: number) => (isEnglish ? `${count} packs` : `${count} 个风格包`),
    detailTitle: isEnglish ? 'Pack Details' : '风格包详情',
    detailDesc: isEnglish
      ? 'The runtime uses this full prompt directly. Polish and repolish now share the same Style Pack selection.'
      : '运行时直接使用这里的完整 prompt。polish 和 repolish 走同一套 Style Pack 逻辑。',
    noSelection: isEnglish
      ? 'Select any pack on the left to inspect and edit its full definition.'
      : '选择左侧任意风格包后，这里会显示完整定义和示例编辑区。',
    save: isEnglish ? 'Save Pack' : '保存风格包',
    revert: isEnglish ? 'Discard Changes' : '放弃修改',
    saveSuccess: isEnglish ? 'Style pack saved.' : '风格包已保存',
    saveFailed: (message: string) => (isEnglish ? `Failed to save style pack: ${message}` : `保存风格包失败：${message}`),
    activateSuccess: (name: string) => (isEnglish ? `Activated "${name}".` : `已启用「${name}」`),
    activateFailed: (message: string) => (isEnglish ? `Failed to activate style pack: ${message}` : `启用风格包失败：${message}`),
    enableSuccess: (name: string) => (isEnglish ? `Enabled "${name}".` : `已启用「${name}」`),
    disableSuccess: (name: string) => (isEnglish ? `Disabled "${name}".` : `已停用「${name}」`),
    toggleFailed: (message: string) => (isEnglish ? `Failed to change pack status: ${message}` : `切换启用状态失败：${message}`),
    importSuccess: (name: string) => (isEnglish ? `Imported "${name}".` : `已导入「${name}」`),
    importFailed: (message: string) => (isEnglish ? `Failed to import ZIP: ${message}` : `导入 ZIP 失败：${message}`),
    exportSuccess: (path: string) => (isEnglish ? `Exported to ${path}` : `已导出到 ${path}`),
    exportFailed: (message: string) => (isEnglish ? `Failed to export ZIP: ${message}` : `导出 ZIP 失败：${message}`),
    resetBuiltin: isEnglish ? 'Reset Built-in Pack' : '重置内置风格',
    resetSuccess: (name: string) => (isEnglish ? `Reset "${name}".` : `已重置「${name}」`),
    resetFailed: (message: string) => (isEnglish ? `Failed to reset pack: ${message}` : `重置风格包失败：${message}`),
    deleteImported: isEnglish ? 'Delete Imported Pack' : '删除导入包',
    deleteConfirm: (name: string) =>
      (isEnglish ? `Delete "${name}"? This cannot be undone.` : `确定删除「${name}」吗？删除后无法恢复。`),
    deleteSuccess: (name: string) => (isEnglish ? `Deleted "${name}".` : `已删除「${name}」`),
    deleteFailed: (message: string) => (isEnglish ? `Failed to delete pack: ${message}` : `删除风格包失败：${message}`),
    summaryBuiltin: isEnglish ? 'Built-in Packs' : '内置风格',
    summaryBuiltinHint: isEnglish ? 'Default product semantics with one-click reset.' : '跟随产品默认语义，可被重置到官方基线。',
    summaryImported: isEnglish ? 'Imported Packs' : '导入风格',
    summaryImportedHint: isEnglish ? 'Installed from ZIP and fully portable.' : '来自 ZIP 包，可启用、编辑、导出和删除。',
    summaryEnabled: isEnglish ? 'Rotation Ready' : '当前可轮换',
    summaryCurrent: (name: string) => (isEnglish ? `Current: ${name}` : `当前启用：${name}`),
    summaryCurrentEmpty: isEnglish ? 'No pack selected yet' : '还没有选中风格包',
    metaTitle: isEnglish ? 'Installation Info' : '安装信息',
    metaSource: isEnglish ? 'Source' : '来源',
    metaBaseMode: isEnglish ? 'Base Mode' : '基础模式',
    metaStatus: isEnglish ? 'Status' : '状态',
    metaUpdatedAt: isEnglish ? 'Updated' : '更新时间',
    fieldName: isEnglish ? 'Name' : '名称',
    fieldAuthor: isEnglish ? 'Author' : '作者',
    fieldAuthorPlaceholder: isEnglish ? 'Optional source label' : '可选，方便标注来源',
    fieldVersion: isEnglish ? 'Version' : '版本',
    fieldTags: isEnglish ? 'Tags' : '标签',
    fieldTagsPlaceholder: isEnglish ? 'Comma-separated tags, e.g. community, voiceover, formal' : '用英文逗号分隔，例如 社区, 口播, 正式',
    fieldDescription: isEnglish ? 'Description' : '描述',
    fieldModel: isEnglish ? 'Recommended Model' : '推荐模型',
    fieldModelPlaceholder: isEnglish ? 'Optional, e.g. gpt-4.1 / deepseek-v3' : '可选，例如 gpt-4.1 / deepseek-v3',
    fieldCompatibility: isEnglish ? 'Compatible App Version' : '兼容版本',
    fieldCompatibilityPlaceholder: isEnglish ? 'Optional, e.g. >=1.3.0' : '可选，例如 >=1.3.0',
    fullPromptTitle: isEnglish ? 'Full System Prompt' : '完整 System Prompt',
    fullPromptHint: isEnglish
      ? 'This is the full runtime prompt for the pack, not a suffix appended to a hardcoded prompt.'
      : '这里编辑的是该风格包的完整运行时 prompt，不是在旧 prompt 末尾追加一句话。',
    examplesTitle: isEnglish ? 'Effect Examples' : '效果示例',
    examplesDesc: isEnglish
      ? 'Present input/output pairs like a product detail page. They will be exported into examples.json.'
      : '像商品详情页一样展示输入和输出。导出 ZIP 时会一起写入 examples.json。',
    addExample: isEnglish ? 'Add Example' : '新增示例',
    examplesEmpty: isEnglish
      ? 'This pack has no examples yet. Add at least one input/output pair before sharing it.'
      : '这个风格包还没有示例。建议至少补一组输入 / 输出，方便导出后被别人理解和复用。',
    exampleTitlePlaceholder: (index: number) => (isEnglish ? `Example ${index} title` : `示例 ${index} 标题`),
    exampleInput: isEnglish ? 'Input' : '输入',
    exampleOutput: isEnglish ? 'Output' : '输出',
  };
  const [packs, setPacks] = useState<StylePack[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [draft, setDraft] = useState<StylePack | null>(null);
  const [busy, setBusy] = useState<BusyAction>('loading');
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const loadPacks = async (preferredId?: string | null) => {
    setBusy('loading');
    setError(null);
    try {
      const next = await listStylePacks();
      setPacks(next);
      const nextSelectedId =
        (preferredId && next.some(pack => pack.id === preferredId) && preferredId) ||
        next.find(pack => pack.active)?.id ||
        next[0]?.id ||
        null;
      setSelectedId(nextSelectedId);
    } catch (loadError) {
      setError(store.loadFailed(String(loadError)));
    } finally {
      setBusy(null);
    }
  };

  useEffect(() => {
    void loadPacks();
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    (async () => {
      try {
        const { listen } = await import('@tauri-apps/api/event');
        unlisten = await listen('prefs:changed', () => {
          void loadPacks(selectedId);
        });
        if (cancelled && unlisten) unlisten();
      } catch {
        // Browser dev mock does not have the event bridge.
      }
    })();
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, [selectedId]);

  const selectedPack = packs.find(pack => pack.id === selectedId) ?? null;
  const builtinCount = packs.filter(pack => pack.kind === 'builtin').length;
  const importedCount = packs.filter(pack => pack.kind === 'imported').length;
  const enabledCount = packs.filter(pack => pack.enabled).length;

  useEffect(() => {
    if (!selectedPack) {
      setDraft(null);
      return;
    }
    setDraft(clonePack(selectedPack));
  }, [selectedPack?.id, selectedPack?.updatedAt, selectedPack?.active, selectedPack?.enabled]);

  const dirty = editableFingerprint(selectedPack) !== editableFingerprint(draft);

  const patchDraft = (patch: Partial<StylePack>) => {
    setDraft(current => (current ? { ...current, ...patch } : current));
  };

  const patchExample = (index: number, patch: Partial<StylePackExample>) => {
    setDraft(current => {
      if (!current) return current;
      const nextExamples = current.examples.map((example, currentIndex) =>
        currentIndex === index ? { ...example, ...patch } : example,
      );
      return { ...current, examples: nextExamples };
    });
  };

  const appendExample = () => {
    setDraft(current => (current ? { ...current, examples: [...current.examples, blankExample()] } : current));
  };

  const removeExample = (index: number) => {
    setDraft(current => {
      if (!current) return current;
      return {
        ...current,
        examples: current.examples.filter((_, currentIndex) => currentIndex !== index),
      };
    });
  };

  const showSuccess = (message: string) => {
    setNotice(message);
    setError(null);
  };

  const handleSave = async () => {
    if (!draft) return;
    setBusy('saving');
    try {
      const saved = await saveStylePack({
        ...draft,
        tags: draft.tags.filter(Boolean),
      });
      showSuccess(store.saveSuccess);
      await loadPacks(saved.id);
    } catch (saveError) {
      setError(store.saveFailed(String(saveError)));
    } finally {
      setBusy(null);
    }
  };

  const handleActivate = async (pack: StylePack) => {
    setBusy('activating');
    try {
      await setActiveStylePack(pack.id);
      showSuccess(store.activateSuccess(pack.name));
      await loadPacks(pack.id);
    } catch (activateError) {
      setError(store.activateFailed(String(activateError)));
    } finally {
      setBusy(null);
    }
  };

  const handleToggleEnabled = async (pack: StylePack) => {
    setBusy('toggling');
    try {
      await setStylePackEnabled(pack.id, !pack.enabled);
      showSuccess(
        pack.enabled ? store.disableSuccess(pack.name) : store.enableSuccess(pack.name),
      );
      await loadPacks(pack.id);
    } catch (toggleError) {
      setError(store.toggleFailed(String(toggleError)));
    } finally {
      setBusy(null);
    }
  };

  const handleResetBuiltin = async () => {
    if (!selectedPack || selectedPack.kind !== 'builtin') return;
    setBusy('resetting');
    try {
      await resetBuiltinStylePack(selectedPack.id);
      showSuccess(store.resetSuccess(selectedPack.name));
      await loadPacks(selectedPack.id);
    } catch (resetError) {
      setError(store.resetFailed(String(resetError)));
    } finally {
      setBusy(null);
    }
  };

  const handleDeleteImported = async () => {
    if (!selectedPack || selectedPack.kind !== 'imported') return;
    if (!window.confirm(store.deleteConfirm(selectedPack.name))) {
      return;
    }
    setBusy('deleting');
    try {
      await deleteStylePack(selectedPack.id);
      showSuccess(store.deleteSuccess(selectedPack.name));
      await loadPacks();
    } catch (deleteError) {
      setError(store.deleteFailed(String(deleteError)));
    } finally {
      setBusy(null);
    }
  };

  const handleImportZip = async () => {
    setBusy('importing');
    try {
      let zipPath: string | null = null;
      if (isTauri) {
        const { open } = await import('@tauri-apps/plugin-dialog');
        const picked = await open({
          filters: [{ name: 'Style Pack ZIP', extensions: ['zip'] }],
          multiple: false,
        });
        zipPath = typeof picked === 'string' ? picked : null;
      } else {
        zipPath = 'mock-style-pack.zip';
      }
      if (!zipPath) {
        setBusy(null);
        return;
      }
      const imported = await importStylePackFromZip(zipPath);
      showSuccess(store.importSuccess(imported.name));
      await loadPacks(imported.id);
    } catch (importError) {
      setError(store.importFailed(String(importError)));
    } finally {
      setBusy(null);
    }
  };

  const handleExportZip = async () => {
    if (!selectedPack) return;
    setBusy('exporting');
    try {
      const defaultName = `${sanitizeZipFileName(selectedPack.name)}.zip`;
      let targetPath: string | null = null;
      if (isTauri) {
        const { save } = await import('@tauri-apps/plugin-dialog');
        targetPath = await save({
          defaultPath: defaultName,
          filters: [{ name: 'Style Pack ZIP', extensions: ['zip'] }],
        });
      } else {
        targetPath = `~/Downloads/${defaultName}`;
      }
      if (!targetPath) {
        setBusy(null);
        return;
      }
      const savedPath = await exportStylePackToZip(selectedPack.id, targetPath);
      showSuccess(store.exportSuccess(savedPath));
    } catch (exportError) {
      setError(store.exportFailed(String(exportError)));
    } finally {
      setBusy(null);
    }
  };

  return (
    <>
      <PageHeader
        kicker={store.kicker}
        title={store.title}
        desc={store.desc}
        right={(
          <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap', justifyContent: 'flex-end' }}>
            <Btn variant="ghost" icon="refresh" onClick={() => void loadPacks(selectedId)} disabled={busy === 'loading'}>
              {t('common.refresh')}
            </Btn>
            <Btn variant="blue" icon="archive" onClick={() => void handleImportZip()} disabled={busy === 'importing'}>
              {busy === 'importing' ? t('common.loading') : store.importZip}
            </Btn>
          </div>
        )}
      />

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 12, marginBottom: 16 }}>
        <Card padding={16} glassy>
          <div style={{ fontSize: 11, letterSpacing: '.08em', textTransform: 'uppercase', color: 'var(--ol-ink-4)', marginBottom: 8 }}>
            {store.summaryBuiltin}
          </div>
          <div style={{ fontSize: 24, fontWeight: 600, color: 'var(--ol-ink)' }}>{builtinCount}</div>
          <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 4 }}>{store.summaryBuiltinHint}</div>
        </Card>
        <Card padding={16} glassy>
          <div style={{ fontSize: 11, letterSpacing: '.08em', textTransform: 'uppercase', color: 'var(--ol-ink-4)', marginBottom: 8 }}>
            {store.summaryImported}
          </div>
          <div style={{ fontSize: 24, fontWeight: 600, color: 'var(--ol-ink)' }}>{importedCount}</div>
          <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 4 }}>{store.summaryImportedHint}</div>
        </Card>
        <Card padding={16} glassy>
          <div style={{ fontSize: 11, letterSpacing: '.08em', textTransform: 'uppercase', color: 'var(--ol-ink-4)', marginBottom: 8 }}>
            {store.summaryEnabled}
          </div>
          <div style={{ fontSize: 24, fontWeight: 600, color: 'var(--ol-ink)' }}>{enabledCount}</div>
          <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 4 }}>
            {selectedPack ? store.summaryCurrent(selectedPack.name) : store.summaryCurrentEmpty}
          </div>
        </Card>
      </div>

      {(notice || error) && (
        <div
          role={error ? 'alert' : 'status'}
          style={{
            marginBottom: 14,
            padding: '12px 14px',
            borderRadius: 12,
            border: error ? '0.5px solid rgba(239,68,68,0.22)' : '0.5px solid rgba(37,99,235,0.16)',
            background: error ? 'rgba(254,242,242,0.9)' : 'rgba(239,246,255,0.92)',
            color: error ? 'var(--ol-red, #b91c1c)' : 'var(--ol-blue)',
            fontSize: 12.5,
            lineHeight: 1.55,
          }}
        >
          {error ?? notice}
        </div>
      )}

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(360px, 1fr))', gap: 16, alignItems: 'start' }}>
        <Card padding={0} style={{ overflow: 'hidden' }}>
          <div style={{ padding: 18, borderBottom: '0.5px solid var(--ol-line)' }}>
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12 }}>
              <div>
                <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.listTitle}</div>
                <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 4 }}>{store.listDesc}</div>
              </div>
              <Pill tone="outline">{store.listCount(packs.length)}</Pill>
            </div>
          </div>
          <div style={{ padding: 18 }}>
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))', gap: 12 }}>
              {packs.map(pack => {
                const selected = pack.id === selectedId;
                return (
                  <div
                    key={pack.id}
                    onClick={() => {
                      setSelectedId(pack.id);
                      setNotice(null);
                      setError(null);
                    }}
                    role="button"
                    tabIndex={0}
                    onKeyDown={event => {
                      if (event.key === 'Enter' || event.key === ' ') {
                        event.preventDefault();
                        setSelectedId(pack.id);
                        setNotice(null);
                        setError(null);
                      }
                    }}
                    style={{
                      textAlign: 'left',
                      border: '0.5px solid',
                      borderColor: selected || pack.active ? 'var(--ol-blue)' : 'var(--ol-line)',
                      background: pack.active
                        ? 'linear-gradient(180deg, rgba(239,246,255,0.92), rgba(255,255,255,0.98))'
                        : 'var(--ol-surface-2)',
                      borderRadius: 16,
                      padding: 16,
                      boxShadow: selected || pack.active ? '0 0 0 3px var(--ol-blue-ring)' : 'none',
                      cursor: 'default',
                      opacity: pack.enabled ? 1 : 0.58,
                      transition: 'border-color 0.16s var(--ol-motion-quick), box-shadow 0.18s var(--ol-motion-soft), opacity 0.18s var(--ol-motion-soft)',
                    }}
                  >
                    <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 10, marginBottom: 10 }}>
                      <div style={{ minWidth: 0 }}>
                        <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', marginBottom: 6 }}>
                          <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--ol-ink)' }}>{pack.name}</div>
                          <Pill tone={pack.kind === 'builtin' ? 'outline' : 'blue'} size="sm">
                            {pack.kind === 'builtin' ? store.builtin : store.imported}
                          </Pill>
                          {pack.active && <Pill tone="dark" size="sm">{store.active}</Pill>}
                          {!pack.enabled && <Pill tone="default" size="sm">{store.disabled}</Pill>}
                        </div>
                        <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', lineHeight: 1.55 }}>{pack.description}</div>
                      </div>
                      <div
                        style={{
                          width: 34,
                          height: 34,
                          borderRadius: 12,
                          display: 'grid',
                          placeItems: 'center',
                          background: pack.active ? 'rgba(37,99,235,0.12)' : 'rgba(15,23,42,0.05)',
                          color: pack.active ? 'var(--ol-blue)' : 'var(--ol-ink-3)',
                          flexShrink: 0,
                        }}
                      >
                        <Icon name={pack.kind === 'builtin' ? 'sparkle' : 'archive'} size={16} />
                      </div>
                    </div>

                    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', marginBottom: 12 }}>
                      <Pill tone={modeTone(pack.baseMode)} size="sm">{t(`style.modes.${pack.baseMode}.name`)}</Pill>
                      {pack.tags.slice(0, 3).map(tag => (
                        <Pill key={`${pack.id}-${tag}`} tone="default" size="sm">{tag}</Pill>
                      ))}
                    </div>

                    <div
                      style={{
                        fontSize: 12.5,
                        lineHeight: 1.6,
                        color: 'var(--ol-ink-2)',
                        background: '#fff',
                        borderRadius: 12,
                        border: '0.5px solid var(--ol-line)',
                        padding: 12,
                        minHeight: 90,
                        display: '-webkit-box',
                        WebkitBoxOrient: 'vertical',
                        WebkitLineClamp: 4,
                        overflow: 'hidden',
                        marginBottom: 12,
                      }}
                    >
                      {pack.prompt}
                    </div>

                    <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                      <Btn
                        size="sm"
                        variant={selected ? 'blue' : 'ghost'}
                        onClick={() => {
                          setSelectedId(pack.id);
                          setNotice(null);
                          setError(null);
                        }}
                      >
                        {store.details}
                      </Btn>
                      <Btn
                        size="sm"
                        variant={pack.active ? 'soft' : 'ghost'}
                        disabled={pack.active || busy === 'activating'}
                        onClick={() => void handleActivate(pack)}
                      >
                        {pack.active ? store.active : store.activate}
                      </Btn>
                      <Btn
                        size="sm"
                        variant="ghost"
                        disabled={busy === 'toggling'}
                        onClick={() => void handleToggleEnabled(pack)}
                      >
                        {pack.enabled ? store.disable : store.enable}
                      </Btn>
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        </Card>

        <Card padding={0} style={{ overflow: 'hidden' }}>
          <div style={{ padding: 18, borderBottom: '0.5px solid var(--ol-line)' }}>
            <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 12, flexWrap: 'wrap' }}>
              <div>
                <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.detailTitle}</div>
                <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 4 }}>{store.detailDesc}</div>
              </div>
              {selectedPack && (
                <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                  <Btn variant="ghost" icon="archive" onClick={() => void handleExportZip()} disabled={busy === 'exporting'}>
                    {store.exportZip}
                  </Btn>
                  <Btn
                    variant={selectedPack.active ? 'soft' : 'blue'}
                    icon="check"
                    disabled={selectedPack.active || busy === 'activating'}
                    onClick={() => void handleActivate(selectedPack)}
                  >
                    {selectedPack.active ? store.active : store.activate}
                  </Btn>
                </div>
              )}
            </div>
          </div>

          {!draft ? (
            <div style={{ padding: 28, color: 'var(--ol-ink-3)', fontSize: 13, lineHeight: 1.6 }}>
              {busy === 'loading' ? t('common.loading') : store.noSelection}
            </div>
          ) : (
            <div style={{ padding: 18, display: 'flex', flexDirection: 'column', gap: 16 }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
                <Pill tone={draft.kind === 'builtin' ? 'outline' : 'blue'}>
                  {draft.kind === 'builtin' ? store.builtin : store.imported}
                </Pill>
                <Pill tone={modeTone(draft.baseMode)}>{t(`style.modes.${draft.baseMode}.name`)}</Pill>
                {draft.active && <Pill tone="dark">{store.active}</Pill>}
                {dirty && <Pill tone="outline">{store.unsaved}</Pill>}
              </div>

              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 12 }}>
                <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                  <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.fieldName}</span>
                  <input
                    value={draft.name}
                    onChange={event => patchDraft({ name: event.target.value })}
                    style={inputStyle}
                  />
                </label>
                <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                  <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.fieldAuthor}</span>
                  <input
                    value={draft.author ?? ''}
                    onChange={event => patchDraft({ author: event.target.value || null })}
                    style={inputStyle}
                    placeholder={store.fieldAuthorPlaceholder}
                  />
                </label>
                <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                  <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.fieldVersion}</span>
                  <input
                    value={draft.version}
                    onChange={event => patchDraft({ version: event.target.value })}
                    style={inputStyle}
                  />
                </label>
                <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                  <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.fieldTags}</span>
                  <input
                    value={draft.tags.join(', ')}
                    onChange={event => patchDraft({ tags: event.target.value.split(',').map(value => value.trim()).filter(Boolean) })}
                    style={inputStyle}
                    placeholder={store.fieldTagsPlaceholder}
                  />
                </label>
              </div>

              <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.fieldDescription}</span>
                <textarea
                  value={draft.description}
                  onChange={event => patchDraft({ description: event.target.value })}
                  style={{ ...textareaStyle, minHeight: 86 }}
                />
              </label>

              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 12 }}>
                <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                  <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.fieldModel}</span>
                  <input
                    value={draft.recommendedModel ?? ''}
                    onChange={event => patchDraft({ recommendedModel: event.target.value || null })}
                    style={inputStyle}
                    placeholder={store.fieldModelPlaceholder}
                  />
                </label>
                <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                  <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.fieldCompatibility}</span>
                  <input
                    value={draft.compatibleAppVersion ?? ''}
                    onChange={event => patchDraft({ compatibleAppVersion: event.target.value || null })}
                    style={inputStyle}
                    placeholder={store.fieldCompatibilityPlaceholder}
                  />
                </label>
              </div>

              <label style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
                <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.fullPromptTitle}</span>
                <span style={{ fontSize: 11.5, color: 'var(--ol-ink-4)', lineHeight: 1.55 }}>{store.fullPromptHint}</span>
                <textarea
                  value={draft.prompt}
                  onChange={event => patchDraft({ prompt: event.target.value })}
                  style={{ ...textareaStyle, minHeight: 210 }}
                />
              </label>

              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12, flexWrap: 'wrap' }}>
                <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                  <Btn variant={dirty ? 'blue' : 'ghost'} icon="check" onClick={() => void handleSave()} disabled={!dirty || busy === 'saving'}>
                    {busy === 'saving' ? t('common.saving') : store.save}
                  </Btn>
                  <Btn
                    variant="ghost"
                    icon="refresh"
                    onClick={() => selectedPack && setDraft(clonePack(selectedPack))}
                    disabled={!dirty}
                  >
                    {store.revert}
                  </Btn>
                </div>
                <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                  {draft.kind === 'builtin' ? (
                    <Btn variant="soft" icon="refresh" onClick={() => void handleResetBuiltin()} disabled={busy === 'resetting'}>
                      {store.resetBuiltin}
                    </Btn>
                  ) : (
                    <Btn variant="soft" icon="trash" onClick={() => void handleDeleteImported()} disabled={busy === 'deleting'}>
                      {store.deleteImported}
                    </Btn>
                  )}
                  <Btn variant="ghost" icon="archive" onClick={() => void handleExportZip()} disabled={busy === 'exporting'}>
                    {store.exportZip}
                  </Btn>
                </div>
              </div>

              <div
                style={{
                  padding: 14,
                  borderRadius: 14,
                  background: 'linear-gradient(180deg, rgba(248,250,252,0.98), rgba(241,245,249,0.95))',
                  border: '0.5px solid var(--ol-line)',
                }}
              >
                <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 10 }}>
                  <div style={{ fontSize: 13, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.metaTitle}</div>
                  <Pill tone="default" size="sm">{draft.id}</Pill>
                </div>
                <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(160px, 1fr))', gap: 10 }}>
                  <MetaItem label={store.metaSource} value={draft.kind === 'builtin' ? store.builtin : store.imported} />
                  <MetaItem label={store.metaBaseMode} value={t(`style.modes.${draft.baseMode}.name`)} />
                  <MetaItem label={store.metaStatus} value={draft.enabled ? store.enabled : store.disabled} />
                  <MetaItem label={store.metaUpdatedAt} value={draft.updatedAt || '—'} />
                </div>
              </div>

              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12, flexWrap: 'wrap' }}>
                <div>
                  <div style={{ fontSize: 13, fontWeight: 600, color: 'var(--ol-ink)' }}>{store.examplesTitle}</div>
                  <div style={{ fontSize: 11.5, color: 'var(--ol-ink-4)', marginTop: 4 }}>{store.examplesDesc}</div>
                </div>
                <Btn variant="ghost" icon="plus" onClick={appendExample}>{store.addExample}</Btn>
              </div>

              <div style={{ display: 'grid', gap: 12 }}>
                {draft.examples.length === 0 && (
                  <Card padding={18} style={{ background: 'var(--ol-surface-2)' }}>
                    <div style={{ fontSize: 12.5, color: 'var(--ol-ink-3)', lineHeight: 1.6 }}>
                      {store.examplesEmpty}
                    </div>
                  </Card>
                )}

                {draft.examples.map((example, index) => (
                  <Card
                    key={`${draft.id}-example-${index}`}
                    padding={16}
                    style={{
                      background: 'linear-gradient(180deg, rgba(255,255,255,0.98), rgba(248,250,252,0.98))',
                    }}
                  >
                    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12, marginBottom: 12 }}>
                      <input
                        value={example.title ?? ''}
                        onChange={event => patchExample(index, { title: event.target.value })}
                        style={{ ...inputStyle, fontWeight: 600 }}
                        placeholder={store.exampleTitlePlaceholder(index + 1)}
                      />
                      <Btn variant="ghost" size="sm" icon="trash" onClick={() => removeExample(index)}>
                        {t('common.delete')}
                      </Btn>
                    </div>

                    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))', gap: 12 }}>
                      <div
                        style={{
                          borderRadius: 14,
                          border: '0.5px solid rgba(148,163,184,0.22)',
                          background: 'rgba(248,250,252,0.9)',
                          padding: 14,
                        }}
                      >
                        <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 10 }}>
                          <Pill tone="outline" size="sm">{store.exampleInput}</Pill>
                        </div>
                        <textarea
                          value={example.input}
                          onChange={event => patchExample(index, { input: event.target.value })}
                          style={{ ...textareaStyle, minHeight: 120, background: '#fff' }}
                        />
                      </div>

                      <div
                        style={{
                          borderRadius: 14,
                          border: '0.5px solid rgba(37,99,235,0.16)',
                          background: 'rgba(239,246,255,0.86)',
                          padding: 14,
                        }}
                      >
                        <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 10 }}>
                          <Pill tone="blue" size="sm">{store.exampleOutput}</Pill>
                        </div>
                        <textarea
                          value={example.output}
                          onChange={event => patchExample(index, { output: event.target.value })}
                          style={{ ...textareaStyle, minHeight: 120, background: '#fff' }}
                        />
                      </div>
                    </div>
                  </Card>
                ))}
              </div>
            </div>
          )}
        </Card>
      </div>
    </>
  );
}

function MetaItem({ label, value }: { label: string; value: string }) {
  return (
    <div
      style={{
        borderRadius: 12,
        border: '0.5px solid rgba(148,163,184,0.2)',
        background: 'rgba(255,255,255,0.92)',
        padding: '10px 12px',
      }}
    >
      <div style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: '.08em', color: 'var(--ol-ink-4)', marginBottom: 6 }}>
        {label}
      </div>
      <div style={{ fontSize: 12.5, lineHeight: 1.5, color: 'var(--ol-ink-2)', wordBreak: 'break-word' }}>{value}</div>
    </div>
  );
}

const inputStyle: CSSProperties = {
  width: '100%',
  boxSizing: 'border-box',
  minHeight: 38,
  padding: '9px 11px',
  borderRadius: 10,
  border: '0.5px solid var(--ol-line-strong)',
  background: '#fff',
  color: 'var(--ol-ink)',
  font: 'inherit',
  fontSize: 12.5,
};

const textareaStyle: CSSProperties = {
  width: '100%',
  boxSizing: 'border-box',
  padding: '11px 12px',
  borderRadius: 12,
  border: '0.5px solid var(--ol-line-strong)',
  background: '#fff',
  color: 'var(--ol-ink)',
  font: 'inherit',
  fontSize: 12.5,
  lineHeight: 1.65,
  resize: 'vertical',
};
