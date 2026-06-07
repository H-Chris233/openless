// LessComputerPanel.tsx — Less Computer 语音 Agent 浮窗（窗口 label = "less-computer"）。
//
// 把「按住专用键说话 → Agent 操控电脑」的交互渲染成聊天结构：
//   - 用户气泡：语音指令转写（`user` 事件，开启新会话并清空旧内容）。
//   - 助手气泡：流式回复（`delta` 累积）+ 工具调用 chip（`tool`）。
//   - 内联审批卡（`approval`）：高风险动作被护栏拦下时弹 Approve / Deny。
//   - 完成（`completed`）落最终结果 + 成本；出错（`error`）红色样式。
//
// 窗口随内容自适应高度（measure content → setSize），不可拖动、置顶、磨砂。
// 仅 macOS 实际触发（后端只在 macOS 注册 Less Computer 热键并 emit 事件）。
// 关闭：Esc / ✕ → less_computer_window_dismiss → 后端隐藏窗口。

import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from 'react';
import { useTranslation } from 'react-i18next';
import DOMPurify from 'dompurify';
import {
  isTauri,
  lessComputerApprove,
  lessComputerWindowDismiss,
  lessComputerWindowResize,
} from '../lib/ipc';
import type { LessComputerEvent } from '../lib/types';
import { renderQaMarkdown, renderQaPlainText } from '../lib/qaMarkdown';

type RunStatus = 'idle' | 'working' | 'done' | 'error' | 'cancelled';

interface ToolChip {
  kind: 'tool';
  name: string;
}

interface ApprovalCard {
  kind: 'approval';
  token: string;
  command: string;
  reason: string;
  /** 用户已点过的结果，决定按钮禁用态。undefined = 待处理。 */
  decision?: 'approved' | 'denied';
}

/** 助手回复流里穿插的工具 chip / 审批卡，按到达顺序排列。 */
type Activity = ToolChip | ApprovalCard;

/** 一轮对话：用户一句 + 助手流式回复 + 其间的工具/审批 + 本轮收尾态。连续对话累积成数组。 */
interface Turn {
  user: string;
  answer: string;
  activities: Activity[];
  status: RunStatus;
  errorMsg: string;
  costUsd: number | null;
}

/** 对 turns 数组「最后一轮」做不可变更新。 */
function updateLastTurn(turns: Turn[], fn: (t: Turn) => Turn): Turn[] {
  if (turns.length === 0) return turns;
  return [...turns.slice(0, -1), fn(turns[turns.length - 1])];
}

const WINDOW_MIN_HEIGHT = 120;
const WINDOW_MAX_HEIGHT = 520;
const TOOLBAR_HEIGHT = 28;

