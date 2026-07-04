// QaPanel.tsx — 划词追问浮窗 v2（issue #118 v2）。
//
// 触发链路：
//   1) 用户按 Cmd+Shift+;（默认）→ 后端 toggle 浮窗可见性；显示时发
//      `qa:state { kind: "idle", messages: [] }`。
//   2) 提问有两条并存的路径（用户拍板：文字 + 语音，不以全局热键为中心）：
//      · 文字：在底部输入框打字 → Enter → qa_submit_text。
//      · 语音：点输入框左侧麦克风（或复用听写键）→ 录音 → 再点/再按 → ASR + LLM。
//   3) 答案后可继续多轮追问，messages 累积。
//
// 关闭：Esc / ✕ / 再按 Cmd+Shift+; → qa_window_dismiss → 后端清历史 + 隐藏窗口。
//
// 视觉与 Less Computer 面板统一（shadcn 聊天卡片规范）：ChatHeader 标题 + 状态副行、
// MessageScroller 滚动区（新轮次锚顶 / 离底不跟随 / 跳到最新）、底部 Composer
// 文字输入（录音时内嵌胶囊同款 SiriGL 彩虹光条、思考时 ThinkingDots）。

import { useEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { useTranslation } from 'react-i18next';
import DOMPurify from 'dompurify';
import { useRafThrottle } from '../lib/useRafThrottle';
import { MessageScroller } from '../components/MessageScroller';
import { SiriGL } from '../components/SiriGL';
import { ThinkingDots } from '../components/ThinkingDots';
import {
  isTauri,
  qaSubmitText,
  qaToggleRecording,
  qaWindowDismiss,
  qaWindowPin,
} from '../lib/ipc';
import type { QaChatMessage, QaStatePayload } from '../lib/types';
import { renderQaMarkdown, renderQaPlainText } from '../lib/qaMarkdown';

const SELECTION_PREVIEW_MAX = 60;

type Status = 'idle' | 'recording' | 'thinking' | 'error';

interface QaPanelProps {
  embedded?: boolean;
  onRequestClose?: () => void;
}

// 浏览器预览（vite dev，非 Tauri）：?window=qa&demo=1 注入两轮演示对话，方便调样式。
function getPreviewMessages(): QaChatMessage[] {
  if (isTauri || typeof window === 'undefined') return [];
  if (new URLSearchParams(window.location.search).get('demo') !== '1') return [];
  return [
    {
      role: 'user',
      content:
        '# 选区原文\nThe mitochondria is the powerhouse of the cell.\n\n# 我的问题\n这句话为什么会成为梗？',
    },
    {
      role: 'assistant',
      content:
        '这句话出自初中生物课本，因为**被反复要求背诵**而深入人心。它简短好记、几乎人人见过，于是在英文互联网上被当作“唯一记得的知识点”反复调侃——任何需要往里塞一句正经知识的场合都能用它。',
    },
    {
      role: 'user',
      content: '那更严谨的说法是什么？',
    },
    {
      role: 'assistant',
      content:
        '线粒体主要通过**氧化磷酸化**生成 ATP，是细胞的“能量工厂”没错，但：\n\n1. 它不是唯一的能量来源（糖酵解在细胞质里进行）\n2. 它还参与钙离子调控、凋亡信号等\n\n所以课本那句是简化版，方便记忆但不完整。',
    },
  ];
}

export function QaPanel({ embedded = false, onRequestClose }: QaPanelProps = {}) {
  const { t } = useTranslation();
  const [messages, setMessages] = useState<QaChatMessage[]>(getPreviewMessages);
  const [status, setStatus] = useState<Status>('idle');
  const [errorMsg, setErrorMsg] = useState<string>('');
  const [selectionPreview, setSelectionPreview] = useState<string>('');
  const [composerText, setComposerText] = useState<string>('');
  const [pinned, setPinned] = useState(false);
  /** 流式 LLM 答案：answer_delta 累积、answer 事件来时清空（最终内容已落到 messages）。 */
  const [streamingAnswer, setStreamingAnswer] = useState<string>('');
  /** 录音电平：0..1。后端每帧 33ms 通过 qa:level emit。详见 issue #162。 */
  const [level, setLevel] = useState<number>(0);
  const tRef = useRef(t);
  tRef.current = t;
  const embeddedRef = useRef(embedded);
  embeddedRef.current = embedded;
  const onRequestCloseRef = useRef(onRequestClose);
  onRequestCloseRef.current = onRequestClose;

  // ── 后端事件订阅（mount 时订阅一次，永不重订阅）──────────────────
  useEffect(() => {
    if (!isTauri) return;
    let unlistenState: (() => void) | undefined;
    let unlistenDismiss: (() => void) | undefined;
    let unlistenLevel: (() => void) | undefined;
    let cancelled = false;
    (async () => {
      try {
        const { listen } = await import('@tauri-apps/api/event');
        const stateHandle = await listen<QaStatePayload>('qa:state', event => {
          const payload = event.payload;
          if (payload.messages) {
            setMessages(payload.messages);
          }
          switch (payload.kind) {
            case 'idle':
              setStatus('idle');
              setSelectionPreview('');
              setErrorMsg('');
              setStreamingAnswer('');
              setLevel(0);
              break;
            case 'recording':
              setStatus('recording');
              setSelectionPreview(payload.selection_preview ?? '');
              setErrorMsg('');
              setStreamingAnswer('');
              break;
            case 'loading':
              // ASR 在 finalize、user message 还没 push 的过渡帧。提前切到 thinking
              // 视图避免 UI 卡 recording 几百 ms 反馈缺失。详见 issue #161。
              setStatus('thinking');
              if (payload.selection_preview != null) {
                setSelectionPreview(payload.selection_preview);
              }
              setErrorMsg('');
              setStreamingAnswer('');
              setLevel(0);
              break;
            case 'thinking':
              setStatus('thinking');
              if (payload.selection_preview != null) {
                setSelectionPreview(payload.selection_preview);
              }
              setErrorMsg('');
              setStreamingAnswer('');
              setLevel(0);
              break;
            case 'answer_delta':
              // 流式增量。仍保持 thinking 状态——直到 answer 事件落定后才回 idle。
              if (payload.chunk) {
                setStreamingAnswer(prev => prev + payload.chunk);
              }
              break;
            case 'answer':
              setStatus('idle');
              setErrorMsg('');
              // messages 已被上面的 setMessages 落定，清掉流式 buffer 避免和最终气泡重影。
              setStreamingAnswer('');
              setLevel(0);
              break;
            case 'error':
              setStatus('error');
              setErrorMsg(payload.error ?? tRef.current('qa.error'));
              setStreamingAnswer('');
              setLevel(0);
              break;
          }
        });
        const dismissHandle = await listen<unknown>('qa:dismiss', () => {
          setPinned(false);
          setSelectionPreview('');
          setComposerText('');
          if (embeddedRef.current) {
            onRequestCloseRef.current?.();
          } else {
            void qaWindowDismiss();
          }
        });
        // qa:level — 录音电平，节流 ~33ms/帧。详见 issue #162。
        const levelHandle = await listen<{ level: number }>('qa:level', event => {
          setLevel(event.payload.level ?? 0);
        });
        if (cancelled) {
          stateHandle();
          dismissHandle();
          levelHandle();
        } else {
          unlistenState = stateHandle;
          unlistenDismiss = dismissHandle;
          unlistenLevel = levelHandle;
        }
      } catch (error) {
        console.error('[QaPanel] listener setup failed', error);
      }
    })();
    return () => {
      cancelled = true;
      unlistenState?.();
      unlistenDismiss?.();
      unlistenLevel?.();
    };
  }, []);

  // ── Esc 关闭 ────────────────────────────────────────────────────────
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        void qaWindowDismiss();
        onRequestClose?.();
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [onRequestClose]);

  const onTogglePin = () => {
    const next = !pinned;
    setPinned(next);
    void qaWindowPin(next);
  };

  const onClose = () => {
    void qaWindowDismiss();
    onRequestClose?.();
  };

  const onSubmitText = () => {
    const text = composerText.trim();
    if (!text || status === 'thinking' || status === 'recording') return;
    setComposerText('');
    void qaSubmitText(text).catch(error => {
      console.error('[QaPanel] qa_submit_text failed', error);
      setErrorMsg(error instanceof Error ? error.message : String(error));
      setStatus('error');
    });
  };

  const onToggleRecording = () => {
    if (status === 'thinking') return;
    void qaToggleRecording().catch(error => {
      console.error('[QaPanel] qa_toggle_recording failed', error);
    });
  };

  // 新轮次锚定信号：用户消息条数变化 = 新提问发出（MessageScroller 把它定位到视口顶部）。
  const userTurnCount = messages.filter(m => m.role === 'user').length;

  // Header 副行：活动状态优先，空闲显示欢迎语（不再提「按某某键」）。
  const headerSubtitle =
    status === 'recording'
      ? t('qa.statusRecording')
      : status === 'thinking'
        ? t('qa.thinking')
        : status === 'error'
          ? t('qa.statusError')
          : t('qa.headerHint');
  const headerTone: 'active' | 'error' | 'idle' =
    status === 'recording' || status === 'thinking' ? 'active' : status === 'error' ? 'error' : 'idle';

  return (
    <div style={embedded ? embeddedShellStyle : shellStyle}>
      <ChatHeader
        title={t('qa.title')}
        subtitle={headerSubtitle}
        tone={headerTone}
        pinned={pinned}
        onTogglePin={onTogglePin}
        onClose={onClose}
        embedded={embedded}
        t={t}
      />
      {/* 滚动行为遵循聊天滚动标准（MessageScroller）：新轮次锚定视口顶部、
          读者离底不跟随、屏外增长浮现「跳到最新」。 */}
      <MessageScroller
        anchorKey={userTurnCount || undefined}
        busy={status === 'thinking'}
        jumpLabel={t('qa.jumpToLatest')}
        style={scrollWrapStyle}
        contentStyle={contentStyle}
      >
        {messages.length === 0 && status === 'idle' && <EmptyHint t={t} />}
        {messages.map((message, i) => (
          <MessageRow key={i} message={message} />
        ))}
        {streamingAnswer && <AssistantText markdown={streamingAnswer} streaming />}
        {status === 'error' && <ErrorRow message={errorMsg} t={t} />}
      </MessageScroller>
      {status === 'recording' && selectionPreview && (
        <SelectionChip text={selectionPreview} t={t} />
      )}
      <Composer
        value={composerText}
        status={status}
        level={level}
        t={t}
        onChange={setComposerText}
        onSubmit={onSubmitText}
        onToggleRecording={onToggleRecording}
      />
    </div>
  );
}

