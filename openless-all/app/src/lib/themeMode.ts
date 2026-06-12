// 主题模式 — localStorage 持久化，通过 html[data-ol-theme] 切换暗色 token 集。

export type ThemeMode = 'system' | 'light' | 'dark';

const THEME_MODE_KEY = 'ol-theme-mode';

function systemPrefersDark(): boolean {
  return window.matchMedia('(prefers-color-scheme: dark)').matches;
}

export function readThemeMode(): ThemeMode {
  try {
    const v = window.localStorage.getItem(THEME_MODE_KEY);
    if (v === 'system' || v === 'light' || v === 'dark') return v;
  } catch { /* localStorage 不可用：忽略，落回默认 */ }
  return 'system';
}

export function applyThemeMode(mode: ThemeMode): void {
  const root = document.documentElement;
  if (mode === 'dark' || (mode === 'system' && systemPrefersDark())) {
    root.dataset.olTheme = 'dark';
  } else {
    delete root.dataset.olTheme;
  }
}

export function setThemeMode(mode: ThemeMode): ThemeMode {
  try { window.localStorage.setItem(THEME_MODE_KEY, mode); } catch { /* 忽略 */ }
  applyThemeMode(mode);
  return mode;
}

export function subscribeThemeMode(): () => void {
  const mq = window.matchMedia('(prefers-color-scheme: dark)');
  const onChange = () => {
    if (readThemeMode() === 'system') applyThemeMode('system');
  };
  mq.addEventListener('change', onChange);
  return () => mq.removeEventListener('change', onChange);
}
