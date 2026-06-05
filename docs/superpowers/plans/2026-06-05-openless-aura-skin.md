# OpenLess Aura Skin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apply an Aura-inspired visual skin to OpenLess by upgrading the shell, glass treatment, radii, typography, and overview surfaces while preserving the current information architecture and page density.

**Architecture:** The implementation stays shallow and visual-only. It starts by adding a contract test that defines the new skin hooks, then updates global tokens and chrome, then reskins the main shell and settings modal, and finally aligns shared atoms and the overview page to the new surface system.

**Tech Stack:** React 18, TypeScript, Vite 5, inline-style component patterns, CSS token files, Node-based contract checks

---

## File Structure

- Modify: `openless-all/app/package.json`
  Purpose: register a repeatable Aura skin contract check command.
- Create: `openless-all/app/scripts/aura-skin-contract.test.mjs`
  Purpose: assert the new skin tokens and shell hooks exist before visual work is considered complete.
- Modify: `openless-all/app/src/styles/tokens.css`
  Purpose: replace the current light-glass token ladder with the new Aura-inspired radii, surfaces, shadows, and typography.
- Modify: `openless-all/app/src/styles/global.css`
  Purpose: add the static layered background and shared shell/panel helper classes without introducing animation.
- Modify: `openless-all/app/src/components/FloatingShell.tsx`
  Purpose: reskin the app shell, sidebar, version zone, and main console wrapper.
- Modify: `openless-all/app/src/components/SettingsModal.tsx`
  Purpose: align the settings modal shell and left rail with the new skin system.
- Modify: `openless-all/app/src/pages/_atoms.tsx`
  Purpose: make cards, buttons, pills, and page headers inherit the new radius and surface vocabulary.
- Modify: `openless-all/app/src/pages/Overview.tsx`
  Purpose: make the overview page visibly match the new shell without changing its data structure.

### Task 1: Add Aura Skin Contract Check

**Files:**
- Create: `openless-all/app/scripts/aura-skin-contract.test.mjs`
- Modify: `openless-all/app/package.json`
- Test: `openless-all/app/scripts/aura-skin-contract.test.mjs`

- [ ] **Step 1: Write the failing test**

Create `openless-all/app/scripts/aura-skin-contract.test.mjs`:

```js
import { readFile } from 'node:fs/promises';
import assert from 'node:assert/strict';

const root = new URL('../', import.meta.url);

async function read(relPath) {
  return readFile(new URL(relPath, root), 'utf8');
}

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

assert.match(shell, /ol-app-shell-bg/, 'FloatingShell must use the app shell background class');
assert.match(shell, /ol-aura-sidebar/, 'FloatingShell must expose an Aura sidebar hook');
assert.match(shell, /ol-aura-panel/, 'FloatingShell must expose an Aura panel hook');

assert.match(settingsModal, /ol-aura-settings/, 'SettingsModal must expose an Aura settings wrapper');
assert.match(overview, /ol-overview-hero/, 'Overview must expose a high-visibility overview surface hook');

console.log('Aura skin contract OK');
```

- [ ] **Step 2: Register the check in `package.json`**

Modify `openless-all/app/package.json`:

