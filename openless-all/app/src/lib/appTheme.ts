export type AppThemeId = 'light' | 'dark';

export const APP_THEME_KEY = 'ol-app-theme';

export function readAppTheme(): AppThemeId {
  try {
    const value = window.localStorage.getItem(APP_THEME_KEY);
    if (value === 'light' || value === 'dark') return value;
  } catch {
    // Ignore storage access failures and fall back to the default theme.
  }
  return 'light';
}

export function applyAppTheme(theme: AppThemeId): void {
  document.documentElement.dataset.olTheme = theme;
}

export function setAppTheme(theme: AppThemeId): void {
  applyAppTheme(theme);
  try {
    window.localStorage.setItem(APP_THEME_KEY, theme);
  } catch {
    // Ignore storage write failures.
  }
}
