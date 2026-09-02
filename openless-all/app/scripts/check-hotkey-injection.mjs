import { spawnSync } from 'node:child_process';

if (process.platform === 'win32') {
  // The full Tauri lib-test binary is compile-only on clean Windows runners because an
  // optional native runtime DLL is unavailable there. CI executes this behavioral gate on
  // macOS/Linux and still compiles the same Rust test on Windows via `cargo test --no-run`.
  console.log('Hotkey injection runtime gate skipped on Windows (covered by lib-test compilation).');
  process.exit(0);
}

const result = spawnSync(
  'cargo',
  ['test', '--manifest-path', 'src-tauri/Cargo.toml', 'hotkey_injection_gate_logs_pressed_and_cancels', '--', '--nocapture'],
  {
    env: { ...process.env, OPENLESS_HOTKEY_INJECTION_DRY_RUN: '1' },
    encoding: 'utf8',
  },
);

const output = `${result.stdout ?? ''}${result.stderr ?? ''}`;
for (const chunk of (result.stdout ?? '').match(/[\s\S]{1,8192}/g) ?? []) process.stdout.write(chunk);
for (const chunk of (result.stderr ?? '').match(/[\s\S]{1,8192}/g) ?? []) process.stderr.write(chunk);

if (result.status !== 0) {
  if (result.error) console.error(result.error);
  process.exit(result.status ?? 1);
}

if (!output.includes('[coord] hotkey pressed')) {
  console.error("Hotkey injection gate did not emit '[coord] hotkey pressed'.");
  process.exit(1);
}

console.log('Hotkey injection gate passed.');