// ── 子组件 ────────────────────────────────────────────────────────────

/**
 * 聊天卡片头（与 Less Computer 面板统一）：标题 + 状态副行常驻上方，右上角图钉 + ✕。
 * 整条作为拖动把手（macOS 走 NSWindow movableByWindowBackground，Windows 走
 * data-tauri-drag-region）；图钉/✕ 作为 button 子元素仍正常 click。
 */
function ChatHeader({
  title,
  subtitle,
  tone,
  pinned,
  onTogglePin,
  onClose,
  embedded,
  t,
}: {
  title: string;
  subtitle: string;
  tone: 'active' | 'error' | 'idle';
  pinned: boolean;
  onTogglePin: () => void;
  onClose: () => void;
  embedded: boolean;
  t: ReturnType<typeof useTranslation>['t'];
}) {
  return (
    <div
      data-tauri-drag-region
      style={{ ...(embedded ? embeddedHeaderStyle : headerStyle), cursor: embedded ? 'default' : 'grab' }}
    >
      <div data-tauri-drag-region style={{ flex: 1, minWidth: 0 }}>
        <div data-tauri-drag-region style={headerTitleStyle}>
          {title}
        </div>
        <div
          data-tauri-drag-region
          role={tone === 'active' ? 'status' : undefined}
          style={{
            ...headerSubtitleStyle,
            color: tone === 'error' ? 'var(--ol-err)' : 'var(--ol-ink-3)',
          }}
        >
          {subtitle || ' '}
        </div>
      </div>
      <IconBtn
        label={pinned ? t('qa.unpinTooltip') : t('qa.pinTooltip')}
        active={pinned}
        onClick={onTogglePin}
      >
        <svg width="13" height="13" viewBox="0 0 16 16" fill="none">
          <path
            d="M10.5 2L14 5.5L11.5 8L9.5 7L7 9.5L6.5 9L4 11.5L3 13L4.5 11.5L7 9L6.5 8.5L9 6L8 4L10.5 2Z"
            stroke="currentColor"
            strokeWidth="1.2"
            strokeLinejoin="round"
            fill={pinned ? 'currentColor' : 'none'}
          />
        </svg>
      </IconBtn>
      <IconBtn label={t('qa.closeTooltip')} onClick={onClose}>
        <svg width="11" height="11" viewBox="0 0 11 11">
          <path
            d="M1.5 1.5l8 8M9.5 1.5l-8 8"
            stroke="currentColor"
            strokeWidth="1.6"
            strokeLinecap="round"
          />
        </svg>
      </IconBtn>
    </div>
  );
}

