// SelectLite — 受控的下拉组件，替代 native <select> 以避开 Windows Win32 ComboBox
// 弹框的直角丑框（issue #418）。
//
// 设计：
// - 触发器是一个 button（chevron + 当前值标签），样式可被 `style` 覆盖
// - popover 用 portal 渲染到 document.body，避开父容器 overflow:hidden
// - 键盘：ArrowDown/ArrowUp 切换高亮，Enter 确认，Esc 关闭
// - 点击外部 / 滚动 / resize 都会关闭或重定位

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
  const [highlight, setHighlight] = useState<number>(-1);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);
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
    // 默认在触发器下方；若下方空间放不下 popover（按 maxHeight 280 估），翻转向上以避免被视口裁剪。
    const popoverMaxHeight = popoverRef.current?.getBoundingClientRect().height ?? 280;
    const spaceBelow = window.innerHeight - rect.bottom;
    const flipUp = spaceBelow < popoverMaxHeight + 8 && rect.top > popoverMaxHeight + 8;
    const top = flipUp ? rect.top - popoverMaxHeight - 4 : rect.bottom + 4;
    setAnchor({ left: rect.left, top, width: rect.width });
  }, []);

  useLayoutEffect(() => {
    if (!open) return;
    positionPopover();
    const handleReflow = () => positionPopover();
    window.addEventListener('resize', handleReflow);
    window.addEventListener('scroll', handleReflow, true);
    return () => {
      window.removeEventListener('resize', handleReflow);
      window.removeEventListener('scroll', handleReflow, true);
    };
  }, [open, positionPopover]);

  useEffect(() => {
    if (!open) return;
    const handlePointerDown = (event: MouseEvent) => {
      const target = event.target as Node | null;
      if (!target) return;
      if (triggerRef.current?.contains(target)) return;
      if (popoverRef.current?.contains(target)) return;
      setOpen(false);
    };
    document.addEventListener('mousedown', handlePointerDown);
    return () => document.removeEventListener('mousedown', handlePointerDown);
  }, [open]);

  const openMenu = () => {
    if (disabled) return;
    const initial = options.findIndex(opt => opt.value === value && !opt.disabled);
    setHighlight(initial >= 0 ? initial : options.findIndex(opt => !opt.disabled));
    setOpen(true);
  };

  const closeMenu = () => {
    setOpen(false);
    setHighlight(-1);
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
          ref={popoverRef}
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
            animation: 'ol-select-pop .14s var(--ol-motion-quick) both',
          }}
        >
          {options.map((option, index) => {
            const isSelected = option.value === value;
            const isHighlighted = index === highlight;
            return (
              <div
                key={option.value || `__opt_${index}`}
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