export function LessComputerPanel() {
  const { t } = useTranslation();
  // 连续对话：每按一次说话键追加一轮（除非后端标记 fresh=新会话则清空重开）。
  const [turns, setTurns] = useState<Turn[]>([]);

  // ── 后端事件订阅（mount 一次）────────────────────────────────────────
  useEffect(() => {
    if (!isTauri) return;
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    (async () => {
      try {
        const { listen } = await import('@tauri-apps/api/event');
        const handle = await listen<LessComputerEvent>(
          'less-computer:event',
          event => applyEvent(event.payload),
        );
        if (cancelled) handle();
        else unlisten = handle;
      } catch (error) {
        console.error('[LessComputer] listener setup failed', error);
      }
    })();
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const applyEvent = (ev: LessComputerEvent) => {
    switch (ev.kind) {
      case 'user': {
        // 一轮新对话。fresh=true（后端无可续会话→新会话）则清空历史重开；否则追加为后续轮次。
        const fresh: Turn = {
          user: ev.text,
          answer: '',
          activities: [],
          status: 'working',
          errorMsg: '',
          costUsd: null,
        };
        setTurns(prev => (ev.fresh ? [fresh] : [...prev, fresh]));
        break;
      }
      case 'started':
        setTurns(prev => updateLastTurn(prev, tn => ({ ...tn, status: 'working' })));
        break;
      case 'delta':
        setTurns(prev => updateLastTurn(prev, tn => ({ ...tn, answer: tn.answer + ev.text })));
        break;
      case 'tool':
        setTurns(prev =>
          updateLastTurn(prev, tn => ({
            ...tn,
            activities: [...tn.activities, { kind: 'tool', name: ev.name }],
          })),
        );
        break;
      case 'approval':
        setTurns(prev =>
          updateLastTurn(prev, tn => ({
            ...tn,
            activities: [
              ...tn.activities,
              { kind: 'approval', token: ev.token, command: ev.command, reason: ev.reason },
            ],
          })),
        );
        break;
      case 'completed':
        setTurns(prev =>
          updateLastTurn(prev, tn => ({
            ...tn,
            answer: ev.text || tn.answer,
            costUsd: ev.costUsd ?? null,
            status: 'done',
          })),
        );
        break;
      case 'error':
        setTurns(prev => updateLastTurn(prev, tn => ({ ...tn, errorMsg: ev.message, status: 'error' })));
        break;
      case 'cancelled':
        setTurns(prev => updateLastTurn(prev, tn => ({ ...tn, status: 'cancelled' })));
        break;
    }
  };

  const onApproval = (token: string, approved: boolean) => {
    setTurns(prev =>
      prev.map(tn => ({
        ...tn,
        activities: tn.activities.map(a =>
          a.kind === 'approval' && a.token === token
            ? { ...a, decision: approved ? 'approved' : 'denied' }
            : a,
        ),
      })),
    );
    void lessComputerApprove(token, approved);
  };

  const onClose = () => void lessComputerWindowDismiss();

  // ── Esc 关闭 ────────────────────────────────────────────────────────
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        void lessComputerWindowDismiss();
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, []);

  // ── 内容自适应：measure 内容高 + toolbar → 回传后端 clamp + bottom-anchored 摆放，
  // 让内容增长向上撑开。超出 max 则窗口内部滚动。
  const contentRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!isTauri) return;
    const el = contentRef.current;
    if (!el) return;
    const measured = Math.ceil(el.scrollHeight) + TOOLBAR_HEIGHT;
    const target = Math.min(WINDOW_MAX_HEIGHT, Math.max(WINDOW_MIN_HEIGHT, measured));
    void lessComputerWindowResize(target);
  }, [turns]);

  // 自动滚动到底
  const scrollRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [turns]);

  return (
    <div className="ol-frost lc-shell" style={shellStyle}>
      <div className="lc-bg" aria-hidden />
      <Toolbar label={t('lessComputer.closeTooltip')} onClose={onClose} />
      <div ref={scrollRef} style={scrollStyle}>
        <div ref={contentRef} style={contentStyle}>
          {turns.map((turn, ti) => (
            <TurnView key={ti} turn={turn} onApproval={onApproval} t={t} />
          ))}
        </div>
      </div>
    </div>
  );
}

// ── 子组件 ────────────────────────────────────────────────────────────

/** 渲染单轮对话：用户气泡 → 工具/审批 → 助手流式回复 → 收尾(错误/花费)。 */
function TurnView({
  turn,
  onApproval,
  t,
}: {
  turn: Turn;
  onApproval: (token: string, approved: boolean) => void;
  t: ReturnType<typeof useTranslation>['t'];
}) {
  return (
    <>
      <UserBubble text={turn.user} label={t('lessComputer.you')} />
      {turn.activities.map((a, i) =>
        a.kind === 'tool' ? (
          <ToolChipRow key={`t${i}`} name={a.name} t={t} />
        ) : (
          <ApprovalRow key={a.token} card={a} onDecide={onApproval} t={t} />
        ),
      )}
      {turn.answer && <AssistantBubble markdown={turn.answer} working={turn.status === 'working'} />}
      {turn.status === 'working' && !turn.answer && <WorkingRow label={t('lessComputer.working')} />}
      {turn.status === 'error' && <ErrorRow message={turn.errorMsg || t('lessComputer.error')} />}
      {turn.status === 'cancelled' && <CostRow label={t('common.cancelled')} />}
      {turn.status === 'done' && turn.costUsd != null && (
        <CostRow label={t('lessComputer.cost', { cost: turn.costUsd.toFixed(3) })} />
      )}
    </>
  );
}

