// LessComputerPanel.tsx — Less Computer 语音 Agent 浮窗（窗口 label = "less-computer"）。
//
// 把「按住专用键说话 → Agent 操控电脑」的交互渲染成 agent 输出流（Codex 风格）：
//   - 用户气泡：语音指令转写（`user` 事件，开启新会话并清空旧内容）。
//   - 助手输出：文本与工具调用**按到达顺序交错**成一条文档流 —— 文本段落之间
//     穿插工具行（纯文字，无图标无边框），进行中文字扫光，结束后停光继续往下。
//   - 内联审批卡（`approval`）：高风险动作被护栏拦下时弹 Approve / Deny。
//   - 完成（`completed`）落成本；出错（`error`）红色样式。
// 视觉：shadcn Card 消息窗（主题 token 自适应浅/深色）。
//
// 窗口随内容自适应高度（measure content → setSize），不可拖动、置顶。
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
import { useRafThrottle } from '../lib/useRafThrottle';
import { Marker, MarkerContent, MarkerIcon } from '../components/Marker';
import { MessageScroller } from '../components/MessageScroller';
import { ThinkingDots } from '../components/ThinkingDots';
import { SiriGL } from '../components/SiriGL';
import {
  isTauri,
  lessComputerApprove,
  lessComputerSubmitText,
  lessComputerWindowDismiss,
  lessComputerWindowResize,
} from '../lib/ipc';
import type { CapsulePayload, LessComputerEvent } from '../lib/types';
import { renderQaMarkdown, renderQaPlainText } from '../lib/qaMarkdown';

type RunStatus = 'idle' | 'working' | 'done' | 'error' | 'cancelled';

interface TextSegment {
  kind: 'text';
  content: string;
}

interface ToolSegment {
  kind: 'tool';
  name: string;
  /** 后端没有工具结束事件：下一个事件（delta/tool/approval/收尾）到达即视为结束。 */
  running: boolean;
}

interface ApprovalSegment {
  kind: 'approval';
  token: string;
  command: string;
  reason: string;
  /** 用户已点过的结果，决定按钮禁用态。undefined = 待处理。 */
  decision?: 'approved' | 'denied';
}

interface CompactionSegment {
  kind: 'compaction';
}

/** 助手输出流：文本 / 工具行 / 上下文压缩 / 审批卡按到达顺序排列（Codex 式交错）。 */
type Segment = TextSegment | ToolSegment | CompactionSegment | ApprovalSegment;

/** 一轮对话：用户一句 + 助手输出流 + 本轮收尾态。连续对话累积成数组。 */
interface Turn {
  user: string;
  segments: Segment[];
  status: RunStatus;
  errorMsg: string;
  costUsd: number | null;
}

/** 对 turns 数组「最后一轮」做不可变更新。 */
function updateLastTurn(turns: Turn[], fn: (t: Turn) => Turn): Turn[] {
  if (turns.length === 0) return turns;
  return [...turns.slice(0, -1), fn(turns[turns.length - 1])];
}

/** 把流里还在扫光的工具行停下来（下一个事件到达 = 上一个工具已结束）。 */
function settleRunningTools(segments: Segment[]): Segment[] {
  if (!segments.some(s => s.kind === 'tool' && s.running)) return segments;
  return segments.map(s => (s.kind === 'tool' && s.running ? { ...s, running: false } : s));
}

const WINDOW_MIN_HEIGHT = 140;
const WINDOW_MAX_HEIGHT = 520;
/** ChatHeader 的固定高度（标题 + 状态行两行），窗口自适应测量时加上它。 */
const TOOLBAR_HEIGHT = 56;
/** 底部输入区（Composer）的固定高度，窗口自适应测量时加上它。 */
const COMPOSER_HEIGHT = 58;

// 浏览器预览（vite dev，非 Tauri）：?window=less-computer&demo=1 注入一轮演示对话，
// 覆盖「文本 → 扫光工具行 → 文本 → 已停光工具行 → 审批卡」的交错流，方便调样式。
function getPreviewTurns(): Turn[] {
  if (isTauri || typeof window === 'undefined') return [];
  if (new URLSearchParams(window.location.search).get('demo') !== '1') return [];
  return [
    {
      user: '帮我把桌面上的截图整理到「本周素材」文件夹',
      segments: [
        { kind: 'text', content: '好的，我先看一下桌面上有哪些截图。' },
        { kind: 'tool', name: 'Bash', running: false },
        { kind: 'text', content: '找到 6 张截图，正在移动并按日期重命名…' },
        { kind: 'tool', name: 'Bash', running: true },
        {
          kind: 'approval',
          token: 'demo',
          command: 'mv ~/Desktop/Screenshot*.png ~/Documents/本周素材/',
          reason: 'Moving files outside the working directory.',
        },
      ],
      status: 'working',
      errorMsg: '',
      costUsd: null,
    },
  ];
}

