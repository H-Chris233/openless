// SelectLite — 受控的下拉组件，替代 native <select> 以避开 Windows Win32 ComboBox
// 弹框的直角丑框（issue #418）。
//
// 设计：
// - 触发器是一个 button（chevron + 当前值标签），样式可被 `style` 覆盖
// - popover 用 portal 渲染到 document.body，避开父容器 overflow:hidden
// - 键盘：ArrowDown/ArrowUp 切换高亮，Enter 确认，Esc 关闭
// - 点击外部 / 滚动外部容器都会关闭（popover 内部 scroll 不关闭）
// - 关闭有 .14s exit 动画；mount 时 callback ref + RAF 二次定位防 first-paint 错位

import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent as ReactKeyboardEvent,
} from 'react';
import { createPortal } from 'react-dom';
import { Icon } from '../Icon';

export interface SelectOption {
  value: string;
  label: string;
  disabled?: boolean;
}

interface SelectLiteProps {
  value: string;
  onChange: (value: string) => void;
  options: SelectOption[];
  placeholder?: string;
  disabled?: boolean;
  style?: CSSProperties;
  ariaLabel?: string;
}

const DEFAULT_TRIGGER_STYLE: CSSProperties = {
  display: 'inline-flex',
  alignItems: 'center',
  justifyContent: 'space-between',
  gap: 8,
  padding: '0 10px',
  height: 32,
  fontSize: 12.5,
  fontFamily: 'inherit',
  borderRadius: 8,
  border: '0.5px solid var(--ol-line-strong)',
  background: 'var(--ol-surface-2)',
  color: 'var(--ol-ink)',
  cursor: 'default',
  outline: 'none',
  textAlign: 'left',
  minWidth: 160,
};

const EXIT_ANIM_MS = 140;