interface IconBtnProps {
  label: string;
  active?: boolean;
  onClick: () => void;
  children: React.ReactNode;
}

function IconBtn({ label, active, onClick, children }: IconBtnProps) {
  return (
    <button
      onClick={onClick}
      onMouseDown={event => {
        // header 整条可拖动；按钮自身不触发拖拽，否则 click 被 startDragging 吞掉。
        event.stopPropagation();
      }}
      title={label}
      aria-label={label}
      style={{
        ...iconBtnBaseStyle,
        color: active ? 'var(--ol-blue)' : 'var(--ol-ink-3)',
        background: active ? 'rgba(37,99,235,0.12)' : 'transparent',
      }}
    >
      {children}
    </button>
  );
}

function EmptyHint({ t }: { t: ReturnType<typeof useTranslation>['t'] }) {
  return (
    <div style={emptyHintStyle}>
      <div style={emptyIconStyle} aria-hidden>
        <svg width="22" height="22" viewBox="0 0 24 24" fill="none">
          <path
            d="M4 5.5A1.5 1.5 0 0 1 5.5 4h13A1.5 1.5 0 0 1 20 5.5v9A1.5 1.5 0 0 1 18.5 16H9l-4 3.5V16H5.5A1.5 1.5 0 0 1 4 14.5v-9Z"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinejoin="round"
            strokeDasharray="2.6 2.4"
          />
        </svg>
      </div>
      <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--ol-ink)' }}>
        {t('qa.emptyTitle')}
      </div>
      <div style={{ fontSize: 12, color: 'var(--ol-ink-3)', lineHeight: 1.6, maxWidth: 260 }}>
        {t('qa.emptyDesc')}
      </div>
    </div>
  );
}

