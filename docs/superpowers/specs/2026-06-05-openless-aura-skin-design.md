# OpenLess Aura Skin Design

## Goal

Refresh the OpenLess desktop UI by borrowing the visual language of the reference HTML at `iframe-export-1779865710530(1).html` while preserving OpenLess's current information architecture, page density, and reading comfort.

This is a skin redesign, not a product redesign.

## Design Intent

The target look should feel more premium, sculpted, and technical without becoming dark, noisy, or visually tiring.

We will reuse these reference qualities:

- larger radii and softer geometry
- frosted glass layering
- stronger typography hierarchy
- restrained monochrome palette with blue reserved for accents
- clearer separation between shell, panel, card, and utility surfaces

We will explicitly not reuse these reference qualities:

- animated halo or WebGL background
- dark-theme-first presentation
- login-page composition
- decorative technical labels that compete with product content
- large glow effects that reduce legibility

## Scope

This work changes visual skin only.

Included:

- app background treatment
- main shell and console framing
- sidebar navigation skin
- overview cards and list containers
- high-visibility buttons, chips, badges, and status treatments
- settings modal shell and section styling
- global tokens for radii, surface color, glass, shadows, and typography

Excluded:

- navigation structure
- page IA and feature grouping
- product logic, IPC, or state flow
- major layout rewrites inside functional pages
- dark mode implementation
- animations beyond subtle existing transitions

## Recommended Approach

Use a light-theme skin migration that concentrates changes in design tokens, the app shell, and the most visible shared surfaces.

This approach is preferred because it:

- gives the fastest visible transformation
- keeps implementation risk low
- preserves the current UX model
- creates a reusable foundation for later page-by-page polish

## Visual Direction

### Background

Replace the current mostly flat neutral app backdrop with a quiet layered field:

- soft warm-gray to cool-gray base
- one or two static radial highlights for depth
- no motion, glow sweep, halo, or particle treatment

The background should support the interface rather than become the interface.

### Shell

The top-level OpenLess window should read as a frosted control console:

- larger outer radii than today
- stronger edge softness
- slightly deeper shadow stack
- subtle translucent panel feel
- clearer distinction between sidebar zone and content zone while keeping them visually related

The shell should feel continuous and premium, not like separate floating white boxes.

### Surfaces

Introduce a more intentional surface ladder:

1. app backdrop
2. shell glass
3. main console panel
4. elevated cards and grouped containers
5. utility pills, chips, and compact controls

Each level should differ through opacity, blur, border contrast, and shadow depth rather than through aggressive color changes.

### Typography

Adopt the reference's typographic mood without sacrificing Chinese readability.

- primary UI font: modern sans with better personality than the current system-default feel
- mono font: reserved for version labels, technical badges, and small metadata
- page titles: larger, more deliberate, more architectural
- secondary labels: quieter and cleaner

If external web fonts are not already part of the app, prefer safe local or bundled-friendly fallbacks rather than adding fragile runtime font dependencies.

### Color Hierarchy

Keep OpenLess light and readable.

- main palette remains white, off-white, graphite, and soft gray
- blue remains the only expressive accent
- blue is used for active states, focus, selected indicators, and important actions
- avoid broad saturated areas
- avoid neon glow treatments

## Component Strategy

### Sidebar Navigation

Keep the same navigation items and ordering, but restyle them to match the new shell:

- calmer inactive state
- more tactile hover state
- stronger active pill treatment
- cleaner icon and label alignment
- better-integrated version and beta chip zone

The sidebar should feel like part of the same industrial-glass instrument rather than a separate admin rail.

### Overview Page

Do not reorganize the overview information model.

Instead:

- give provider cards more presence
- unify stats cards into the same radius and surface system
- style charts and recent-record containers as elevated glass/solid hybrids
- improve whitespace rhythm and title hierarchy

### Buttons, Chips, and Status

Unify small UI affordances into one design language:

- pill-like compact chips
- more sculpted primary buttons
- restrained secondary buttons
- status accents that rely on color and contrast, not glow

### Settings Modal

Retain the current settings structure and content, but reskin:

- modal container
- section tabs or navigation surfaces
- row grouping
- toggles, chips, and button styles
- footer actions

The modal should feel like it belongs to the same skin system as the main shell.

## Technical Plan

Apply the redesign through three layers.

### 1. Global Tokens

Update `src/styles/tokens.css` to define:

- revised neutral palette
- revised accent ladder
- larger radius system
- updated shadow tokens
- shell and card translucency tokens
- font stack updates

### 2. Global Chrome

Update `src/styles/global.css` and closely related shell-level styling to define:

- background gradients
- shell glass helpers
- optional subtle grain
- scrollbar polish
- global focus and interaction refinement

### 3. High-Visibility Containers

Update the shell and visible page wrappers first:

- `src/components/FloatingShell.tsx`
- settings modal shell
- overview high-visibility containers
- shared visual wrappers where existing pages already inherit common surfaces

This keeps the redesign cohesive while avoiding a broad page-by-page rewrite.

## Risks and Guardrails

### Risk: Readability Regression

Glass and translucency can reduce legibility, especially with Chinese text.

Guardrail:

- keep text surfaces more opaque than the reference
- use blur as support, not as the primary effect
- preserve strong text contrast

### Risk: Incomplete Visual Consistency

If only the shell changes, some inner surfaces may look older than the new frame.

Guardrail:

- prioritize the overview page and settings modal after tokens
- rely on shared tokens so untouched pages still improve passively

### Risk: Over-stylizing

A direct copy of the reference would conflict with OpenLess's utilitarian desktop workflow.

Guardrail:

- no dark-theme conversion
- no decorative labels
- no animated background
- no major layout theatrics

## Verification

The redesign is successful when:

- the app visibly echoes the reference's radii, frosted surfaces, and typographic hierarchy
- OpenLess still feels light, calm, and easy to scan
- navigation and settings behavior remain unchanged
- the overview page looks like part of the new skin instead of the old one
- there is no animated light ring or equivalent visual effect anywhere in the shell

## Implementation Boundaries

The first implementation pass should stop once the following are true:

- shell background is updated
- sidebar skin is updated
- main console panel is updated
- overview cards and containers are visually aligned
- settings modal visually matches the shell

Anything beyond that should be treated as a follow-up polish pass, not folded into the first skin migration.
