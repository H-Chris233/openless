# Releasing OpenLess

This document defines who may cut releases and how. It is policy, not a how-to for
day-to-day development.

## Authority: admins only

**Only repository administrators may create version tags and publish releases.**

- Only an admin may create a release tag (`v*-tauri`), invoke the Linux asset workflow for an existing release, or publish a GitHub Release.
- Contributors (including AI agents) **must not** create release tags, publish
  releases, or trigger release automation. If a release is needed, **request an admin
  to cut it** — open an issue or ping a maintainer with the target version and the
  channel (Beta or Stable).
- Release automation (CI workflows that build, sign, and publish artifacts, and the
  auto-updater feed) must be **admin-triggered only**. Tag-triggered pipelines are
  considered admin-triggered because only admins may push the triggering tag.

## Channels and tags

Two channels; the branch name equals the channel name:

- **`beta`** — default development branch and the Beta channel.
- **`main`** — the Stable channel (正式版). Always releasable; only maintainers merge
  `beta → main`.

Tauri host release tags (created by an admin only):

- **Stable release:** push tag `v<version>-tauri`.
- **Beta release:** push tag `v<X.Y.Z>-Beta.<N>-tauri`, for example
  `v1.3.15-Beta.1-tauri` (published as a GitHub pre-release; never auto-updates
  Stable users). The updater still recognizes the historical `*-beta-tauri`
  suffix for existing releases, but new releases use the `Beta.<N>` form.

These tags publish the macOS, Windows, and Android Tauri hosts. Linux is not part of the Tauri matrix. Its non-UI backend/Adapter is built by `.github/workflows/release-linux-egui.yml`, which accepts an existing `release_tag` and writes `latest-linux-egui-x86_64.json`. The workflow intentionally has no automatic tag trigger while `openless-all/app/linux-egui/src/main.rs` is still the UI-team stub.

## Version-sync gate

A Tauri release fails CI unless **five** locations carry the same version. Bump them together
with `scripts/bump-version.sh <X.Y.Z>`:

- `openless-all/app/package.json`
- `openless-all/app/package-lock.json` (root and nested `packages.""`)
- `openless-all/app/src-tauri/tauri.conf.json`
- `openless-all/app/src-tauri/Cargo.toml`
- `openless-all/app/src-tauri/Cargo.lock` (the `name = "openless"` block)

The root `openless-all/app/Cargo.lock` belongs only to the framework-independent core/Linux workspace and is not one of the five Tauri application version locations.

## License boundary

Published 1.x releases remain MIT. `2.0.0-Beta.1` is the effective boundary for
the repository's `AGPL-3.0-only` license; third-party vendor files retain their
own MIT, Apache, LGPL, or other upstream terms.

The script takes a plain `X.Y.Z`; for a prerelease version such as
`X.Y.Z-Beta.N`, edit the files by hand.

## Pre-release checklist (for the admin cutting the release)

1. Branch is the intended channel (`beta` for Beta, `main` for Stable).
2. All five version files match (version-sync gate green).
3. CI is green on the commit being tagged.
4. Then, and only then, push the release tag.

Before attaching Linux assets, additionally require all of the following:

1. The egui team has replaced `linux-egui/src/main.rs` with the real `eframe::App`; the stub fails the product gate even if packaging succeeds.
2. Linux core/host tests, dependency gates, and secret-surface gates are green on Ubuntu.
3. The manual Linux workflow verifies the ELF dependency list, deb/rpm/AppImage contents, desktop metadata, fcitx5 plugin paths, minisign output, and independent updater manifest.
4. An admin passes the already published Tauri release tag as `release_tag`; a non-empty tag also requires `LINUX_EGUI_MINISIGN_SECRET_KEY`.

## Process summary

1. Land work on `beta` via PRs (open PRs against `beta`, never `main`).
2. For a Stable release, a maintainer merges `beta → main`.
3. An **admin** bumps the Tauri version (five-location sync), verifies CI is green,
   and pushes the release tag, which triggers the macOS/Windows/Android publish and
   auto-update pipeline.
4. After the Linux UI gate is enabled and Linux packaging has passed on its native
   runner, an admin invokes the independent Linux workflow against that existing tag.