function Toolbar({ label, onClose }: { label: string; onClose: () => void }) {
  return (
    // 顶栏作为拖动把手：按住空白处可把整个聊天框拖到屏幕任意位置（resize 会保住拖后的位置）。
    <div data-tauri-drag-region style={{ ...toolbarStyle, cursor: 'grab' }}>
      <div data-tauri-drag-region style={{ flex: 1, alignSelf: 'stretch' }} />
      <button
        onClick={onClose}
        onMouseDown={(event) => {
          event.preventDefault();
          event.stopPropagation();
        }}
        title={label}
        aria-label={label}
        style={closeBtnStyle}
      >
        <svg width="11" height="11" viewBox="0 0 11 11">
          <path
            d="M1.5 1.5l8 8M9.5 1.5l-8 8"
            stroke="currentColor"
            strokeWidth="1.6"
            strokeLinecap="round"
          />
        </svg>
      </button>
    </div>
  );
}

function UserBubble({ text, label }: { text: string; label: string }) {
  return (
    <div className="lc-enter" style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-end', gap: 3 }}>
      <span style={roleLabelStyle}>{label}</span>
      <div style={userBubbleStyle}>{text}</div>
    </div>
  );
}

function AssistantBubble({ markdown, working }: { markdown: string; working: boolean }) {
  const html = useMemo(() => {
    let rendered: string;
    try {
      rendered = renderQaMarkdown(markdown);
    } catch (error) {
      console.error('[LessComputer] markdown render failed', error);
      rendered = renderQaPlainText(String(markdown ?? ''));
    }
    // 兜底再消毒：qaMarkdown 已转义 raw HTML token，这里 DOMPurify 多一道防线，
    // 即便上游渲染逻辑回归也不会把恶意标记注入 DOM。
    return DOMPurify.sanitize(rendered, { ADD_ATTR: ['target', 'rel'] });
  }, [markdown]);
  return (
    <div className="lc-enter" style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-start', gap: 3 }}>
      <div
        className="lc-answer"
        style={assistantBubbleStyle}
        // eslint-disable-next-line react/no-danger
        dangerouslySetInnerHTML={{ __html: html }}
      />
      {working && <span style={caretStyle} />}
    </div>
  );
}

function ToolChipRow({
  name,
  t,
}: {
  name: string;
  t: ReturnType<typeof useTranslation>['t'];
}) {
  return (
    <div className="lc-enter" style={{ display: 'flex' }}>
      <span style={toolChipStyle}>
        <span aria-hidden style={{ marginRight: 4 }}>
          {'\u{1F6E0}'}
        </span>
        {t('lessComputer.tool', { name })}
      </span>
    </div>
  );
}

function WorkingRow({ label }: { label: string }) {
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
      <span style={dotStyle} />
      <span style={{ fontSize: 12, color: 'var(--ol-ink-3)', fontWeight: 500 }}>{label}</span>
    </div>
  );
}

function ApprovalRow({
  card,
  onDecide,
  t,
}: {
  card: ApprovalCard;
  onDecide: (token: string, approved: boolean) => void;
  t: ReturnType<typeof useTranslation>['t'];
}) {
  const decided = card.decision != null;
  return (
    <div className="lc-enter" style={approvalCardStyle}>
      <div style={{ fontSize: 12.5, fontWeight: 600, color: 'var(--ol-ink)' }}>
        {t('lessComputer.approvalTitle')}
      </div>
      <code style={approvalCmdStyle}>{card.command}</code>
      <div style={{ fontSize: 11.5, color: 'var(--ol-ink-3)' }}>{card.reason}</div>
      {!decided && (
        <div style={approvalRerunWarningStyle}>
          {t('lessComputer.approvalRerunWarning')}
        </div>
      )}
      {decided ? (
        <div style={{ fontSize: 11.5, fontWeight: 600, color: 'var(--ol-ink-3)' }}>
          {card.decision === 'approved'
            ? t('lessComputer.approved')
            : t('lessComputer.denied')}
        </div>
      ) : (
        <div style={{ display: 'flex', gap: 8 }}>
          <button
            style={denyBtnStyle}
            onMouseDown={(event) => event.stopPropagation()}
            onClick={() => onDecide(card.token, false)}
          >
            {t('lessComputer.deny')}
          </button>
          <button
            style={approveBtnStyle}
            onMouseDown={(event) => event.stopPropagation()}
            onClick={() => onDecide(card.token, true)}
          >
            {t('lessComputer.approve')}
          </button>
        </div>
      )}
    </div>
  );
}

