// SegSimple — segmented control used in the Settings modal sub-sections.

import { useState } from 'react';

interface SegSimpleProps {
  options: string[];
  active: string;
}

export function SegSimple({ options, active }: SegSimpleProps) {
  const [v, setV] = useState(active);
  return (
    <div style={{ display: 'inline-flex', padding: 2, borderRadius: 'var(--ol-control-radius)', background: 'var(--ol-segmented-bg)' }}>
      {options.map((o) => (
        <button
          key={o}
          onClick={() => setV(o)}
          style={{
            padding: '5px 12px', fontSize: 12, fontWeight: 500, border: 0, borderRadius: 'var(--ol-r-sm)',
            fontFamily: 'inherit',
            background: v === o ? 'var(--ol-segmented-active-bg)' : 'transparent',
            color: v === o ? 'var(--ol-ink)' : 'var(--ol-ink-3)',
            boxShadow: v === o ? 'var(--ol-segmented-active-shadow)' : 'none',
            cursor: 'default',
          }}
        >
          {o}
        </button>
      ))}
    </div>
  );
}