function MessageRow({ message }: { message: QaChatMessage }) {
  if (message.role === 'user') {
    // 第一轮可能含 "# 选区原文 ... # 我的问题 ..." → 抽出问题单独显示，选区作引用块淡显在上。
    const { selection, question } = splitFirstTurnUser(message.content);
    return (
      // data-scroll-anchor：用户消息是新轮次的锚点行（MessageScroller 定位到视口顶部附近）。
      <div
        className="qa-enter"
        data-scroll-anchor
        style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-end', gap: 4 }}
      >
        {selection && (
          <div style={selectionQuoteStyle}>
            <span style={{ color: 'var(--ol-ink-4)', marginRight: 4 }}>“</span>
            {truncate(selection, 120)}
            <span style={{ color: 'var(--ol-ink-4)', marginLeft: 4 }}>”</span>
          </div>
        )}
        <div style={userBubbleStyle}>{question}</div>
      </div>
    );
  }
  return <AssistantText markdown={message.content} streaming={false} />;
}

/** 助手文本段：无气泡的文档流（与 Less Computer 面板一致），流式时末尾带光标。 */
function AssistantText({ markdown, streaming }: { markdown: string; streaming: boolean }) {
  // 按帧率节流：流式回复逐 token 全量 parse + DOMPurify 是 O(n²)，长回复越来越卡。
  const throttled = useRafThrottle(markdown);
  const html = useMemo(() => {
    let rendered: string;
    try {
      rendered = renderQaMarkdown(throttled);
    } catch (error) {
      console.error('[qa] markdown render failed', error);
      rendered = renderQaPlainText(String(throttled ?? ''));
    }
    // 兜底再消毒：qaMarkdown 已转义 raw HTML token，DOMPurify 多一道防线。
    return DOMPurify.sanitize(rendered, { ADD_ATTR: ['target', 'rel'] });
  }, [throttled]);
  return (
    <div
      className="qa-enter"
      style={{ display: 'flex', flexDirection: 'column', alignItems: 'stretch', gap: 3 }}
    >
      <div
        className="qa-answer"
        style={assistantTextStyle}
        // eslint-disable-next-line react/no-danger
        dangerouslySetInnerHTML={{ __html: html }}
      />
      {streaming && <span style={caretStyle} />}
    </div>
  );
}