function ErrorRow({ message }: { message: string }) {
  return (
    <div style={errorRowStyle}>
      <span style={{ fontSize: 12.5, color: 'var(--ol-err)', lineHeight: 1.5 }}>{message}</span>
    </div>
  );
}

function CostRow({ label }: { label: string }) {
  return (
    <div style={{ display: 'flex' }}>
      <span style={costChipStyle}>{label}</span>
    </div>
  );
}

// ── 样式 ──────────────────────────────────────────────────────────────

const shellStyle: CSSProperties = {
  width: '100%',
  height: '100vh',
  display: 'flex',
  flexDirection: 'column',
  borderRadius: 14,
  overflow: 'hidden',
  border: '0.5px solid rgba(0, 0, 0, 0.12)',
  background: 'rgba(246, 247, 250, 0.88)',
  boxShadow: '0 18px 44px -18px rgba(15,17,22,.28), 0 0 0 0.5px rgba(255,255,255,.7) inset',
  fontFamily: 'var(--ol-font-sans)',
  color: 'var(--ol-ink)',
  isolation: 'isolate',
};

const toolbarStyle: CSSProperties = {
  height: 28,
  display: 'flex',
  alignItems: 'center',
  padding: '0 8px',
  borderBottom: '0.5px solid rgba(0, 0, 0, 0.08)',
  background:
    'linear-gradient(180deg, rgba(255,255,255,0.74), rgba(238,240,245,0.58))',
  boxShadow: '0 1px 0 rgba(255,255,255,.55) inset',
  backdropFilter: 'blur(18px) saturate(150%)',
  WebkitBackdropFilter: 'blur(18px) saturate(150%)',
  flexShrink: 0,
  position: 'relative',
  zIndex: 1,
};

const closeBtnStyle: CSSProperties = {
  width: 22,
  height: 22,
  border: 0,
  borderRadius: 6,
  display: 'inline-flex',
  alignItems: 'center',
  justifyContent: 'center',
  cursor: 'default',
  padding: 0,
  background: 'transparent',
  color: 'var(--ol-ink-3)',
  transition: 'background 0.16s var(--ol-motion-quick)',
};

const scrollStyle: CSSProperties = {
  flex: 1,
  minHeight: 0,
  overflow: 'auto',
  position: 'relative',
  zIndex: 1,
};

const contentStyle: CSSProperties = {
  padding: 14,
  display: 'flex',
  flexDirection: 'column',
  gap: 10,
};

const roleLabelStyle: CSSProperties = {
  fontSize: 10.5,
  color: 'var(--ol-ink-4)',
  fontWeight: 600,
};

const userBubbleStyle: CSSProperties = {
  maxWidth: '85%',
  padding: '8px 12px',
  borderRadius: 14,
  borderBottomRightRadius: 4,
  background: 'var(--ol-blue)',
  color: '#fff',
  fontSize: 13,
  lineHeight: 1.55,
  wordBreak: 'break-word',
};

const assistantBubbleStyle: CSSProperties = {
  maxWidth: '92%',
  padding: '8px 12px',
  borderRadius: 14,
  borderBottomLeftRadius: 4,
  background: 'rgba(0,0,0,0.04)',
  fontSize: 13,
  lineHeight: 1.6,
  color: 'var(--ol-ink)',
  wordBreak: 'break-word',
  alignSelf: 'flex-start',
};

const caretStyle: CSSProperties = {
  display: 'inline-block',
  width: 6,
  height: 12,
  background: 'var(--ol-blue)',
  marginLeft: 12,
  animation: 'lc-pulse 0.9s var(--ol-motion-soft) infinite',
  borderRadius: 1,
};

const toolChipStyle: CSSProperties = {
  display: 'inline-flex',
  alignItems: 'center',
  fontSize: 11.5,
  fontWeight: 500,
  color: 'var(--ol-ink-2)',
  background: 'rgba(0,0,0,0.045)',
  border: '0.5px solid rgba(0,0,0,0.06)',
  borderRadius: 8,
  padding: '4px 8px',
};

const costChipStyle: CSSProperties = {
  fontSize: 11,
  fontWeight: 500,
  color: 'var(--ol-ink-4)',
  fontFamily: 'var(--ol-font-mono)',
};

