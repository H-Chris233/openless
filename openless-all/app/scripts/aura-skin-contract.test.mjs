import { readFile } from 'node:fs/promises';
import assert from 'node:assert/strict';

const root = new URL('../', import.meta.url);

async function read(relPath) {
  return readFile(new URL(relPath, root), 'utf8');
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function assertUsesClassName(source, className, message) {
  const escapedClassName = escapeRegExp(className);
  const patterns = [
    new RegExp(`className\\s*=\\s*(?:\\{\\s*)?"(?:[^"]*\\s)?${escapedClassName}(?:\\s[^"]*)?"(?:\\s*\\})?`),
    new RegExp(`className\\s*=\\s*(?:\\{\\s*)?'(?:[^']*\\s)?${escapedClassName}(?:\\s[^']*)?'(?:\\s*\\})?`),
    new RegExp(`className\\s*=\\s*(?:\\{\\s*)?\`(?:[^\`]*\\s)?${escapedClassName}(?:\\s[^\`]*)?\`(?:\\s*\\})?`),
  ];

  assert.ok(patterns.some((pattern) => pattern.test(source)), message);
}

assert.throws(
  () => assertUsesClassName('<div>ol-app-shell-bg</div>', 'ol-app-shell-bg', 'sample must require className usage'),
  /sample must require className usage/,
);
assert.throws(
  () =>
    assertUsesClassName(
      '<div className="foo-ol-app-shell-bg-bar" />',
      'ol-app-shell-bg',
      'sample must require an exact class token',
    ),
  /sample must require an exact class token/,
);
assertUsesClassName(
  '<div className="foo ol-app-shell-bg bar" />',
  'ol-app-shell-bg',
  'sample should accept className usage',
);

const [tokens, globalCss, shell, settingsModal, overview] = await Promise.all([
  read('src/styles/tokens.css'),
  read('src/styles/global.css'),
  read('src/components/FloatingShell.tsx'),
  read('src/components/SettingsModal.tsx'),
  read('src/pages/Overview.tsx'),
]);

assert.match(tokens, /--ol-shell-radius:/, 'tokens.css must define --ol-shell-radius');
assert.match(tokens, /--ol-panel-radius:/, 'tokens.css must define --ol-panel-radius');
assert.match(tokens, /--ol-aura-shadow:/, 'tokens.css must define --ol-aura-shadow');
assert.match(tokens, /--ol-font-display:/, 'tokens.css must define --ol-font-display');

assert.match(globalCss, /\.ol-app-shell-bg\b/, 'global.css must expose .ol-app-shell-bg');
assert.match(globalCss, /\.ol-aura-panel\b/, 'global.css must expose .ol-aura-panel');
assert.doesNotMatch(globalCss, /@keyframes ol-aura-halo/, 'global.css must not add an animated halo');

assertUsesClassName(shell, 'ol-app-shell-bg', 'FloatingShell must use the app shell background class');
assertUsesClassName(shell, 'ol-aura-sidebar', 'FloatingShell must expose an Aura sidebar hook');
assertUsesClassName(shell, 'ol-aura-panel', 'FloatingShell must expose an Aura panel hook');

assertUsesClassName(settingsModal, 'ol-aura-settings', 'SettingsModal must expose an Aura settings wrapper');
assertUsesClassName(overview, 'ol-overview-hero', 'Overview must expose a high-visibility overview surface hook');

console.log('Aura skin contract OK');