```json
{
  "scripts": {
    "dev": "vite",
    "build": "tsc && vite build",
    "preview": "vite preview",
    "tauri": "tauri",
    "check:aura-skin": "node scripts/aura-skin-contract.test.mjs",
    "check:macos-capsule-spaces": "node scripts/macos-capsule-spaces-contract.test.mjs",
    "check:hotkey-injection": "node scripts/check-hotkey-injection.mjs"
  }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run:

```bash
npm run check:aura-skin
```

Expected:

```text
AssertionError [ERR_ASSERTION]: tokens.css must define --ol-shell-radius
```

- [ ] **Step 4: Commit the failing test harness**

```bash
git add openless-all/app/package.json openless-all/app/scripts/aura-skin-contract.test.mjs
git commit -m "test: add aura skin contract"
```

### Task 2: Replace Global Tokens and App Chrome

**Files:**
- Modify: `openless-all/app/src/styles/tokens.css`
- Modify: `openless-all/app/src/styles/global.css`
- Test: `openless-all/app/scripts/aura-skin-contract.test.mjs`

- [ ] **Step 1: Update the token ladder**

Replace the current token center in `openless-all/app/src/styles/tokens.css` with the Aura skin ladder:

```css
:root {
  --ol-bg-base: #f3f4f6;
  --ol-bg-elevated: #fbfbfc;
  --ol-surface: rgba(255, 255, 255, 0.72);
  --ol-surface-2: rgba(255, 255, 255, 0.88);
  --ol-surface-solid: #ffffff;

  --ol-line: rgba(15, 23, 42, 0.08);
  --ol-line-strong: rgba(15, 23, 42, 0.14);
  --ol-line-soft: rgba(255, 255, 255, 0.55);

  --ol-ink: #10131a;
  --ol-ink-2: #222938;
  --ol-ink-3: rgba(16, 19, 26, 0.64);
  --ol-ink-4: rgba(16, 19, 26, 0.42);
  --ol-ink-5: rgba(16, 19, 26, 0.24);

  --ol-blue: #2f6df6;
  --ol-blue-hover: #2458c8;
  --ol-blue-soft: rgba(47, 109, 246, 0.10);
  --ol-blue-ring: rgba(47, 109, 246, 0.22);

  --ol-shell-radius: 32px;
  --ol-panel-radius: 28px;
  --ol-card-radius: 22px;
  --ol-pill-radius: 999px;

  --ol-aura-shadow: 0 24px 80px -32px rgba(15, 23, 42, 0.24), 0 12px 36px -18px rgba(15, 23, 42, 0.12);
  --ol-aura-shadow-soft: 0 10px 30px -18px rgba(15, 23, 42, 0.16), 0 0 0 0.5px rgba(255, 255, 255, 0.55) inset;

  --ol-font-display: "Aptos", "Segoe UI Variable Display", "PingFang SC", "Microsoft YaHei", sans-serif;
  --ol-font-sans: "Aptos", "Segoe UI Variable Text", "PingFang SC", "Microsoft YaHei", sans-serif;
  --ol-font-mono: "JetBrains Mono", "SF Mono", "Cascadia Code", Consolas, monospace;
}
```

- [ ] **Step 2: Add static Aura shell helpers**

Append shared helpers to `openless-all/app/src/styles/global.css`:

```css
body {
  background:
    radial-gradient(circle at 18% 12%, rgba(255, 255, 255, 0.92), rgba(255, 255, 255, 0) 34%),
    radial-gradient(circle at 86% 18%, rgba(47, 109, 246, 0.08), rgba(47, 109, 246, 0) 30%),
    linear-gradient(180deg, #f6f7fa 0%, #eef1f6 100%);
  color: var(--ol-ink);
}

.ol-app-shell-bg {
  background:
    radial-gradient(circle at top left, rgba(255, 255, 255, 0.68), rgba(255, 255, 255, 0) 38%),
    linear-gradient(180deg, rgba(255, 255, 255, 0.68), rgba(245, 247, 251, 0.58));
}

.ol-aura-panel {
  background: var(--ol-surface);
  backdrop-filter: blur(24px) saturate(150%);
  -webkit-backdrop-filter: blur(24px) saturate(150%);
  border: 1px solid rgba(255, 255, 255, 0.58);
  box-shadow: var(--ol-aura-shadow);
}

.ol-aura-card {
  background: linear-gradient(180deg, rgba(255, 255, 255, 0.92), rgba(252, 252, 253, 0.82));
  border: 1px solid rgba(255, 255, 255, 0.74);
  box-shadow: var(--ol-aura-shadow-soft);
}
```

- [ ] **Step 3: Run the contract check**

Run:

```bash
npm run check:aura-skin
```

Expected:

```text
AssertionError [ERR_ASSERTION]: FloatingShell must use the app shell background class
```

- [ ] **Step 4: Commit the token/chrome layer**

```bash
git add openless-all/app/src/styles/tokens.css openless-all/app/src/styles/global.css
git commit -m "feat: add aura skin tokens and chrome"
```

### Task 3: Reskin the Shell and Settings Modal

**Files:**
- Modify: `openless-all/app/src/components/FloatingShell.tsx`
- Modify: `openless-all/app/src/components/SettingsModal.tsx`
- Test: `openless-all/app/scripts/aura-skin-contract.test.mjs`

- [ ] **Step 1: Apply Aura shell hooks in `FloatingShell.tsx`**

Update the shell wrappers in `openless-all/app/src/components/FloatingShell.tsx`:

```tsx
<div
  className="ol-app-shell-bg"
  style={{
    flex: 1,
    position: 'relative',
    display: 'flex',
    flexDirection: 'column',
    minHeight: 0,
    paddingTop: os === 'mac' ? 28 : 0,
  }}
>
```

Update the sidebar and main panel wrappers:

```tsx
<aside
  className="ol-aura-sidebar"
  style={{
    width: 196,
    flexShrink: 0,
    display: 'flex',
    flexDirection: 'column',
    padding: '14px 12px 14px',
  }}
>
```

```tsx
<main
  className="ol-console-main ol-aura-panel"
  style={{
    flex: 1,
    minWidth: 0,
    overflow: 'hidden',
    borderRadius: 'var(--ol-panel-radius)',
    display: 'flex',
    flexDirection: 'column',
  }}
>
```

- [ ] **Step 2: Rework the sidebar visual details**

Update the brand zone and footer chip in `openless-all/app/src/components/FloatingShell.tsx`:

```tsx
<div style={{ display: 'flex', alignItems: 'center', gap: 10, padding: '6px 10px 16px' }}>
  <img
    src="AppIcon.png"
    alt="OpenLess"
    style={{
      width: 26,
      height: 26,
      borderRadius: 8,
      boxShadow: '0 10px 24px -16px rgba(47, 109, 246, 0.45)',
    }}
  />
  <div>
    <div style={{ fontSize: 14, fontWeight: 600, fontFamily: 'var(--ol-font-display)' }}>OpenLess</div>
    <div style={{ fontSize: 10.5, color: 'var(--ol-ink-4)', fontFamily: 'var(--ol-font-mono)', letterSpacing: '.08em' }}>
      VOICE CONSOLE
    </div>
  </div>
</div>
```

- [ ] **Step 3: Align the settings modal shell**

Update `openless-all/app/src/components/SettingsModal.tsx`:

```tsx
<div
  className="ol-aura-settings"
  onClick={(e) => e.stopPropagation()}
  style={{
    width: '100%',
    maxWidth: 920,
    height: '100%',
    maxHeight: 620,
    background: 'var(--ol-surface)',
    borderRadius: 'var(--ol-shell-radius)',
    border: '1px solid rgba(255,255,255,0.62)',
    boxShadow: 'var(--ol-aura-shadow)',
    display: 'flex',
    overflow: 'hidden',
    position: 'relative',
  }}
>
```

Reskin the left rail:

```tsx
<aside
  style={{
    width: 214,
    flexShrink: 0,
    background: 'linear-gradient(180deg, rgba(255,255,255,0.58), rgba(246,248,252,0.72))',
    borderRight: '1px solid rgba(255,255,255,0.52)',
    padding: '20px 14px',
    display: 'flex',
    flexDirection: 'column',
    gap: 16,
  }}
>
```

- [ ] **Step 4: Run the contract check**

Run:

```bash
npm run check:aura-skin
```

Expected:

```text
AssertionError [ERR_ASSERTION]: Overview must expose a high-visibility overview surface hook
```

- [ ] **Step 5: Commit the shell layer**

```bash
git add openless-all/app/src/components/FloatingShell.tsx openless-all/app/src/components/SettingsModal.tsx
git commit -m "feat: reskin aura shell and settings modal"
```

### Task 4: Align Shared Atoms and the Overview Page

**Files:**
- Modify: `openless-all/app/src/pages/_atoms.tsx`
- Modify: `openless-all/app/src/pages/Overview.tsx`
- Test: `openless-all/app/scripts/aura-skin-contract.test.mjs`
- Test: `openless-all/app/package.json`

- [ ] **Step 1: Upgrade the shared atoms**

Modify `openless-all/app/src/pages/_atoms.tsx`:

```tsx
export function PageHeader({ kicker, title, desc, right, titleRight }: PageHeaderProps) {
  return (
    <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 24, marginBottom: 28 }}>
      <div style={{ minWidth: 0 }}>
        {kicker && (
          <div style={{ fontSize: 11, fontWeight: 600, letterSpacing: '.14em', textTransform: 'uppercase', color: 'var(--ol-ink-4)', marginBottom: 10, fontFamily: 'var(--ol-font-mono)' }}>
            {kicker}
          </div>
        )}
        <div style={{ display: 'flex', alignItems: 'center', gap: 12, flexWrap: 'wrap' }}>
          <h1 style={{ margin: 0, fontSize: 34, fontWeight: 600, letterSpacing: '-0.04em', color: 'var(--ol-ink)', fontFamily: 'var(--ol-font-display)' }}>
            {title}
          </h1>
          {titleRight}
        </div>
        {desc && <p style={{ margin: '10px 0 0', fontSize: 13.5, color: 'var(--ol-ink-3)', maxWidth: 680, lineHeight: 1.6 }}>{desc}</p>}
      </div>
      {right}
    </div>
  );
}
```

Update `Card` to use the new helper class and radius:

```tsx
export function Card({ children, style, padding = 18, glassy = false, className }: CardProps) {
  return (
    <div
      className={['ol-aura-card', className].filter(Boolean).join(' ')}
      style={{
        background: glassy ? 'var(--ol-surface)' : 'var(--ol-surface-solid)',
        border: '1px solid rgba(255,255,255,0.74)',
        borderRadius: 'var(--ol-card-radius)',
        padding,
        boxShadow: 'var(--ol-aura-shadow-soft)',
        ...style,
      }}
    >
      {children}
    </div>
  );
}
```

- [ ] **Step 2: Make the overview page the first complete showcase**

Modify `openless-all/app/src/pages/Overview.tsx`:

```tsx
<PageHeader
  kicker="AURA OVERVIEW"
  title={t('overview.title')}
  desc={t('overview.metricTotalTrend')}