const dotStyle: CSSProperties = {
  width: 8,
  height: 8,
  borderRadius: '50%',
  background: 'var(--ol-blue)',
  animation: 'lc-pulse 1.2s var(--ol-motion-soft) infinite',
};

const approvalCardStyle: CSSProperties = {
  display: 'flex',
  flexDirection: 'column',
  gap: 6,
  padding: '10px 12px',
  borderRadius: 12,
  background: 'rgba(220,38,38,0.05)',
  border: '0.5px solid rgba(220,38,38,0.20)',
};

const approvalCmdStyle: CSSProperties = {
  fontFamily: 'var(--ol-font-mono)',
  fontSize: 11.5,
  color: 'var(--ol-ink)',
  background: 'rgba(0,0,0,0.05)',
  borderRadius: 6,
  padding: '5px 8px',
  wordBreak: 'break-all',
};

const approvalRerunWarningStyle: CSSProperties = {
  fontSize: 11,
  lineHeight: 1.45,
  color: 'rgb(180,83,9)',
  background: 'rgba(245,158,11,0.10)',
  border: '0.5px solid rgba(245,158,11,0.25)',
  borderRadius: 6,
  padding: '5px 8px',
};

const approveBtnStyle: CSSProperties = {
  flex: 1,
  border: 0,
  borderRadius: 8,
  padding: '6px 10px',
  fontSize: 12,
  fontWeight: 600,
  cursor: 'default',
  background: 'var(--ol-blue)',
  color: '#fff',
};

const denyBtnStyle: CSSProperties = {
  flex: 1,
  borderRadius: 8,
  padding: '6px 10px',
  fontSize: 12,
  fontWeight: 600,
  cursor: 'default',
  background: 'transparent',
  border: '0.5px solid rgba(0,0,0,0.14)',
  color: 'var(--ol-ink-2)',
};

const errorRowStyle: CSSProperties = {
  padding: '8px 12px',
  borderRadius: 10,
  background: 'rgba(220,38,38,0.06)',
  border: '0.5px solid rgba(220,38,38,0.18)',
};

const globalCss = `
@keyframes lc-pulse {
  0%, 100% { opacity: 1; transform: scale(1); }
  50%      { opacity: 0.35; transform: scale(.94); }
}
/* 内容进场：工具芯片 / 气泡 / 审批卡出现时柔和淡入上滑，而不是直接闪出。 */
@keyframes lc-enter {
  from { opacity: 0; transform: translateY(6px); }
  to   { opacity: 1; transform: translateY(0); }
}
.lc-shell { position: relative; }
.lc-bg {
  position: absolute;
  inset: -1px;
  border-radius: inherit;
  background:
    radial-gradient(120% 80% at 18% 0%, rgba(255,255,255,.72), rgba(255,255,255,0) 58%),
    linear-gradient(180deg, rgba(248,249,252,.78), rgba(235,238,244,.72));
  pointer-events: none;
  z-index: 0;
}
.lc-enter { animation: lc-enter 0.30s var(--ol-motion-soft, cubic-bezier(.16,1,.3,1)) both; }
@media (prefers-reduced-motion: reduce) {
  .lc-enter { animation: none; }
}
.lc-answer p        { margin: 0 0 6px; }
.lc-answer p:last-child { margin-bottom: 0; }
.lc-answer ul,
.lc-answer ol       { margin: 0 0 6px; padding-left: 18px; }
.lc-answer li       { margin: 2px 0; }
.lc-answer code     { font-family: var(--ol-font-mono); font-size: 12px;
                      padding: 1px 5px; border-radius: 4px;
                      background: rgba(0,0,0,0.05); }
.lc-answer pre      { margin: 0 0 6px; padding: 8px 10px;
                      border-radius: 8px; background: rgba(0,0,0,0.05);
                      overflow-x: auto; }
.lc-answer pre code { padding: 0; background: transparent; }
.lc-answer a        { color: var(--ol-blue); text-decoration: none; }
`;

if (typeof document !== 'undefined' && !document.getElementById('less-computer-panel-style')) {
  const tag = document.createElement('style');
  tag.id = 'less-computer-panel-style';
  tag.textContent = globalCss;
  document.head.appendChild(tag);
}
