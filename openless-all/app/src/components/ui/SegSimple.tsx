// SegSimple — segmented control used in the Settings modal sub-sections.

export type SegOption = { value: string; label: string };

interface SegSimpleProps {
  options: SegOption[];
  value: string;
  onChange?: (value: string) => void;
}

export function SegSimple({ options, value, onChange }: SegSimpleProps) {
  return (
    <div
      role="radiogroup"
      style={{ display: 'inline-flex', padding: 2, borderRadius: 'var(--ol-control-radius)', background: 'var(--ol-segmented-bg)' }}
    >
      {options.map((o) => {
        const selected = value === o.value;
        return (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={selected}
          onClick={() => onChange?.(o.value)}
          style={{
            padding: '5px 12px', fontSize: 12, fontWeight: 500, border: 0, borderRadius: 'var(--ol-r-sm)',
            fontFamily: 'inherit',
            background: selected ? 'var(--ol-segmented-active-bg)' : 'transparent',
            color: selected ? 'var(--ol-ink)' : 'var(--ol-ink-3)',
            boxShadow: selected ? 'var(--ol-segmented-active-shadow)' : 'none',
            cursor: 'default',
          }}
        >
          {o.label}
        </button>
        );
      })}
    </div>
  );
}