/>
```

Add the overview showcase hooks and higher-visibility providers row:

```tsx
<div
  className="ol-overview-hero"
  style={{
    display: 'grid',
    gridTemplateColumns: '1fr 1fr',
    gap: 14,
    marginBottom: 20,
  }}
>
```

Elevate the bottom containers:

```tsx
<Card className="ol-overview-hero" padding={20} style={{ display: 'flex', flexDirection: 'column', minHeight: 0 }}>
```

- [ ] **Step 3: Run the Aura contract check**

Run:

```bash
npm run check:aura-skin
```

Expected:

```text
Aura skin contract OK
```

- [ ] **Step 4: Run the production build**

Run:

```bash
npm run build
```

Expected:

```text
vite v5.x building for production...
✓ built in
```

- [ ] **Step 5: Commit the overview alignment**

```bash
git add openless-all/app/src/pages/_atoms.tsx openless-all/app/src/pages/Overview.tsx
git commit -m "feat: apply aura styling to overview surfaces"
```

### Task 5: Final Verification and Manual Review

**Files:**
- Test: `openless-all/app/scripts/aura-skin-contract.test.mjs`
- Test: local dev preview at `http://127.0.0.1:1420`

- [ ] **Step 1: Start the dev server**

Run:

```bash
npm run dev -- --host 127.0.0.1
```

Expected:

```text
Local: http://127.0.0.1:1420/
```

- [ ] **Step 2: Manually verify the shell and overview**

Check these exact visual outcomes:

```text
1. No animated light halo or moving glow exists anywhere in the app shell.
2. The app background shows a static layered light field.
3. Sidebar, main panel, and settings modal all share the same Aura glass language.
4. Overview cards use larger radii and stronger hierarchy than the old skin.
5. The product still reads as light, calm, and easy to scan.
```

- [ ] **Step 3: Re-run automated verification**

Run:

```bash
npm run check:aura-skin && npm run build
```

Expected:

```text
Aura skin contract OK
...build succeeds...
```

- [ ] **Step 4: Final commit**

```bash
git add openless-all/app
git commit -m "feat: ship openless aura skin"
```
