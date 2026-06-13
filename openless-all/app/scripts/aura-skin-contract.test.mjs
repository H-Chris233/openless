import { readFile, readdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import assert from 'node:assert/strict';

// AURA 界面大改已回退到 beta-3 外观（commit 5671805），随之删除了 themeMode.ts /
// ThemeSection、solid-button token（--ol-*-solid-*）与 .ol-aura-* / .ol-app-shell-bg
// 等 AURA 专属皮肤类。本契约相应精简：只保留回退后仍存在、且仍在守护的护栏 ——
// WCAG 对比度、Style 页主题 token，以及「禁止硬编码白底 / 坏配色组合 / 引号包裹的
// CSS 变量」的全量源码扫描（commit 760f662 修复暗色白底就依赖这条）。

const root = new URL('../', import.meta.url);

async function read(relPath) {
  return readFile(new URL(relPath, root), 'utf8');
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function extractCssBlock(css, selector) {
  const escapedSelector = escapeRegExp(selector);
  const match = css.match(new RegExp(`${escapedSelector}\\s*\\{([\\s\\S]*?)\\n\\}`));
  assert.ok(match, `${selector} block must exist`);
  return match[1];
}

function parseCustomProperties(block) {
  const tokens = new Map();
  const re = /^\s*(--[\w-]+)\s*:\s*([^;]+);/gm;
  let match;
  while ((match = re.exec(block)) !== null) {
    tokens.set(match[1], match[2].trim());
  }
  return tokens;
}

function resolveTokenValue(name, tokens, stack = new Set()) {
  const raw = tokens.get(name);
  assert.ok(raw, `missing token ${name}`);
  if (!raw.startsWith('var(')) {
    return raw;
  }
  const inner = raw.slice(4, -1).trim();
  const refName = inner.split(',')[0].trim();
  assert.ok(refName.startsWith('--'), `unsupported var() reference in ${name}: ${raw}`);
  if (stack.has(refName)) {
    throw new Error(`circular var() reference: ${[...stack, refName].join(' -> ')}`);
  }
  stack.add(refName);
  return resolveTokenValue(refName, tokens, stack);
}

function parseCssColor(value) {
  const hexMatch = value.match(/^#([0-9a-f]{3}|[0-9a-f]{6})$/i);
  if (hexMatch) {
    let hex = hexMatch[1];
    if (hex.length === 3) {
      hex = hex
        .split('')
        .map((ch) => ch + ch)
        .join('');
    }
    return {
      r: Number.parseInt(hex.slice(0, 2), 16),
      g: Number.parseInt(hex.slice(2, 4), 16),
      b: Number.parseInt(hex.slice(4, 6), 16),
      a: 1,
    };
  }

  const rgbaMatch = value.match(/^rgba?\(\s*([\d.]+)\s*,\s*([\d.]+)\s*,\s*([\d.]+)(?:\s*,\s*([\d.]+))?\s*\)$/i);
  if (rgbaMatch) {
    return {
      r: Number(rgbaMatch[1]),
      g: Number(rgbaMatch[2]),
      b: Number(rgbaMatch[3]),
      a: rgbaMatch[4] === undefined ? 1 : Number(rgbaMatch[4]),
    };
  }

  throw new Error(`unsupported color format: ${value}`);
}

function srgbChannel(value) {
  const normalized = value / 255;
  return normalized <= 0.03928 ? normalized / 12.92 : ((normalized + 0.055) / 1.055) ** 2.4;
}

function relativeLuminance({ r, g, b }) {
  const R = srgbChannel(r);
  const G = srgbChannel(g);
  const B = srgbChannel(b);
  return 0.2126 * R + 0.7152 * G + 0.0722 * B;
}

function contrastRatio(foreground, background) {
  const fg = parseCssColor(foreground);
  const bg = parseCssColor(background);
  assert.equal(fg.a, 1, `foreground must be opaque for contrast checks: ${foreground}`);
  assert.equal(bg.a, 1, `background must be opaque for contrast checks: ${background}`);
  const lighter = Math.max(relativeLuminance(fg), relativeLuminance(bg));
  const darker = Math.min(relativeLuminance(fg), relativeLuminance(bg));
  return (lighter + 0.05) / (darker + 0.05);
}

function compositeOverBackground(fgValue, bgValue) {
  const fg = parseCssColor(fgValue);
  const bg = parseCssColor(bgValue);
  if (fg.a === 1) {
    return fgValue;
  }
  const alpha = fg.a;
  const r = Math.round(fg.r * alpha + bg.r * (1 - alpha));
  const g = Math.round(fg.g * alpha + bg.g * (1 - alpha));
  const b = Math.round(fg.b * alpha + bg.b * (1 - alpha));
  return `rgb(${r}, ${g}, ${b})`;
}

function contrastRatioOverBackground(foreground, background) {
  const effectiveFg = compositeOverBackground(foreground, background);
  return contrastRatio(effectiveFg, background);
}

function assertMutedContrast(tokens, label, bgToken, inkToken, minRatio = 4.5) {
  const bg = resolveTokenValue(bgToken, tokens);
  const ink = resolveTokenValue(inkToken, tokens);
  const ratio = contrastRatioOverBackground(ink, bg);
  assert.ok(
    ratio >= minRatio,
    `${label}: ${inkToken} on ${bgToken} must meet WCAG AA (${ratio.toFixed(2)}:1 < ${minRatio}:1)`,
  );
  return ratio;
}

const srcRoot = fileURLToPath(new URL('src/', root));

async function walkSourceFiles(dirPath, files = []) {
  const entries = await readdir(dirPath, { withFileTypes: true });
  for (const entry of entries) {
    const entryPath = path.join(dirPath, entry.name);
    if (entry.isDirectory()) {
      await walkSourceFiles(entryPath, files);
      continue;
    }
    if (/\.(tsx?|css)$/.test(entry.name)) {
      files.push(path.relative(srcRoot, entryPath).replace(/\\/g, '/'));
    }
  }
  return files;
}

const [tokens, globalCss, stylePage, sourceFiles, remoteStyle] = await Promise.all([
  read('src/styles/tokens.css'),
  read('src/styles/global.css'),
  read('src/pages/Style.tsx'),
  walkSourceFiles(srcRoot),
  read('src-tauri/src/remote_server/assets/style.css'),
]);

// 回退后仍保留的设计 token。
assert.match(tokens, /--ol-shell-radius:/, 'tokens.css must define --ol-shell-radius');
assert.match(tokens, /--ol-panel-radius:/, 'tokens.css must define --ol-panel-radius');
assert.match(tokens, /--ol-aura-shadow:/, 'tokens.css must define --ol-aura-shadow');
assert.match(tokens, /--ol-font-display:/, 'tokens.css must define --ol-font-display');
assert.match(tokens, /--ol-on-accent:/, 'tokens.css must define --ol-on-accent');
assert.match(tokens, /--ol-control-radius:/, 'tokens.css must define --ol-control-radius');

const lightTokens = parseCustomProperties(extractCssBlock(tokens, ':root'));

// 弱化文字（ink-4）在浅色 surface 上仍要满足 WCAG AA。
const mutedContrastPairs = [
  { label: 'light ink-4 on surface', tokens: lightTokens, bg: '--ol-surface', ink: '--ol-ink-4' },
  { label: 'light ink-4 on surface-2', tokens: lightTokens, bg: '--ol-surface-2', ink: '--ol-ink-4' },
];
const mutedContrastRatios = {};
for (const pair of mutedContrastPairs) {
  mutedContrastRatios[pair.label] = assertMutedContrast(pair.tokens, pair.label, pair.bg, pair.ink);
}

const remoteTokens = parseCustomProperties(extractCssBlock(remoteStyle, ':root'));
const remoteMutedContrastPairs = [
  { label: 'remote ink-4 on surface', tokens: remoteTokens, bg: '--surface', ink: '--ink-4' },
  { label: 'remote ink-4 on surface-2', tokens: remoteTokens, bg: '--surface-2', ink: '--ink-4' },
];
const remoteMutedContrastRatios = {};
for (const pair of remoteMutedContrastPairs) {
  remoteMutedContrastRatios[pair.label] = assertMutedContrast(pair.tokens, pair.label, pair.bg, pair.ink);
}

// 回退后保持静态磨砂：不得引入动画 halo。
// 注：原 “.ol-frost 不得硬编码白底” 是 dark-mode 护栏，但 AURA 回退删掉了主题切换
// （themeMode.ts），当前应用是 light-only、dark token 休眠，beta-3 的白色磨砂是预期外观，
// 故该护栏暂时移除；若日后恢复 dark-mode 切换，应连同 --ol-frost-bg token 一起加回。
assert.doesNotMatch(globalCss, /@keyframes ol-aura-halo/, 'global.css must not add an animated halo');

// Style 页必须走主题 token、不得硬编码浅色卡片底色（回退后暗色白底的根源，commit 760f662）。
const forbiddenStyleCardLightBackgrounds = [
  /rgba\(\s*255\s*,\s*255\s*,\s*255/i,
  /rgba\(\s*248\s*,\s*250\s*,\s*252/i,
  /rgba\(\s*239\s*,\s*246\s*,\s*255/i,
];
for (const pattern of forbiddenStyleCardLightBackgrounds) {
  assert.doesNotMatch(
    stylePage,
    pattern,
    'Style.tsx must not hardcode light style-card backgrounds (use --ol-style-* tokens)',
  );
}
assert.match(stylePage, /--ol-style-card-bg/, 'Style.tsx must reference --ol-style-card-bg for style pack surfaces');
assert.match(stylePage, /--ol-style-card-ink/, 'Style.tsx must reference --ol-style-card-ink for style pack text');
assert.match(stylePage, /--ol-style-subtle-bg/, 'Style.tsx must reference --ol-style-subtle-bg for editor subtle surfaces');

// 全量源码扫描：注入 CSS 字符串里禁止用引号包裹的 var()；禁止把 --ol-ink 当实心底色；
// 禁止 --ol-blue / --ol-err 与 --ol-on-accent / --ol-accent-solid-ink 的低对比组合。
const illegalCssStringPatterns = [
  /color:\s*'var\([^)]+\)';/,
  /background:\s*'var\([^)]+\)';/,
];

const forbiddenInlineInkBackground = /background:\s*'var\(--ol-ink\)'/;

const forbiddenBlueOnAccentCombo =
  /background:[\s\S]{0,200}var\(--ol-blue\)[\s\S]{0,500}?color:[\s\S]{0,200}var\(--ol-on-accent\)|color:[\s\S]{0,200}var\(--ol-on-accent\)[\s\S]{0,500}?background:[\s\S]{0,200}var\(--ol-blue\)/;

const forbiddenErrOnAccentCombo =
  /background:[\s\S]{0,200}var\(--ol-err[^)]*\)[\s\S]{0,500}?color:[\s\S]{0,200}var\(--ol-on-accent\)|background:[\s\S]{0,200}var\(--ol-err[^)]*\)[\s\S]{0,500}?color:[\s\S]{0,200}var\(--ol-accent-solid-ink\)|color:[\s\S]{0,200}var\(--ol-on-accent\)[\s\S]{0,500}?background:[\s\S]{0,200}var\(--ol-err[^)]*\)|color:[\s\S]{0,200}var\(--ol-accent-solid-ink\)[\s\S]{0,500}?background:[\s\S]{0,200}var\(--ol-err[^)]*\)/;

for (const relPath of sourceFiles) {
  const source = await read(`src/${relPath}`);
  for (const pattern of illegalCssStringPatterns) {
    assert.doesNotMatch(
      source,
      pattern,
      `src/${relPath} must not use quoted CSS custom properties inside injected CSS strings`,
    );
  }
  if (relPath.endsWith('.tsx')) {
    assert.doesNotMatch(
      source,
      forbiddenInlineInkBackground,
      `src/${relPath} must not use --ol-ink as a button/solid background`,
    );
    assert.doesNotMatch(
      source,
      forbiddenBlueOnAccentCombo,
      `src/${relPath} must not pair background var(--ol-blue) with color var(--ol-on-accent) (use --ol-accent-solid-* tokens)`,
    );
    assert.doesNotMatch(
      source,
      forbiddenErrOnAccentCombo,
      `src/${relPath} must not pair background var(--ol-err) with color var(--ol-on-accent) or var(--ol-accent-solid-ink) (use --ol-danger-solid-* tokens)`,
    );
  }
}

console.log('Aura skin contract (trimmed post-revert) OK');
console.log(
  'Muted contrast ratios:',
  Object.fromEntries(
    Object.entries({ ...mutedContrastRatios, ...remoteMutedContrastRatios }).map(([label, ratio]) => [
      label,
      `${ratio.toFixed(2)}:1`,
    ]),
  ),
);