export function SelectLite({
  value,
  onChange,
  options,
  placeholder,
  disabled = false,
  style,
  ariaLabel,
}: SelectLiteProps) {
  const [open, setOpen] = useState(false);
  // leaving 让 popover 在卸载前播完 exit keyframe（用户报"没有收缩动画"——之前直接 unmount）
  const [leaving, setLeaving] = useState(false);
  const [highlight, setHighlight] = useState<number>(-1);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const [anchor, setAnchor] = useState<{ left: number; top: number; width: number } | null>(null);

  const selected = useMemo(
    () => options.find(opt => opt.value === value),
    [options, value],
  );
  const displayLabel = selected?.label ?? placeholder ?? '';

  const positionPopover = useCallback(() => {
    const trigger = triggerRef.current;
    if (!trigger) return;
    const rect = trigger.getBoundingClientRect();
    const popoverRect = popoverRef.current?.getBoundingClientRect();
    const popoverHeight = popoverRect?.height ?? 280;
    // popover 宽度优先用真实测量值（>= trigger 宽），fallback 才用 trigger 宽。
    const popoverWidth = Math.max(popoverRect?.width ?? 0, rect.width);
    // 纵向：默认在触发器下方；若下方空间放不下 popover，翻转向上避免被视口裁剪。
    const spaceBelow = window.innerHeight - rect.bottom;
    const flipUp = spaceBelow < popoverHeight + 8 && rect.top > popoverHeight + 8;
    const top = flipUp ? rect.top - popoverHeight - 4 : rect.bottom + 4;
    // 横向：窗口右边的 select 可能让 popover 溢出屏幕；clamp 到 [8, viewport-width-8]。
    const minLeft = 8;
    const maxLeft = Math.max(minLeft, window.innerWidth - popoverWidth - 8);
    const left = Math.min(Math.max(rect.left, minLeft), maxLeft);
    setAnchor({ left, top, width: rect.width });
  }, []);

  // popover ref callback：每次 popover DOM mount/unmount 调一次。
  // 关键：mount 时拿到真实 popover 宽（content 撑大），requestAnimationFrame
  // 推到下一帧 paint 前再重算 anchor —— 修复"first paint 用 trigger 宽 fallback 后
  // popover 位置漂掉"的 bug。
  const setPopoverRef = useCallback(
    (node: HTMLDivElement | null) => {
      popoverRef.current = node;
      if (node) {
        requestAnimationFrame(() => positionPopover());
      }
    },
    [positionPopover],
  );

  // v1.3.1-8 hotfix: open=true 时立即设 anchor（用 trigger 宽 fallback），不再依赖
  // popover mount 触发 callback ref。之前的死锁：anchor 初始 null → portal 条件
  // `open && anchor` 不通过 → popover DOM 永不挂载 → callback ref 永不 fire →
  // anchor 永远 null。结果所有 dropdown 点击后什么都不发生。
  // 现在 open=true 立即 setAnchor，popover 渲染挂载后 callback ref 再 RAF 重定位
  // 拿真实 popover 宽。
  useLayoutEffect(() => {
    if (!open) return;
    positionPopover();
  }, [open, positionPopover]);

  // 键盘 ArrowUp/Down 改 highlight 后把高亮项 scroll into view —— 长 dropdown 超过
  // maxHeight 280 时键盘用户能看到当前高亮。
  useEffect(() => {
    if (!open || highlight < 0) return;
    const target = popoverRef.current?.querySelector(
      `[data-option-index="${highlight}"]`,
    ) as HTMLElement | null;
    target?.scrollIntoView({ block: 'nearest' });
  }, [highlight, open]);

  // 点击外部 / 滚动外部 → 关闭。popover 内部 scroll 保持打开。
  useEffect(() => {
    if (!open) return;
    const handlePointerDown = (event: MouseEvent) => {
      const target = event.target as Node | null;
      if (!target) return;
      if (triggerRef.current?.contains(target)) return;
      if (popoverRef.current?.contains(target)) return;
      closeMenu();
    };
    // 用户在 popover 外部任何位置滚动（wheel 或 scroll 事件）→ 关闭。
    // popover 内部滚动（长列表 scroll）popover.contains(target) → 保留打开。
    const handleScrollOutside = (event: Event) => {
      const target = event.target as Node | null;
      if (target && popoverRef.current?.contains(target)) return;
      closeMenu();
    };
    // window resize 强制关闭：重算位置成本高且大多数 resize 表明 user 不再想看 popover。
    const handleResize = () => closeMenu();

    document.addEventListener('mousedown', handlePointerDown);
    window.addEventListener('scroll', handleScrollOutside, { capture: true, passive: true });
    window.addEventListener('wheel', handleScrollOutside, { capture: true, passive: true });
    window.addEventListener('resize', handleResize);
    return () => {
      document.removeEventListener('mousedown', handlePointerDown);
      window.removeEventListener('scroll', handleScrollOutside, true);
      window.removeEventListener('wheel', handleScrollOutside, true);
      window.removeEventListener('resize', handleResize);
    };
    // closeMenu 是稳定引用（无 React state 依赖），不放 deps。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const openMenu = () => {
    if (disabled) return;
    const initial = options.findIndex(opt => opt.value === value && !opt.disabled);
    setHighlight(initial >= 0 ? initial : options.findIndex(opt => !opt.disabled));
    setLeaving(false);
    setOpen(true);
  };

  const closeMenu = () => {
    if (!open) return;
    setLeaving(true);
    window.setTimeout(() => {
      setOpen(false);
      setLeaving(false);
      setHighlight(-1);
      setAnchor(null);
    }, EXIT_ANIM_MS);
  };

  const selectIndex = (index: number) => {
    const option = options[index];
    if (!option || option.disabled) return;
    onChange(option.value);
    closeMenu();
    triggerRef.current?.focus();
  };

  const moveHighlight = (direction: 1 | -1) => {
    if (options.length === 0) return;
    let next = highlight;
    for (let i = 0; i < options.length; i += 1) {
      next = (next + direction + options.length) % options.length;
      if (!options[next]?.disabled) {
        setHighlight(next);
        return;
      }
    }
  };

  const handleKeyDown = (event: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (disabled) return;
    if (!open) {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp' || event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        openMenu();
      }
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      closeMenu();
    } else if (event.key === 'ArrowDown') {
      event.preventDefault();
      moveHighlight(1);
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      moveHighlight(-1);
    } else if (event.key === 'Enter') {
      event.preventDefault();
      if (highlight >= 0) selectIndex(highlight);
    } else if (event.key === 'Tab') {
      closeMenu();
    }
  };

  const triggerStyle: CSSProperties = {
    ...DEFAULT_TRIGGER_STYLE,
    ...style,
    opacity: disabled ? 0.5 : 1,
    cursor: disabled ? 'not-allowed' : 'default',
  };

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        className="ol-focus-ring"
        role="combobox"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-disabled={disabled}
        aria-label={ariaLabel}
        disabled={disabled}
        onClick={() => (open ? closeMenu() : openMenu())}
        onKeyDown={handleKeyDown}
        style={triggerStyle}
      >
        <span
          style={{
            flex: 1,
            minWidth: 0,
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
            color: selected ? 'var(--ol-ink)' : 'var(--ol-ink-4)',
          }}
        >
          {displayLabel}
        </span>
        <Icon name="chevDown" size={11} />
      </button>
      {open && anchor && createPortal(
        <div
          ref={setPopoverRef}
          role="listbox"
          style={{
            position: 'fixed',
            left: anchor.left,
            top: anchor.top,
            minWidth: anchor.width,
            maxHeight: 280,
            overflowY: 'auto',
            padding: 4,
            borderRadius: 10,
            border: '0.5px solid rgba(0, 0, 0, 0.10)',
            background: 'rgba(252, 252, 254, 0.94)',
            backdropFilter: 'blur(20px) saturate(180%)',
            WebkitBackdropFilter: 'blur(20px) saturate(180%)',
            boxShadow: '0 12px 30px -10px rgba(15, 17, 22, 0.25), 0 0 0 0.5px rgba(0, 0, 0, 0.06)',
            zIndex: 9999,
            fontFamily: 'inherit',
            fontSize: 12.5,
            animation: leaving
              ? 'ol-select-pop-out .14s cubic-bezier(.4,.0,.7,.2) forwards'
              : 'ol-select-pop .14s var(--ol-motion-quick) both',
            transformOrigin: 'top center',
          }}
        >
          {options.map((option, index) => {
            const isSelected = option.value === value;
            const isHighlighted = index === highlight;
            return (
              <div
                key={option.value || `__opt_${index}`}
                data-option-index={index}
                role="option"
                aria-selected={isSelected}
                aria-disabled={option.disabled}
                onMouseEnter={() => !option.disabled && setHighlight(index)}
                onMouseDown={event => {
                  event.preventDefault();
                  selectIndex(index);
                }}
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  gap: 8,
                  padding: '7px 10px',
                  borderRadius: 6,
                  cursor: option.disabled ? 'not-allowed' : 'default',
                  opacity: option.disabled ? 0.45 : 1,
                  background: isHighlighted && !option.disabled
                    ? 'rgba(37, 99, 235, 0.10)'
                    : 'transparent',
                  color: isSelected ? 'var(--ol-blue)' : 'var(--ol-ink)',
                  fontWeight: isSelected ? 600 : 500,
                  whiteSpace: 'nowrap',
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  transition: 'background 0.10s var(--ol-motion-quick)',
                }}
              >
                <span style={{ flex: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis' }}>
                  {option.label}
                </span>
                {isSelected && <Icon name="check" size={12} />}
              </div>
            );
          })}
        </div>,
        document.body,
      )}
    </>
  );
}
