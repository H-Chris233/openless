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
  const copy = {
    kicker: 'STYLE PACKS',
    title: isEnglish ? 'Style Packs' : '风格包',
    desc: isEnglish
      ? 'Manage output styles as pluggable Style Packs. Built-in and imported packs share the same runtime prompt, ZIP exchange format, and repolish path.'
      : '把输出风格作为可插拔的 Style Pack 来管理。内置风格和导入风格共用同一套运行时 prompt、ZIP 交换格式和 repolish 链路。',
    loadFailed: (message: string) => (isEnglish ? `Failed to load style packs: ${message}` : `加载风格包失败：${message}`),
    importZip: isEnglish ? 'Import ZIP' : '导入 ZIP',
    exportZip: isEnglish ? 'Export ZIP' : '导出 ZIP',
    exportShort: isEnglish ? 'Export' : '导出',
    builtin: isEnglish ? 'Built-in' : '内置',
    imported: isEnglish ? 'Imported' : '导入',
    active: isEnglish ? 'Active' : '当前启用',
    enabled: isEnglish ? 'Enabled' : '已启用',
    disabled: isEnglish ? 'Disabled' : '已停用',
    activate: isEnglish ? 'Activate' : '启用',
    enable: isEnglish ? 'Enable' : '启用',
    disable: isEnglish ? 'Disable' : '停用',
    edit: isEnglish ? 'Edit' : '编辑',
    closeEditor: isEnglish ? 'Close' : '关闭',
    unsaved: isEnglish ? 'Unsaved' : '未保存',
    listTitle: isEnglish ? 'Local Packs' : '本地风格包',
    listDesc: isEnglish
      ? 'Browse and manage packs like a local storefront. Keep this page focused on cards, then open a dedicated editor only when you need to change prompt details.'
      : '像本地商店一样浏览和管理风格包。主页面只保留卡片浏览，真正修改 prompt 和示例时再打开独立编辑面板。',
    listCount: (count: number) => (isEnglish ? `${count} packs` : `${count} 个风格包`),
    save: isEnglish ? 'Save Pack' : '保存风格包',
    revert: isEnglish ? 'Discard Changes' : '放弃修改',
    saveSuccess: isEnglish ? 'Style pack saved.' : '风格包已保存',
    saveFailed: (message: string) => (isEnglish ? `Failed to save style pack: ${message}` : `保存风格包失败：${message}`),
    activateSuccess: (name: string) => (isEnglish ? `Activated "${name}".` : `已启用“${name}”`),
    activateFailed: (message: string) => (isEnglish ? `Failed to activate style pack: ${message}` : `启用风格包失败：${message}`),
    enableSuccess: (name: string) => (isEnglish ? `Enabled "${name}".` : `已启用“${name}”`),
    disableSuccess: (name: string) => (isEnglish ? `Disabled "${name}".` : `已停用“${name}”`),
    toggleFailed: (message: string) => (isEnglish ? `Failed to change pack status: ${message}` : `切换启用状态失败：${message}`),
    importSuccess: (name: string) => (isEnglish ? `Imported "${name}".` : `已导入“${name}”`),
    importFailed: (message: string) => (isEnglish ? `Failed to import ZIP: ${message}` : `导入 ZIP 失败：${message}`),
    exportSuccess: (path: string) => (isEnglish ? `Exported to ${path}` : `已导出到 ${path}`),
    exportFailed: (message: string) => (isEnglish ? `Failed to export ZIP: ${message}` : `导出 ZIP 失败：${message}`),
    resetBuiltin: isEnglish ? 'Reset Built-in Pack' : '重置内置风格',
    resetSuccess: (name: string) => (isEnglish ? `Reset "${name}".` : `已重置“${name}”`),
    resetFailed: (message: string) => (isEnglish ? `Failed to reset pack: ${message}` : `重置风格包失败：${message}`),
    deleteImported: isEnglish ? 'Delete Imported Pack' : '删除导入风格',
    deleteConfirm: (name: string) => (isEnglish
      ? `Delete "${name}"? This cannot be undone.`
      : `确定删除“${name}”吗？删除后无法恢复。`),
    deleteSuccess: (name: string) => (isEnglish ? `Deleted "${name}".` : `已删除“${name}”`),
    deleteFailed: (message: string) => (isEnglish ? `Failed to delete pack: ${message}` : `删除风格包失败：${message}`),
    summaryBuiltin: isEnglish ? 'Built-in Packs' : '内置风格',
    summaryBuiltinHint: isEnglish ? 'Default product semantics with one-click reset.' : '跟随产品默认语义，可一键重置到官方基线。',
    summaryImported: isEnglish ? 'Imported Packs' : '导入风格',
    summaryImportedHint: isEnglish ? 'Installed from ZIP and fully portable.' : '来自 ZIP 包，可启用、编辑、导出和删除。',
    summaryEnabled: isEnglish ? 'Rotation Ready' : '当前可轮换',
    summaryCurrent: (name: string) => (isEnglish ? `Current: ${name}` : `当前启用：${name}`),
    summaryCurrentEmpty: isEnglish ? 'No pack selected yet' : '还没有选中风格包',
    summaryFocused: (name: string) => (isEnglish ? `Focused: ${name}` : `当前浏览：${name}`),
    editorTitle: isEnglish ? 'Style Pack Editor' : '风格包编辑面板',
    editorDesc: isEnglish
      ? 'Edit the full runtime prompt, examples, tags, export, reset, and metadata here without crowding the main store view.'
      : '在这里集中编辑完整运行时 prompt、示例、标签、导入导出和重置操作，不再占满主列表空间。',
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
    fieldTagsPlaceholder: isEnglish ? 'Comma-separated tags, e.g. community, voiceover, formal' : '用英文逗号分隔，例如 community, voiceover, formal',
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
      : '这个风格包还没有示例。建议至少补一组输入/输出，方便导出后被别人理解和复用。',
    exampleTitlePlaceholder: (index: number) => (isEnglish ? `Example ${index} title` : `示例 ${index} 标题`),
    exampleInput: isEnglish ? 'Input' : '输入',
    exampleOutput: isEnglish ? 'Output' : '输出',
    examplesCount: (count: number) => (isEnglish ? `${count} examples` : `${count} 个示例`),
    discardCloseConfirm: isEnglish
      ? 'Discard unsaved changes and close the editor?'
      : '关闭编辑面板前要放弃未保存修改吗？',
    discardSwitchConfirm: (name: string) => (isEnglish
      ? `Discard unsaved changes and switch to "${name}"?`
      : `要放弃当前未保存修改，并切换到“${name}”吗？`),
  };

  const [packs, setPacks] = useState<StylePack[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [draft, setDraft] = useState<StylePack | null>(null);
  const [busy, setBusy] = useState<BusyAction>('loading');
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [editorOpen, setEditorOpen] = useState(false);

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
      setError(copy.loadFailed(String(loadError)));
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
      unlisten?.();
    };
  }, [selectedId]);

  const selectedPack = packs.find(pack => pack.id === selectedId) ?? null;
  const activePack = packs.find(pack => pack.active) ?? null;
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

  const focusPack = (packId: string) => {
    setSelectedId(packId);
    setNotice(null);
    setError(null);
  };

  const discardDraftChanges = () => {
    if (selectedPack) {
      setDraft(clonePack(selectedPack));
    }
  };

  const closeEditor = () => {
    if (dirty) {
      if (!window.confirm(copy.discardCloseConfirm)) {
        return;
      }
      discardDraftChanges();
    }
    setEditorOpen(false);
  };

  const openEditorForPack = (pack: StylePack) => {
    if (editorOpen && dirty && selectedPack && selectedPack.id !== pack.id) {
      if (!window.confirm(copy.discardSwitchConfirm(pack.name))) {
        return;
      }
    }
    focusPack(pack.id);
    setEditorOpen(true);
  };

  useEffect(() => {
    if (!editorOpen) return;
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        closeEditor();
      }
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => {
      document.body.style.overflow = previousOverflow;
      window.removeEventListener('keydown', handleKeyDown);
    };
  }, [editorOpen, dirty, selectedPack, draft]);

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
      showSuccess(copy.saveSuccess);
      await loadPacks(saved.id);
    } catch (saveError) {
      setError(copy.saveFailed(String(saveError)));
    } finally {
      setBusy(null);
    }
  };

  const handleActivate = async (pack: StylePack) => {
    setBusy('activating');
    try {
      await setActiveStylePack(pack.id);
      showSuccess(copy.activateSuccess(pack.name));
      await loadPacks(pack.id);
    } catch (activateError) {
      setError(copy.activateFailed(String(activateError)));
    } finally {
      setBusy(null);
    }
  };

  const handleToggleEnabled = async (pack: StylePack) => {
    setBusy('toggling');
    try {
      await setStylePackEnabled(pack.id, !pack.enabled);
      showSuccess(pack.enabled ? copy.disableSuccess(pack.name) : copy.enableSuccess(pack.name));
      await loadPacks(pack.id);
    } catch (toggleError) {
      setError(copy.toggleFailed(String(toggleError)));
    } finally {
      setBusy(null);
    }
  };

  const handleResetBuiltin = async () => {
    if (!selectedPack || selectedPack.kind !== 'builtin') return;
    setBusy('resetting');
    try {
      await resetBuiltinStylePack(selectedPack.id);
      showSuccess(copy.resetSuccess(selectedPack.name));
      await loadPacks(selectedPack.id);
    } catch (resetError) {
      setError(copy.resetFailed(String(resetError)));
    } finally {
      setBusy(null);
    }
  };

  const handleDeleteImported = async () => {
    if (!selectedPack || selectedPack.kind !== 'imported') return;
    if (!window.confirm(copy.deleteConfirm(selectedPack.name))) {
      return;
    }
    setBusy('deleting');
    try {
      await deleteStylePack(selectedPack.id);
      showSuccess(copy.deleteSuccess(selectedPack.name));
      setEditorOpen(false);
      await loadPacks();
    } catch (deleteError) {
      setError(copy.deleteFailed(String(deleteError)));
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
      showSuccess(copy.importSuccess(imported.name));
      await loadPacks(imported.id);
    } catch (importError) {
      setError(copy.importFailed(String(importError)));
    } finally {
      setBusy(null);
    }
  };

  const handleExportZip = async (pack = selectedPack) => {
    if (!pack) return;
    setBusy('exporting');
    try {
      const defaultName = `${sanitizeZipFileName(pack.name)}.zip`;
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
      const savedPath = await exportStylePackToZip(pack.id, targetPath);
      showSuccess(copy.exportSuccess(savedPath));
    } catch (exportError) {
      setError(copy.exportFailed(String(exportError)));
    } finally {
      setBusy(null);
    }
  };

  return (
    <>
      <PageHeader
        kicker={copy.kicker}
        title={copy.title}
        desc={copy.desc}
        right={(
          <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap', justifyContent: 'flex-end' }}>
            <Btn variant="ghost" icon="refresh" onClick={() => void loadPacks(selectedId)} disabled={busy === 'loading'}>
              {t('common.refresh')}
            </Btn>
            <Btn variant="blue" icon="archive" onClick={() => void handleImportZip()} disabled={busy === 'importing'}>
              {busy === 'importing' ? t('common.loading') : copy.importZip}
            </Btn>
          </div>
        )}
      />

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 12, marginBottom: 16 }}>
        <Card padding={16} glassy>
          <div style={{ fontSize: 11, letterSpacing: '.08em', textTransform: 'uppercase', color: 'var(--ol-ink-4)', marginBottom: 8 }}>
            {copy.summaryBuiltin}
          </div>
          <div style={{ fontSize: 24, fontWeight: 600, color: 'var(--ol-ink)' }}>{builtinCount}</div>
          <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 4 }}>{copy.summaryBuiltinHint}</div>
        </Card>
        <Card padding={16} glassy>
          <div style={{ fontSize: 11, letterSpacing: '.08em', textTransform: 'uppercase', color: 'var(--ol-ink-4)', marginBottom: 8 }}>
            {copy.summaryImported}
          </div>
          <div style={{ fontSize: 24, fontWeight: 600, color: 'var(--ol-ink)' }}>{importedCount}</div>
          <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 4 }}>{copy.summaryImportedHint}</div>
        </Card>
        <Card padding={16} glassy>
          <div style={{ fontSize: 11, letterSpacing: '.08em', textTransform: 'uppercase', color: 'var(--ol-ink-4)', marginBottom: 8 }}>
            {copy.summaryEnabled}
          </div>
          <div style={{ fontSize: 24, fontWeight: 600, color: 'var(--ol-ink)' }}>{enabledCount}</div>
          <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 4 }}>
            {activePack ? copy.summaryCurrent(activePack.name) : copy.summaryCurrentEmpty}
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

      <Card padding={0} style={{ overflow: 'hidden' }}>
        <div style={{ padding: 18, borderBottom: '0.5px solid var(--ol-line)' }}>
          <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 12, flexWrap: 'wrap' }}>
            <div>
              <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.listTitle}</div>
              <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 4, maxWidth: 760 }}>{copy.listDesc}</div>
            </div>
            <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', justifyContent: 'flex-end' }}>
              {selectedPack && <Pill tone="default">{copy.summaryFocused(selectedPack.name)}</Pill>}
              <Pill tone="outline">{copy.listCount(packs.length)}</Pill>
            </div>
          </div>
        </div>
        <div style={{ padding: 18 }}>
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(260px, 1fr))', gap: 12 }}>
            {packs.map(pack => {
              const selected = pack.id === selectedId;
              return (
                <div
                  key={pack.id}
                  style={{
                    textAlign: 'left',
                    border: '0.5px solid',
                    borderColor: selected || pack.active ? 'var(--ol-blue)' : 'var(--ol-line)',
                    background: pack.active
                      ? 'linear-gradient(180deg, rgba(239,246,255,0.92), rgba(255,255,255,0.98))'
                      : 'linear-gradient(180deg, rgba(255,255,255,0.98), rgba(248,250,252,0.92))',
                    borderRadius: 18,
                    padding: 16,
                    boxShadow: selected || pack.active ? '0 0 0 3px var(--ol-blue-ring)' : 'none',
                    cursor: 'default',
                    opacity: pack.enabled ? 1 : 0.72,
                    transition: 'border-color 0.16s var(--ol-motion-quick), box-shadow 0.18s var(--ol-motion-soft), opacity 0.18s var(--ol-motion-soft)',
                  }}
                >
                  <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 10, marginBottom: 12 }}>
                    <div style={{ minWidth: 0 }}>
                      <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', marginBottom: 6 }}>
                        <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--ol-ink)' }}>{pack.name}</div>
                        <Pill tone={pack.kind === 'builtin' ? 'outline' : 'blue'} size="sm">
                          {pack.kind === 'builtin' ? copy.builtin : copy.imported}
                        </Pill>
                        {pack.active && <Pill tone="dark" size="sm">{copy.active}</Pill>}
                        {!pack.enabled && <Pill tone="default" size="sm">{copy.disabled}</Pill>}
                      </div>
                      <div
                        style={{
                          fontSize: 12.5,
                          color: 'var(--ol-ink-3)',
                          lineHeight: 1.6,
                          display: '-webkit-box',
                          WebkitBoxOrient: 'vertical',
                          WebkitLineClamp: 3,
                          overflow: 'hidden',
                        }}
                      >
                        {pack.description}
                      </div>
                    </div>
                    <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-end', gap: 8, flexShrink: 0 }}>
                      <div
                        style={{
                          width: 36,
                          height: 36,
                          borderRadius: 12,
                          display: 'grid',
                          placeItems: 'center',
                          background: pack.active ? 'rgba(37,99,235,0.12)' : 'rgba(15,23,42,0.05)',
                          color: pack.active ? 'var(--ol-blue)' : 'var(--ol-ink-3)',
                        }}
                      >
                        <Icon name={pack.kind === 'builtin' ? 'sparkle' : 'archive'} size={16} />
                      </div>
                      <Btn
                        size="sm"
                        variant={selected ? 'blue' : 'ghost'}
                        icon="expand"
                        onClick={() => openEditorForPack(pack)}
                      >
                        {copy.edit}
                      </Btn>
                    </div>
                  </div>

                  <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', marginBottom: 10 }}>
                    <Pill tone={modeTone(pack.baseMode)} size="sm">{t(`style.modes.${pack.baseMode}.name`)}</Pill>
                    <Pill tone="default" size="sm">{copy.examplesCount(pack.examples.length)}</Pill>
                    {pack.tags.slice(0, 2).map(tag => (
                      <Pill key={`${pack.id}-${tag}`} tone="default" size="sm">{tag}</Pill>
                    ))}
                  </div>

                  <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', marginBottom: 14 }}>
                    <Pill tone="outline" size="sm">
                      {copy.metaSource}: {pack.kind === 'builtin' ? copy.builtin : copy.imported}
                    </Pill>
                    <Pill tone="outline" size="sm">
                      {copy.metaStatus}: {pack.enabled ? copy.enabled : copy.disabled}
                    </Pill>
                  </div>

                  <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                    <Btn
                      size="sm"
                      variant={pack.active ? 'soft' : 'ghost'}
                      disabled={pack.active || busy === 'activating'}
                      onClick={() => void handleActivate(pack)}
                    >
                      {pack.active ? copy.active : copy.activate}
                    </Btn>
                    <Btn
                      size="sm"
                      variant="ghost"
                      disabled={busy === 'toggling'}
                      onClick={() => void handleToggleEnabled(pack)}
                    >
                      {pack.enabled ? copy.disable : copy.enable}
                    </Btn>
                    <Btn
                      size="sm"
                      variant="ghost"
                      icon="archive"
                      disabled={busy === 'exporting'}
                      onClick={() => void handleExportZip(pack)}
                    >
                      {copy.exportShort}
                    </Btn>
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      </Card>

      {editorOpen && (
        <>
          <div
            aria-hidden="true"
            onClick={closeEditor}
            style={{
              position: 'fixed',
              inset: 0,
              background: 'rgba(15,23,42,0.24)',
              backdropFilter: 'blur(6px)',
              WebkitBackdropFilter: 'blur(6px)',
              zIndex: 40,
            }}
          />
          <div
            role="dialog"
            aria-modal="true"
            aria-label={copy.editorTitle}
            style={{
              position: 'fixed',
              top: 16,
              right: 16,
              bottom: 16,
              width: 'min(760px, calc(100vw - 32px))',
              zIndex: 41,
            }}
          >
            <Card
              padding={0}
              style={{
                height: '100%',
                display: 'grid',
                gridTemplateRows: 'auto minmax(0, 1fr)',
                overflow: 'hidden',
                boxShadow: '0 24px 80px rgba(15,23,42,0.22)',
              }}
            >
              <div style={{ padding: 18, borderBottom: '0.5px solid var(--ol-line)' }}>
                <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 12 }}>
                  <div style={{ minWidth: 0 }}>
                    <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.editorTitle}</div>
                    <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', marginTop: 4, lineHeight: 1.6 }}>{copy.editorDesc}</div>
                  </div>
                  <button
                    type="button"
                    onClick={closeEditor}
                    aria-label={copy.closeEditor}
                    style={{
                      width: 34,
                      height: 34,
                      borderRadius: 10,
                      border: '0.5px solid var(--ol-line)',
                      background: 'var(--ol-surface-2)',
                      color: 'var(--ol-ink-3)',
                      display: 'grid',
                      placeItems: 'center',
                      flexShrink: 0,
                    }}
                  >
                    <Icon name="close" size={15} />
                  </button>
                </div>
              </div>

              {!draft ? (
                <div style={{ padding: 28, color: 'var(--ol-ink-3)', fontSize: 13, lineHeight: 1.6 }}>
                  {busy === 'loading' ? t('common.loading') : copy.summaryCurrentEmpty}
                </div>
              ) : (
                <div className="ol-thinscroll" style={{ overflow: 'auto', padding: 18, display: 'flex', flexDirection: 'column', gap: 16 }}>
                  <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12, flexWrap: 'wrap' }}>
                    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
                      <Pill tone={draft.kind === 'builtin' ? 'outline' : 'blue'}>
                        {draft.kind === 'builtin' ? copy.builtin : copy.imported}
                      </Pill>
                      <Pill tone={modeTone(draft.baseMode)}>{t(`style.modes.${draft.baseMode}.name`)}</Pill>
                      {draft.active && <Pill tone="dark">{copy.active}</Pill>}
                      {dirty && <Pill tone="outline">{copy.unsaved}</Pill>}
                    </div>
                    <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                      <Btn variant="ghost" icon="archive" onClick={() => void handleExportZip()} disabled={busy === 'exporting'}>
                        {copy.exportZip}
                      </Btn>
                      <Btn
                        variant={draft.active ? 'soft' : 'blue'}
                        icon="check"
                        disabled={draft.active || busy === 'activating'}
                        onClick={() => void handleActivate(draft)}
                      >
                        {draft.active ? copy.active : copy.activate}
                      </Btn>
                    </div>
                  </div>

                  <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 12 }}>
                    <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                      <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.fieldName}</span>
                      <input
                        value={draft.name}
                        onChange={event => patchDraft({ name: event.target.value })}
                        style={inputStyle}
                      />
                    </label>
                    <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                      <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.fieldAuthor}</span>
                      <input
                        value={draft.author ?? ''}
                        onChange={event => patchDraft({ author: event.target.value || null })}
                        style={inputStyle}
                        placeholder={copy.fieldAuthorPlaceholder}
                      />
                    </label>
                    <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                      <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.fieldVersion}</span>
                      <input
                        value={draft.version}
                        onChange={event => patchDraft({ version: event.target.value })}
                        style={inputStyle}
                      />
                    </label>
                    <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                      <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.fieldTags}</span>
                      <input
                        value={draft.tags.join(', ')}
                        onChange={event => patchDraft({ tags: event.target.value.split(',').map(value => value.trim()).filter(Boolean) })}
                        style={inputStyle}
                        placeholder={copy.fieldTagsPlaceholder}
                      />
                    </label>
                  </div>

                  <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                    <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.fieldDescription}</span>
                    <textarea
                      value={draft.description}
                      onChange={event => patchDraft({ description: event.target.value })}
                      style={{ ...textareaStyle, minHeight: 86 }}
                    />
                  </label>

                  <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 12 }}>
                    <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                      <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.fieldModel}</span>
                      <input
                        value={draft.recommendedModel ?? ''}
                        onChange={event => patchDraft({ recommendedModel: event.target.value || null })}
                        style={inputStyle}
                        placeholder={copy.fieldModelPlaceholder}
                      />
                    </label>
                    <label style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                      <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.fieldCompatibility}</span>
                      <input
                        value={draft.compatibleAppVersion ?? ''}
                        onChange={event => patchDraft({ compatibleAppVersion: event.target.value || null })}
                        style={inputStyle}
                        placeholder={copy.fieldCompatibilityPlaceholder}
                      />
                    </label>
                  </div>

                  <label style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
                    <span style={{ fontSize: 12, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.fullPromptTitle}</span>
                    <span style={{ fontSize: 11.5, color: 'var(--ol-ink-4)', lineHeight: 1.55 }}>{copy.fullPromptHint}</span>
                    <textarea
                      value={draft.prompt}
                      onChange={event => patchDraft({ prompt: event.target.value })}
                      style={{ ...textareaStyle, minHeight: 210 }}
                    />
                  </label>

                  <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12, flexWrap: 'wrap' }}>
                    <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                      <Btn variant={dirty ? 'blue' : 'ghost'} icon="check" onClick={() => void handleSave()} disabled={!dirty || busy === 'saving'}>
                        {busy === 'saving' ? t('common.saving') : copy.save}
                      </Btn>
                      <Btn variant="ghost" icon="refresh" onClick={discardDraftChanges} disabled={!dirty}>
                        {copy.revert}
                      </Btn>
                    </div>
                    <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                      {draft.kind === 'builtin' ? (
                        <Btn variant="soft" icon="refresh" onClick={() => void handleResetBuiltin()} disabled={busy === 'resetting'}>
                          {copy.resetBuiltin}
                        </Btn>
                      ) : (
                        <Btn variant="soft" icon="trash" onClick={() => void handleDeleteImported()} disabled={busy === 'deleting'}>
                          {copy.deleteImported}
                        </Btn>
                      )}
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
                      <div style={{ fontSize: 13, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.metaTitle}</div>
                      <Pill tone="default" size="sm">{draft.id}</Pill>
                    </div>
                    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(160px, 1fr))', gap: 10 }}>
                      <MetaItem label={copy.metaSource} value={draft.kind === 'builtin' ? copy.builtin : copy.imported} />
                      <MetaItem label={copy.metaBaseMode} value={t(`style.modes.${draft.baseMode}.name`)} />
                      <MetaItem label={copy.metaStatus} value={draft.enabled ? copy.enabled : copy.disabled} />
                      <MetaItem label={copy.metaUpdatedAt} value={draft.updatedAt || '—'} />
                    </div>
                  </div>

                  <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12, flexWrap: 'wrap' }}>
                    <div>
                      <div style={{ fontSize: 13, fontWeight: 600, color: 'var(--ol-ink)' }}>{copy.examplesTitle}</div>
                      <div style={{ fontSize: 11.5, color: 'var(--ol-ink-4)', marginTop: 4 }}>{copy.examplesDesc}</div>
                    </div>
                    <Btn variant="ghost" icon="plus" onClick={appendExample}>{copy.addExample}</Btn>
                  </div>

                  <div style={{ display: 'grid', gap: 12 }}>
                    {draft.examples.length === 0 && (
                      <Card padding={18} style={{ background: 'var(--ol-surface-2)' }}>
                        <div style={{ fontSize: 12.5, color: 'var(--ol-ink-3)', lineHeight: 1.6 }}>
                          {copy.examplesEmpty}
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
                            placeholder={copy.exampleTitlePlaceholder(index + 1)}
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
                              <Pill tone="outline" size="sm">{copy.exampleInput}</Pill>
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
                              <Pill tone="blue" size="sm">{copy.exampleOutput}</Pill>
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
      )}
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
