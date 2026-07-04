// MessageScroller.tsx — 聊天记录滚动器（shadcn MessageScroller 行为规范的纯 React 实现）。
//
// 用户拍板的聊天滚动标准（详见 ui.shadcn.com/docs/components/radix/message-scroller）：
//   1. 永不违背读者意愿移动视口：只有读者已经在底部（pinned）时才跟随流式内容；
//      向上滚动 = 主动退出跟随，位置保持不动；选择文本也是退出信号。
//   2. 新轮次锚定：`anchorKey` 变化时，把内容里最后一个 [data-scroll-anchor] 行
//      定位到视口顶部附近（上方留 anchorPeek 的上一轮 peek 保上下文），回复在下方
//      流式展开 —— 而不是把视口甩到底部。
//   3. 屏外增长：未跟随时新内容在屏外累积、视口纹丝不动，浮现「跳到最新」按钮；
//      点击回到底部并恢复跟随。内容高度变化用 ResizeObserver 检测（markdown 渲染 /
//      图片加载引起的布局变化同样被覆盖，读者不丢位置）。
//   4. 无障碍：内容容器 role="log" + aria-relevant="additions"。
//
// 它只负责滚动行为，不持有消息/状态/传输 —— 行内容由调用方渲染，需要锚定的行
// 自带 data-scroll-anchor 属性。

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from 'react';

interface MessageScrollerProps {
  /** 新轮次标识：变化时执行「锚定到视口顶部」。undefined 时不锚定。 */
  anchorKey?: string | number;
  /** 锚定时上一轮内容在锚点上方保留的可见高度（px）。 */
  anchorPeek?: number;
  /**
   * 首次挂载的打开位置（shadcn 规范）：'last-anchor' 定位到最后一个锚点行
   * （通常是用户最后的提问，回复在其下方）——比落到绝对底部更有上下文；
   * 无锚点或内容不溢出时退回 'end'。默认 'end'。
   */
  defaultScrollPosition?: 'end' | 'last-anchor';
  /** 透传 aria-busy（流式进行中）。 */
  busy?: boolean;
  /** 内容高度变化回调（LC 浮窗用它驱动窗口自适应高度）。 */
  onContentResize?: (height: number) => void;
  /** 「跳到最新」按钮的无障碍标签。 */
  jumpLabel: string;
  style?: CSSProperties;
  contentStyle?: CSSProperties;
  children: ReactNode;
}

/** 距底部小于该值即视为「在底部」（惯性滚动/亚像素误差的容差）。 */
const PIN_THRESHOLD = 32;