export function LessComputerPanel() {
  const { t } = useTranslation();
  // 连续对话：每按一次说话键追加一轮（除非后端标记 fresh=新会话则清空重开）。
  const [turns, setTurns] = useState<Turn[]>(getPreviewTurns);
  // 新会话计数：fresh 时 +1，作为 shell 的 key 重放入场动画 —— 浮窗是常驻
  // webview（hide/show 复用），没有这个的话再次唤起时内容直接闪现，很突兀。
  const [sessionSeq, setSessionSeq] = useState(0);

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
          segments: [],
          status: 'working',
          errorMsg: '',
          costUsd: null,
        };
        setTurns(prev => (ev.fresh ? [fresh] : [...prev, fresh]));
        if (ev.fresh) setSessionSeq(seq => seq + 1);
        break;
      }
      case 'started':
        setTurns(prev => updateLastTurn(prev, tn => ({ ...tn, status: 'working' })));
        break;
      case 'delta':
        setTurns(prev =>
          updateLastTurn(prev, tn => {
            const segments = settleRunningTools(tn.segments);
            const last = segments[segments.length - 1];
            if (last?.kind === 'text') {
              return {
                ...tn,
                segments: [
                  ...segments.slice(0, -1),
                  { ...last, content: last.content + ev.text },
                ],
              };
            }
            return { ...tn, segments: [...segments, { kind: 'text', content: ev.text }] };
          }),
        );
        break;
      case 'tool':
        setTurns(prev =>
          updateLastTurn(prev, tn => ({
            ...tn,
            segments: [
              ...settleRunningTools(tn.segments),
              { kind: 'tool', name: ev.name, running: true },
            ],
          })),
        );
        break;
      case 'compaction':
        setTurns(prev =>
          updateLastTurn(prev, tn => ({
            ...tn,
            segments: [...settleRunningTools(tn.segments), { kind: 'compaction' }],
          })),
        );
        break;
      case 'approval':
        setTurns(prev =>
          updateLastTurn(prev, tn => ({
            ...tn,
            segments: [
              ...settleRunningTools(tn.segments),
              { kind: 'approval', token: ev.token, command: ev.command, reason: ev.reason },
            ],
          })),
        );
        break;
      case 'completed':
        setTurns(prev =>
          updateLastTurn(prev, tn => {
            let segments = settleRunningTools(tn.segments);
            // 正常情况最终文本已通过 delta 流出；只有整轮没有任何文本时才用
            // completed 的成品兜底（否则会把穿插的工具行冲掉）。
            if (ev.text && !segments.some(s => s.kind === 'text')) {
              segments = [...segments, { kind: 'text', content: ev.text }];
            }
            return { ...tn, segments, costUsd: ev.costUsd ?? null, status: 'done' };
          }),
        );
        break;
      case 'error':
        setTurns(prev =>
          updateLastTurn(prev, tn => ({
            ...tn,
            segments: settleRunningTools(tn.segments),
            errorMsg: ev.message,
            status: 'error',
          })),
        );
        break;
      case 'cancelled':
        setTurns(prev =>
          updateLastTurn(prev, tn => ({
            ...tn,
            segments: settleRunningTools(tn.segments),
            status: 'cancelled',
          })),
        );
        break;
    }
  };

  const onApproval = (token: string, approved: boolean) => {
    setTurns(prev =>
      prev.map(tn => ({
        ...tn,
        segments: tn.segments.map(s =>
          s.kind === 'approval' && s.token === token
            ? { ...s, decision: approved ? 'approved' : ('denied' as const) }
            : s,
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

  // ── 内容自适应：MessageScroller 的 ResizeObserver 报告内容高（含 markdown 渲染
  // 引起的变化）→ 回传后端 clamp + bottom-anchored 摆放。超出 max 则内部滚动。
  const onContentResize = (height: number) => {
    if (!isTauri) return;
    const target = Math.min(
      WINDOW_MAX_HEIGHT,
      Math.max(WINDOW_MIN_HEIGHT, height + TOOLBAR_HEIGHT + COMPOSER_HEIGHT),
    );
    void lessComputerWindowResize(target);
  };

  const working = turns.some(tn => tn.status === 'working');

  return (
    // key=sessionSeq：新会话唤起时重放 lc-shell-in 入场（浮窗窗口复用不重挂载）。
    <div key={sessionSeq} className="lc-shell lc-shell-in" style={shellStyle}>
      {/* 消息弹窗结构（用户指定的聊天卡片规范）：header 常驻标题 + 工作状态
          （「正在操控电脑…」扫光）在上方显示，右上角 ✕。 */}
      <ChatHeader
        title={t('lessComputer.title')}
        status={working ? t('lessComputer.working') : undefined}
        closeLabel={t('lessComputer.closeTooltip')}
        onClose={onClose}
      />
      {/* 滚动行为遵循聊天滚动标准（MessageScroller）：新轮次锚定视口顶部、
          读者离底不跟随、屏外增长浮现「跳到最新」。 */}
      <MessageScroller
        anchorKey={`${sessionSeq}-${turns.length}`}
        defaultScrollPosition="last-anchor"
        busy={working}
        onContentResize={onContentResize}
        jumpLabel={t('lessComputer.jumpToLatest')}
        style={scrollStyle}
        contentStyle={contentStyle}
      >
        {turns.map((turn, ti) => (
          <TurnView key={ti} turn={turn} onApproval={onApproval} t={t} />
        ))}
      </MessageScroller>
      <Composer working={working} t={t} />
    </div>
  );
}

/**
 * 底部输入区（shadcn chat demo 的 CardFooter 复刻）：打字输入 + 发送按钮。
 * 语音状态内嵌（用户拍板）：按住 Less Computer 键说话时这里变成胶囊同款
 * 彩虹光条（SiriGL wave，真实电平驱动），转写/思考时变成六点圆环 —— 通过监听
 * capsule:state 的 operating 会话获得状态与电平，与屏幕下方胶囊同一数据源。
 */
function Composer({
  working,
  t,
}: {
  working: boolean;
  t: ReturnType<typeof useTranslation>['t'];
}) {
  const [text, setText] = useState('');
  const [voice, setVoice] = useState<{ state: 'recording' | 'thinking'; level: number } | null>(
    null,
  );

  useEffect(() => {
    if (!isTauri) return;
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    (async () => {
      try {
        const { listen } = await import('@tauri-apps/api/event');
        const handle = await listen<CapsulePayload>('capsule:state', event => {
          const p = event.payload;
          if (p.operating !== true) return;
          if (p.state === 'recording') setVoice({ state: 'recording', level: p.level ?? 0 });
          else if (p.state === 'transcribing' || p.state === 'polishing')
            setVoice({ state: 'thinking', level: 0 });
          else setVoice(null);
        });
        if (cancelled) handle();
        else unlisten = handle;
      } catch (error) {
        console.error('[LessComputer] capsule listener setup failed', error);
      }
    })();
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const send = () => {
    const trimmed = text.trim();
    if (!trimmed || working) return;
    setText('');
    void lessComputerSubmitText(trimmed);
  };

  return (
    <div style={composerStyle}>
      {voice ? (
        <div style={{ flex: 1, height: 42, position: 'relative' }}>
          {voice.state === 'recording' ? (
            <SiriGL
              mode="wave"
              level={voice.level}
              resolved
              style={{ position: 'absolute', inset: 0, width: '100%', height: '100%' }}
            />
          ) : (
            <div style={{ display: 'flex', alignItems: 'center', gap: 8, height: '100%' }}>
              <ThinkingDots size={18} />
              <span style={{ fontSize: 12, color: 'var(--ol-ink-3)', fontWeight: 500 }}>
                {t('lessComputer.working')}
              </span>
            </div>
          )}
        </div>
      ) : (
        <>
          <textarea
            value={text}
            onChange={event => setText(event.target.value)}
            onKeyDown={event => {
              if (event.key === 'Enter' && !event.shiftKey) {
                event.preventDefault();
                send();
              }
            }}
            placeholder={t('lessComputer.inputPlaceholder')}
            rows={1}
            style={composerInputStyle}
          />
          <button
            onClick={send}
            disabled={!text.trim() || working}
            aria-label={t('lessComputer.send')}
            title={t('lessComputer.send')}
            style={{
              ...sendButtonStyle,
              opacity: !text.trim() || working ? 0.45 : 1,
            }}
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
        </>
      )}
    </div>
  );
}

// ── 子组件 ────────────────────────────────────────────────────────────

/** 渲染单轮对话：用户气泡 → 输出流（文本/工具行/审批卡交错）→ 收尾(错误/花费)。 */
function TurnView({
  turn,
  onApproval,
  t,
}: {
  turn: Turn;
  onApproval: (token: string, approved: boolean) => void;
  t: ReturnType<typeof useTranslation>['t'];
}) {
  const lastSegment = turn.segments[turn.segments.length - 1];
  // 等待指示只在流里没有「自带活动感」的内容时出现：整轮还没输出，或审批已决、
  // 后端正在带着授权重跑。running 工具行的扫光本身就是活动指示。
  const waiting =
    turn.status === 'working' &&
    (turn.segments.length === 0 ||
      (lastSegment?.kind === 'approval' && lastSegment.decision != null));
  return (
    <>
      <UserBubble text={turn.user} label={t('lessComputer.you')} />
      {turn.segments.map((segment, i) => {
        if (segment.kind === 'text') {
          const streaming =
            turn.status === 'working' && i === turn.segments.length - 1;
          return <AssistantText key={`s${i}`} markdown={segment.content} streaming={streaming} />;
        }
        if (segment.kind === 'tool') {
          return (
            <ToolLine
              key={`s${i}`}
              label={t('lessComputer.tool', { name: segment.name })}
              running={segment.running}
            />
          );
        }
        if (segment.kind === 'compaction') {
          return <CompactionLine key={`s${i}`} label={t('lessComputer.compaction')} />;
        }
        return <ApprovalRow key={segment.token} card={segment} onDecide={onApproval} t={t} />;
      })}
      {waiting && <WorkingRow label={t('lessComputer.working')} />}
      {turn.status === 'error' && <ErrorRow message={turn.errorMsg || t('lessComputer.error')} />}
      {turn.status === 'cancelled' && <CostRow label={t('common.cancelled')} />}
      {turn.status === 'done' && turn.costUsd != null && (
        <CostRow label={t('lessComputer.cost', { cost: turn.costUsd.toFixed(3) })} />
      )}
    </>
  );
}

/**
 * 聊天卡片头（消息弹窗规范）：标题 + 工作状态行（「正在操控电脑…」扫光）常驻上方，
 * 右上角 ✕。整条作为拖动把手：按住空白处可把聊天框拖到屏幕任意位置。
 */
function ChatHeader({
  title,
  status,
  closeLabel,
  onClose,
}: {
  title: string;
  status?: string;
  closeLabel: string;
  onClose: () => void;
}) {
  return (
    <div data-tauri-drag-region style={{ ...headerStyle, cursor: 'grab' }}>
      <div data-tauri-drag-region style={{ flex: 1, minWidth: 0 }}>
        <div data-tauri-drag-region style={headerTitleStyle}>{title}</div>
        <div
          data-tauri-drag-region
          role={status ? 'status' : undefined}
          className={status ? 'lc-tool-shimmer' : undefined}
          style={headerStatusStyle}
        >
          {status ?? ' '}
        </div>
      </div>
      <button
        onClick={onClose}
        onMouseDown={(event) => {
          event.preventDefault();
          event.stopPropagation();
        }}
        title={closeLabel}
        aria-label={closeLabel}
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
    // data-scroll-anchor：用户消息是新轮次的锚点行（MessageScroller 把它定位到视口顶部附近）。
    <div
      className="lc-enter"
      data-scroll-anchor
      style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-end', gap: 3 }}
    >
      <span style={roleLabelStyle}>{label}</span>
      <div style={userBubbleStyle}>{text}</div>
    </div>
  );
}

/** 助手文本段：无气泡的文档流（agent 输出风格），流式时末尾带光标。 */
function AssistantText({ markdown, streaming }: { markdown: string; streaming: boolean }) {
  // 按帧率节流：流式回复逐 token 全量 parse + DOMPurify 是 O(n²)，长回复越来越卡。
  const throttled = useRafThrottle(markdown);
  const html = useMemo(() => {
    let rendered: string;
    try {
      rendered = renderQaMarkdown(throttled);
    } catch (error) {
      console.error('[LessComputer] markdown render failed', error);
      rendered = renderQaPlainText(String(throttled ?? ''));
    }
    // 兜底再消毒：qaMarkdown 已转义 raw HTML token，这里 DOMPurify 多一道防线，
    // 即便上游渲染逻辑回归也不会把恶意标记注入 DOM。
    return DOMPurify.sanitize(rendered, { ADD_ATTR: ['target', 'rel'] });
  }, [throttled]);
  return (
    <div className="lc-enter" style={{ display: 'flex', flexDirection: 'column', alignItems: 'stretch', gap: 3 }}>
      <div
        className="lc-answer"
        style={assistantTextStyle}
        // eslint-disable-next-line react/no-danger
        dangerouslySetInnerHTML={{ __html: html }}
      />
      {streaming && <span style={caretStyle} />}
    </div>
  );
}

/**
 * 工具行（Codex 式，Marker 语义）：纯文字穿插在输出流里，无图标无边框。
 * 进行中 role="status"（辅助技术播报）+ 文字扫光（background-clip:text，
 * 元素只有一行小字，重绘面积可忽略），结束后停光变普通淡字。
 */
function ToolLine({ label, running }: { label: string; running: boolean }) {
  return (
    <Marker className="lc-enter" role={running ? 'status' : undefined}>
      <MarkerContent className={running ? 'lc-tool lc-tool-shimmer' : 'lc-tool'}>
        {label}
      </MarkerContent>
    </Marker>
  );
}

/** 上下文压缩行：separator 变体，横贯输出流标出压缩边界（穿插在它实际发生的位置）。 */
function CompactionLine({ label }: { label: string }) {
  return (
    <Marker className="lc-enter" variant="separator">
      <MarkerContent style={{ fontSize: 11, color: 'var(--ol-ink-4)', fontWeight: 500 }}>
        {label}
      </MarkerContent>
    </Marker>
  );
}

function WorkingRow({ label }: { label: string }) {
  return (
    <Marker role="status" style={{ gap: 8 }}>
      <MarkerIcon>
        <ThinkingDots size={18} />
      </MarkerIcon>
      <MarkerContent style={{ fontSize: 12, color: 'var(--ol-ink-3)', fontWeight: 500 }}>
        {label}
      </MarkerContent>
    </Marker>
  );
}

function ApprovalRow({
  card,
  onDecide,
  t,
}: {
  card: ApprovalSegment;
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
    <Marker>
      <MarkerContent style={costChipStyle}>{label}</MarkerContent>
    </Marker>
  );
}

// ── 样式（shadcn Card 消息窗语言，主题 token 自适应浅/深色 —— 用户拍板）──
// 实底 + 细边框，不用 backdrop-filter（webview 模糊不了透明窗口背后的桌面，
// issue #470 同款结论）。

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

const headerStyle: CSSProperties = {
  height: 56,
  display: 'flex',
  alignItems: 'center',
  gap: 8,
  padding: '0 10px 0 14px',
  borderBottom: '0.5px solid var(--ol-line)',
  flexShrink: 0,
  position: 'relative',
  zIndex: 1,
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

/** 状态行：working 时显示「正在操控电脑…」（扫光由 lc-tool-shimmer 提供），
    空闲时占位保持 header 高度稳定。 */
const headerStatusStyle: CSSProperties = {
  fontSize: 11.5,
  fontWeight: 500,
  lineHeight: '16px',
  color: 'var(--ol-ink-3)',
  whiteSpace: 'nowrap',
  overflow: 'hidden',
  textOverflow: 'ellipsis',
  minHeight: 16,
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

// overflow 由 MessageScroller 的内部 viewport 接管，外层只负责占满剩余高度。
const scrollStyle: CSSProperties = {
  flex: 1,
  minHeight: 0,
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
  background: 'var(--ol-accent-solid-bg)',
  boxShadow: '0 2px 10px -6px rgba(37, 99, 235, 0.5)',
  color: 'var(--ol-accent-solid-ink)',
  fontSize: 13,
  lineHeight: 1.55,
  wordBreak: 'break-word',
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
  animation: 'lc-pulse 0.9s var(--ol-motion-soft) infinite',
  borderRadius: 1,
};

const costChipStyle: CSSProperties = {
  fontSize: 11,
  fontWeight: 500,
  color: 'var(--ol-ink-4)',
  fontFamily: 'var(--ol-font-mono)',
};

const approvalCardStyle: CSSProperties = {
  display: 'flex',
  flexDirection: 'column',
  gap: 6,
  padding: '10px 12px',
  borderRadius: 12,
  background: 'rgba(239, 68, 68, 0.08)',
  border: '0.5px solid rgba(239, 68, 68, 0.28)',
};

const approvalCmdStyle: CSSProperties = {
  fontFamily: 'var(--ol-font-mono)',
  fontSize: 11.5,
  color: 'var(--ol-ink)',
  background: 'var(--ol-surface-2)',
  borderRadius: 6,
  padding: '5px 8px',
  wordBreak: 'break-all',
};

const approvalRerunWarningStyle: CSSProperties = {
  fontSize: 11,
  lineHeight: 1.45,
  color: 'var(--ol-warn)',
  background: 'rgba(245, 158, 11, 0.12)',
  border: '0.5px solid rgba(245, 158, 11, 0.30)',
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
  background: 'var(--ol-accent-solid-bg)',
  color: 'var(--ol-accent-solid-ink)',
};

const denyBtnStyle: CSSProperties = {
  flex: 1,
  borderRadius: 8,
  padding: '6px 10px',
  fontSize: 12,
  fontWeight: 600,
  cursor: 'default',
  background: 'transparent',
  border: '0.5px solid var(--ol-line-strong)',
  color: 'var(--ol-ink-2)',
};

const errorRowStyle: CSSProperties = {
  padding: '8px 12px',
  borderRadius: 10,
  background: 'rgba(239, 68, 68, 0.08)',
  border: '0.5px solid rgba(239, 68, 68, 0.24)',
};

const composerStyle: CSSProperties = {
  flexShrink: 0,
  minHeight: COMPOSER_HEIGHT,
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
  fontSize: 13,
  lineHeight: 1.5,
  fontFamily: 'var(--ol-font-sans)',
  color: 'var(--ol-ink)',
  background: 'var(--ol-surface-2)',
  outline: 'none',
  maxHeight: 92,
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
@keyframes lc-pulse {
  0%, 100% { opacity: 1; transform: scale(1); }
  50%      { opacity: 0.35; transform: scale(.94); }
}
/* 内容进场：工具行 / 气泡 / 审批卡出现时柔和淡入上滑，而不是直接闪出。 */
@keyframes lc-enter {
  from { opacity: 0; transform: translateY(6px); }
  to   { opacity: 1; transform: translateY(0); }
}
/* 浮窗整体入场：新会话唤起时上滑浮现（窗口复用，靠 key 重放）。 */
@keyframes lc-shell-in {
  from { opacity: 0; transform: translateY(10px) scale(.985); }
  to   { opacity: 1; transform: translateY(0) scale(1); }
}
.lc-shell { position: relative; }
.lc-shell-in { animation: lc-shell-in 0.34s var(--ol-motion-spring, cubic-bezier(0.16,1,0.3,1)) both; }
@media (prefers-reduced-motion: reduce) {
  .lc-shell-in { animation: none; }
}
.lc-enter { animation: lc-enter 0.30s var(--ol-motion-soft, cubic-bezier(.16,1,.3,1)) both; }
@media (prefers-reduced-motion: reduce) {
  .lc-enter { animation: none; }
}
/* 工具行：纯文字（无图标无框）。进行中扫光，结束停光。 */
.lc-tool {
  font-size: 12px;
  font-weight: 500;
  line-height: 1.5;
  color: var(--ol-ink-4);
}
.lc-tool-shimmer {
  background-image: linear-gradient(100deg,
    var(--ol-ink-4) 0%, var(--ol-ink-4) 35%,
    var(--ol-blue) 50%,
    var(--ol-ink-4) 65%, var(--ol-ink-4) 100%);
  background-size: 220% auto;
  -webkit-background-clip: text;
  background-clip: text;
  -webkit-text-fill-color: transparent;
  animation: lc-shimmer 1.6s linear infinite;
}
@keyframes lc-shimmer {
  0%   { background-position: 200% center; }
  100% { background-position: -200% center; }
}
@media (prefers-reduced-motion: reduce) {
  .lc-tool-shimmer { animation: none; -webkit-text-fill-color: currentColor; }
}
.lc-answer p        { margin: 0 0 6px; }
.lc-answer p:last-child { margin-bottom: 0; }
.lc-answer ul,
.lc-answer ol       { margin: 0 0 6px; padding-left: 18px; }
.lc-answer li       { margin: 2px 0; }
.lc-answer code     { font-family: var(--ol-font-mono); font-size: 12px;
                      padding: 1px 5px; border-radius: 4px;
                      background: var(--ol-surface-2); }
.lc-answer pre      { margin: 0 0 6px; padding: 8px 10px;
                      border-radius: 8px; background: var(--ol-surface-2);
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
