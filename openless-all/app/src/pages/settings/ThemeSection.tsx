import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { SegSimple } from '../../components/ui/SegSimple';
import { readThemeMode, setThemeMode, type ThemeMode } from '../../lib/themeMode';
import { Card } from '../_atoms';
import { SettingRow } from './shared';

const MODES: ThemeMode[] = ['system', 'light', 'dark'];

export function ThemeSection() {
  const { t } = useTranslation();
  const [mode, setMode] = useState<ThemeMode>(readThemeMode());

  const options = useMemo(() => MODES.map((value) => ({
    value,
    label: t(`settings.appearance.${value === 'system' ? 'followSystem' : value}`),
  })), [t]);

  const apply = (next: ThemeMode) => {
    setMode(next);
    setThemeMode(next);
  };

  return (
    <Card>
      <div style={{ fontSize: 13, fontWeight: 600, marginBottom: 6 }}>{t('settings.appearance.title')}</div>
      <SettingRow label={t('settings.appearance.label')}>
        <SegSimple
          options={options}
          value={mode}
          onChange={next => apply(next as ThemeMode)}
        />
      </SettingRow>
    </Card>
  );
}