function splitFirstTurnUser(content: string): { selection: string; question: string } {
  // 后端拼法：`# 选区原文\n{sel}\n\n# 我的问题\n{q}`。简单 split，对齐 coordinator.rs 的写法。
  const m = content.match(/^# 选区原文\n([\s\S]*?)\n\n# 我的问题\n([\s\S]+)$/);
  if (!m) return { selection: '', question: content };
  return { selection: m[1].trim(), question: m[2].trim() };
}

/** 录音时的选区上下文条：贴在输入区上方，让用户看到「在追问哪段文字」。 */
function SelectionChip({ text, t }: { text: string; t: ReturnType<typeof useTranslation>['t'] }) {
  const truncated = useMemo(() => truncate(text, SELECTION_PREVIEW_MAX), [text]);
  return (
    <div style={selectionChipStyle}>
      <span style={{ color: 'var(--ol-ink-4)', marginRight: 6, flexShrink: 0 }}>
        {t('qa.selectionPreview')}
      </span>
      <span style={{ color: 'var(--ol-ink-2)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
        {truncated}
      </span>
    </div>
  );
}

function ErrorRow({ message, t }: { message: string; t: ReturnType<typeof useTranslation>['t'] }) {
  return (
    <div className="qa-enter" style={errorRowStyle}>
      <div style={{ fontSize: 12.5, color: 'var(--ol-err)', lineHeight: 1.55 }}>{message}</div>
      <div style={{ fontSize: 11.5, color: 'var(--ol-ink-4)' }}>{t('qa.errorRetryHint')}</div>
    </div>
  );
}

/**
 * 底部输入区（与 Less Computer 面板 Composer 统一）：麦克风 + 文字输入 + 发送。
 * 文字与语音并存（用户拍板，不以全局热键为中心）：
 *   · 麦克风按钮点击 = 开/停语音（qa_toggle_recording），录音键仍可用但不再是唯一入口。
 *   · 录音时输入框位置内嵌胶囊同款 SiriGL 彩虹光条（真实电平驱动）。
 *   · 思考时显示 ThinkingDots + 文案。
 */
function Composer({
  value,
  status,
  level,
  t,
  onChange,
  onSubmit,
  onToggleRecording,
}: {
  value: string;
  status: Status;
  level: number;
  t: ReturnType<typeof useTranslation>['t'];
  onChange: (value: string) => void;
  onSubmit: () => void;
  onToggleRecording: () => void;
}) {
  const recording = status === 'recording';
  const thinking = status === 'thinking';
  const canSubmit = value.trim().length > 0 && !recording && !thinking;
  return (
    <div style={composerStyle}>
      <button
        type="button"
        onClick={onToggleRecording}
        onMouseDown={event => event.stopPropagation()}
        disabled={thinking}
        aria-label={recording ? t('qa.micStop') : t('qa.micLabel')}
        title={recording ? t('qa.micStop') : t('qa.micLabel')}
        aria-pressed={recording}
        style={{
          ...micButtonStyle,
          background: recording ? 'rgba(220,38,38,0.12)' : 'var(--ol-surface-2)',
          color: recording ? 'var(--ol-err)' : 'var(--ol-ink-3)',
          opacity: thinking ? 0.45 : 1,
        }}
      >
        <svg width="15" height="15" viewBox="0 0 16 16" fill="none">
          <rect x="5.5" y="1.5" width="5" height="8.5" rx="2.5" stroke="currentColor" strokeWidth="1.3" />
          <path
            d="M3.5 7.5a4.5 4.5 0 0 0 9 0M8 12v2.5"
            stroke="currentColor"
            strokeWidth="1.3"
            strokeLinecap="round"
          />
        </svg>
      </button>
      {recording ? (
        <div style={{ flex: 1, minWidth: 0, height: 38, position: 'relative' }}>
          <SiriGL
            mode="wave"
            level={level}
            resolved
            style={{ position: 'absolute', inset: 0, width: '100%', height: '100%' }}
          />
        </div>
      ) : thinking ? (
        <div style={{ flex: 1, minWidth: 0, display: 'flex', alignItems: 'center', gap: 8, height: 38 }}>
          <ThinkingDots size={18} />
          <span style={{ fontSize: 12, color: 'var(--ol-ink-3)', fontWeight: 500 }}>
            {t('qa.thinking')}
          </span>
        </div>
      ) : (
        <textarea
          value={value}
          rows={1}
          placeholder={t('qa.composerPlaceholder')}
          onChange={event => onChange(event.currentTarget.value)}
          onKeyDown={event => {
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault();
              if (canSubmit) onSubmit();
            }
          }}
          style={composerInputStyle}
        />
      )}
      <button
        type="button"
        disabled={!canSubmit}
        onClick={onSubmit}
        onMouseDown={event => event.stopPropagation()}
        aria-label={t('qa.composerSend')}
        title={t('qa.composerSend')}
        style={{ ...sendButtonStyle, opacity: canSubmit ? 1 : 0.4 }}
      >
        <svg width="13" height="13" viewBox="0 0 13 13" aria-hidden>
          <path
            d="M6.5 11V2M2.5 6 6.5 2l4 4"
            stroke="currentColor"
            strokeWidth="1.7"
            fill="none"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
      </button>
    </div>
  );
}

function truncate(text: string, max: number): string {
  if (text.length <= max) return text;
  return `${text.slice(0, max)}…`;
}

// ── 样式（shadcn 聊天卡片语言，与 Less Computer 面板统一，主题 token 自适应浅/深色）──
// 实底 + 细边框 + 弱阴影，不写 backdrop-filter（webview 模糊不了透明窗口背后的桌面，
// issue #470 结论）。

const shellStyle: CSSProperties = {
  width: '100%',
  height: '100vh',
  display: 'flex',
  flexDirection: 'column',
  borderRadius: 14,
  overflow: 'hidden',
  border: '0.5px solid var(--ol-line)',
  background: 'var(--ol-surface)',
  boxShadow: '0 18px 44px -22px rgba(0,0,0,.4)',
  fontFamily: 'var(--ol-font-sans)',
  color: 'var(--ol-ink)',
  isolation: 'isolate',
};

const embeddedShellStyle: CSSProperties = {
  ...shellStyle,
  height: '100%',
  minHeight: '100vh',
  borderRadius: 0,
  border: 0,
  boxShadow: 'none',
};

const headerStyle: CSSProperties = {
  height: 56,
  display: 'flex',
  alignItems: 'center',
  gap: 6,
  padding: '0 10px 0 14px',
  borderBottom: '0.5px solid var(--ol-line)',
  flexShrink: 0,
  position: 'relative',
  zIndex: 1,
};

const embeddedHeaderStyle: CSSProperties = {
  ...headerStyle,
  height: 'calc(56px + env(safe-area-inset-top, 0px))',
  padding: 'env(safe-area-inset-top, 0px) 10px 0 14px',
};

const headerTitleStyle: CSSProperties = {
  fontSize: 13,
  fontWeight: 600,
  color: 'var(--ol-ink)',
  lineHeight: '18px',
  whiteSpace: 'nowrap',
  overflow: 'hidden',
  textOverflow: 'ellipsis',
};

const headerSubtitleStyle: CSSProperties = {
  fontSize: 11.5,
  fontWeight: 500,
  lineHeight: '16px',
  whiteSpace: 'nowrap',
  overflow: 'hidden',
  textOverflow: 'ellipsis',
  minHeight: 16,
};

const iconBtnBaseStyle: CSSProperties = {
  width: 24,
  height: 24,
  border: 0,
  borderRadius: 6,
  display: 'inline-flex',
  alignItems: 'center',
  justifyContent: 'center',
  cursor: 'default',
  padding: 0,
  flexShrink: 0,
  transition: 'background 0.16s var(--ol-motion-quick), color 0.16s var(--ol-motion-quick)',
};

// 外层占满剩余高度；overflow 由 MessageScroller 内部 viewport 接管。
const scrollWrapStyle: CSSProperties = {
  flex: 1,
  minHeight: 0,
  zIndex: 1,
};

const contentStyle: CSSProperties = {
  // 至少撑满 viewport：EmptyHint 的 margin:auto 垂直居中依赖父容器有高度。
  minHeight: '100%',
  boxSizing: 'border-box',
  padding: 16,
  display: 'flex',
  flexDirection: 'column',
  gap: 12,
};

const emptyHintStyle: CSSProperties = {
  margin: 'auto',
  display: 'flex',
  flexDirection: 'column',
  alignItems: 'center',
  textAlign: 'center',
  gap: 8,
  padding: '0 8px',
};

const emptyIconStyle: CSSProperties = {
  width: 44,
  height: 44,
  borderRadius: 12,
  display: 'inline-flex',
  alignItems: 'center',
  justifyContent: 'center',
  background: 'var(--ol-surface-2)',
  color: 'var(--ol-ink-3)',
  marginBottom: 2,
};

const userBubbleStyle: CSSProperties = {
  maxWidth: '85%',
  padding: '8px 12px',
  borderRadius: 14,
  borderBottomRightRadius: 4,
  background: 'var(--ol-accent-solid-bg)',
  boxShadow: '0 2px 10px -6px rgba(37, 99, 235, 0.5)',
  color: 'var(--ol-accent-solid-ink)',
  fontSize: 13,
  lineHeight: 1.55,
  wordBreak: 'break-word',
};

const selectionQuoteStyle: CSSProperties = {
  maxWidth: '85%',
  padding: '6px 10px',
  borderRadius: 'var(--ol-r-md)',
  background: 'var(--ol-surface-2)',
  border: '0.5px solid var(--ol-line)',
  fontSize: 11.5,
  color: 'var(--ol-ink-3)',
  fontStyle: 'italic',
  lineHeight: 1.5,
};

/** 助手文本：无气泡的文档流。 */
const assistantTextStyle: CSSProperties = {
  fontSize: 13,
  lineHeight: 1.6,
  color: 'var(--ol-ink)',
  wordBreak: 'break-word',
};

const caretStyle: CSSProperties = {
  display: 'inline-block',
  width: 6,
  height: 12,
  background: 'var(--ol-blue)',
  animation: 'qa-pulse 0.9s var(--ol-motion-soft) infinite',
  borderRadius: 1,
};

const selectionChipStyle: CSSProperties = {
  flexShrink: 0,
  display: 'flex',
  alignItems: 'center',
  padding: '6px 14px',
  fontSize: 11.5,
  lineHeight: 1.5,
  borderTop: '0.5px solid var(--ol-line)',
  background: 'var(--ol-surface-2)',
};

const errorRowStyle: CSSProperties = {
  display: 'flex',
  flexDirection: 'column',
  gap: 4,
  padding: '8px 12px',
  borderRadius: 10,
  background: 'rgba(239,68,68,0.08)',
  border: '0.5px solid rgba(239,68,68,0.24)',
};

const composerStyle: CSSProperties = {
  flexShrink: 0,
  display: 'flex',
  alignItems: 'center',
  gap: 8,
  padding: '8px 10px',
  borderTop: '0.5px solid var(--ol-line)',
  position: 'relative',
  zIndex: 1,
};

const composerInputStyle: CSSProperties = {
  flex: 1,
  minWidth: 0,
  resize: 'none',
  border: '0.5px solid var(--ol-line-strong)',
  borderRadius: 10,
  padding: '9px 12px',
  fontFamily: 'var(--ol-font-sans)',
  fontSize: 13,
  lineHeight: 1.5,
  color: 'var(--ol-ink)',
  background: 'var(--ol-surface-2)',
  outline: 'none',
  maxHeight: 92,
};

const micButtonStyle: CSSProperties = {
  width: 32,
  height: 32,
  borderRadius: 999,
  border: 0,
  flexShrink: 0,
  display: 'inline-flex',
  alignItems: 'center',
  justifyContent: 'center',
  cursor: 'default',
  padding: 0,
  transition: 'background 0.16s var(--ol-motion-quick), color 0.16s var(--ol-motion-quick)',
};

const sendButtonStyle: CSSProperties = {
  width: 30,
  height: 30,
  borderRadius: 999,
  border: 0,
  flexShrink: 0,
  display: 'inline-flex',
  alignItems: 'center',
  justifyContent: 'center',
  cursor: 'default',
  padding: 0,
  background: 'var(--ol-accent-solid-bg)',
  color: 'var(--ol-accent-solid-ink)',
};

const globalCss = `
@keyframes qa-pulse {
  0%, 100% { opacity: 1; transform: scale(1); }
  50%      { opacity: 0.35; transform: scale(.94); }
}
@keyframes qa-enter {
  from { opacity: 0; transform: translateY(6px); }
  to   { opacity: 1; transform: translateY(0); }
}
.qa-enter { animation: qa-enter 0.28s var(--ol-motion-soft, cubic-bezier(.16,1,.3,1)) both; }
@media (prefers-reduced-motion: reduce) {
  .qa-enter { animation: none; }
}
.qa-answer p        { margin: 0 0 6px; }
.qa-answer p:last-child { margin-bottom: 0; }
.qa-answer h1,
.qa-answer h2,
.qa-answer h3       { margin: 10px 0 5px; font-weight: 600; line-height: 1.35; }
.qa-answer h1       { font-size: 15px; }
.qa-answer h2       { font-size: 14px; }
.qa-answer h3       { font-size: 13px; }
.qa-answer ul,
.qa-answer ol       { margin: 0 0 6px; padding-left: 18px; }
.qa-answer li       { margin: 2px 0; }
.qa-answer code     { font-family: var(--ol-font-mono); font-size: 12px;
                      padding: 1px 5px; border-radius: 4px;
                      background: var(--ol-surface-2); }
.qa-answer pre      { margin: 0 0 6px; padding: 8px 10px;
                      border-radius: var(--ol-control-radius); background: var(--ol-surface-2);
                      overflow-x: auto; }
.qa-answer pre code { padding: 0; background: transparent; }
.qa-answer a        { color: var(--ol-blue); text-decoration: none; }
.qa-answer a:hover  { text-decoration: underline; }
.qa-answer blockquote { margin: 0 0 6px; padding: 4px 0 4px 8px;
                        border-left: 2px solid var(--ol-line-strong);
                        color: var(--ol-ink-3); }
.qa-answer hr       { border: 0; border-top: 0.5px solid var(--ol-line);
                      margin: 8px 0; }
`;

if (typeof document !== 'undefined' && !document.getElementById('qa-panel-style')) {
  const tag = document.createElement('style');
  tag.id = 'qa-panel-style';
  tag.textContent = globalCss;
  document.head.appendChild(tag);
}
