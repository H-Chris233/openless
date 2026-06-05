// SwitchLite — small toggle used in the Settings modal sub-sections.

import { useState } from 'react';

interface SwitchLiteProps {
  on?: boolean;
}

export function SwitchLite({ on: initial = false }: SwitchLiteProps) {
  const [on, setOn] = useState(initial);
  return (
    <button
      type="button"
      className="ol-focus-ring"
      onClick={() => setOn(!on)}
      style={{
        position: 'relative', width: 32, height: 18, borderRadius: 999, border: 0,
        background: on ? 'var(--ol-blue)' : 'var(--ol-toggle-off-bg)',
        cursor: 'default',
        outline: 'none',
      }}
    >
      <span
        style={{
          position: 'absolute', top: 2, left: on ? 16 : 2,
          width: 14, height: 14, borderRadius: 999, background: 'var(--ol-toggle-knob)',
          boxShadow: '0 1px 2px rgba(0,0,0,.25)', transition: 'left .16s var(--ol-motion-spring)',
        }}
      />
    </button>
  );
}