export function MessageScroller({
  anchorKey,
  anchorPeek = 18,
  defaultScrollPosition = 'end',
  busy,
  onContentResize,
  jumpLabel,
  style,
  contentStyle,
  children,
}: MessageScrollerProps) {
  const viewportRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  // 跟随态。true = 读者在底部，内容增长时视口跟随；false = 读者已主动离开，不动。
  const pinnedRef = useRef(true);
  // 程序滚动进行中：期间的 scroll 事件不是读者意图，不更新 pinned。
  const programmaticRef = useRef(false);
  const [showJump, setShowJump] = useState(false);

  const scrollToBottom = useCallback(() => {
    const viewport = viewportRef.current;
    if (!viewport) return;
    programmaticRef.current = true;
    viewport.scrollTop = viewport.scrollHeight;
    requestAnimationFrame(() => {
      programmaticRef.current = false;
    });
  }, []);

  const syncJumpVisibility = useCallback(() => {
    const viewport = viewportRef.current;
    if (!viewport) return;
    const overflow = viewport.scrollHeight - viewport.clientHeight > 4;
    setShowJump(overflow && !pinnedRef.current);
  }, []);

  const onScroll = useCallback(() => {
    const viewport = viewportRef.current;
    if (!viewport || programmaticRef.current) return;
    const atBottom =
      viewport.scrollTop + viewport.clientHeight >= viewport.scrollHeight - PIN_THRESHOLD;
    pinnedRef.current = atBottom;
    syncJumpVisibility();
  }, [syncJumpVisibility]);

  // 选择文本 = 读者正在阅读，无论位置立刻退出跟随（内容跳动会毁掉选区）。
  const onSelectStart = useCallback(() => {
    pinnedRef.current = false;
    syncJumpVisibility();
  }, [syncJumpVisibility]);

  // 内容高度变化（流式增长 / markdown 渲染 / 图片加载）：跟随中滚到底，
  // 否则保持读者位置、只更新「跳到最新」的显隐。
  useEffect(() => {
    const content = contentRef.current;
    if (!content) return undefined;
    const observer = new ResizeObserver(() => {
      onContentResize?.(Math.ceil(content.scrollHeight));
      if (pinnedRef.current) scrollToBottom();
      syncJumpVisibility();
    });
    observer.observe(content);
    return () => observer.disconnect();
  }, [onContentResize, scrollToBottom, syncJumpVisibility]);

  // 新轮次锚定：最后一个 [data-scroll-anchor]（通常是刚发出的用户消息）定位到
  // 视口顶部附近。锚定后退出跟随 —— 回复在锚点下方展开，超出视口则屏外增长。
  useEffect(() => {
    if (anchorKey == null) return;
    const viewport = viewportRef.current;
    const content = contentRef.current;
    if (!viewport || !content) return;
    const anchors = content.querySelectorAll<HTMLElement>('[data-scroll-anchor]');
    const anchor = anchors[anchors.length - 1];
    if (!anchor) return;
    programmaticRef.current = true;
    viewport.scrollTop = Math.max(0, anchor.offsetTop - anchorPeek);
    requestAnimationFrame(() => {
      programmaticRef.current = false;
    });
    pinnedRef.current = false;
    syncJumpVisibility();
    // anchorPeek/syncJumpVisibility 稳定；仅在新轮次时执行一次锚定。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [anchorKey]);

  const jumpToLatest = useCallback(() => {
    pinnedRef.current = true;
    scrollToBottom();
    setShowJump(false);
  }, [scrollToBottom]);

  // 打开位置（仅首次挂载执行一次）：'last-anchor' 把最后一个锚点行定位到视口
  // 顶部附近 —— 重开保存的会话时读者落在「自己最后的提问」而不是绝对底部。
  const openedRef = useRef(false);
  useEffect(() => {
    if (openedRef.current) return;
    openedRef.current = true;
    if (defaultScrollPosition !== 'last-anchor') return;
    const viewport = viewportRef.current;
    const content = contentRef.current;
    if (!viewport || !content) return;
    const anchors = content.querySelectorAll<HTMLElement>('[data-scroll-anchor]');
    const anchor = anchors[anchors.length - 1];
    // 无锚点或内容不溢出 → 维持 'end' 行为（ResizeObserver 首轮已滚底）。
    if (!anchor || viewport.scrollHeight - viewport.clientHeight <= 4) return;
    programmaticRef.current = true;
    viewport.scrollTop = Math.max(0, anchor.offsetTop - anchorPeek);
    requestAnimationFrame(() => {
      programmaticRef.current = false;
    });
    pinnedRef.current = false;
    syncJumpVisibility();
    // 仅首挂执行；依赖刻意留空。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div style={{ position: 'relative', ...style }}>
      <div
        ref={viewportRef}
        onScroll={onScroll}
        onSelectCapture={onSelectStart}
        className="ol-scroll-fade"
        style={{ position: 'absolute', inset: 0, overflow: 'auto' }}
      >
        <div
          ref={contentRef}
          role="log"
          aria-relevant="additions"
          aria-busy={busy || undefined}
          style={contentStyle}
        >
          {children}
        </div>
      </div>
      {showJump && (
        <button onClick={jumpToLatest} aria-label={jumpLabel} style={jumpButtonStyle}>
          <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden>
            <path
              d="M6 1.5v9M2.2 7.2 6 10.7l3.8-3.5"
              stroke="currentColor"
              strokeWidth="1.6"
              fill="none"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </svg>
        </button>
      )}
    </div>
  );
}

const jumpButtonStyle: CSSProperties = {
  position: 'absolute',
  left: '50%',
  bottom: 10,
  transform: 'translateX(-50%)',
  width: 28,
  height: 28,
  borderRadius: 999,
  border: '0.5px solid var(--ol-line-strong)',
  background: 'var(--ol-surface)',
  color: 'var(--ol-ink-2)',
  display: 'inline-flex',
  alignItems: 'center',
  justifyContent: 'center',
  cursor: 'default',
  padding: 0,
  boxShadow: '0 6px 18px -8px rgba(0, 0, 0, 0.35)',
};
