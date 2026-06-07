import { readFile } from 'node:fs/promises';

function assertMatch(source, pattern, name) {
  if (!pattern.test(source)) {
    throw new Error(`${name}: pattern ${pattern} not found`);
  }
}

// show_capsule_window_no_activate 已随 capsule/IME 窗口插入管线拆到
// coordinator/ime_insertion.rs（SRP 拆分），契约函数本身未改。
const coordinatorRs = (
  await readFile(new URL('../src-tauri/src/coordinator/ime_insertion.rs', import.meta.url), 'utf-8')
).replace(/\r\n/g, '\n');
const functionMatch = coordinatorRs.match(
  /#\[cfg\(target_os = "macos"\)\]\s*(?:pub\(crate\) )?fn show_capsule_window_no_activate[\s\S]*?\n}\n\n#\[cfg\(target_os = "linux"\)\]/,
);

if (!functionMatch) {
  throw new Error('macOS capsule no-activate function not found');
}

const macosNoActivateFunction = functionMatch[0];
const executableMacosNoActivateFunction = macosNoActivateFunction.replace(/\/\/.*$/gm, '');

assertMatch(
  macosNoActivateFunction,
  /set_visible_on_all_workspaces\(true\)[\s\S]*?orderFrontRegardless/,
  'macOS capsule should join all Spaces before showing without activation',
);

assertMatch(
  macosNoActivateFunction,
  /FULL_SCREEN_AUXILIARY[\s\S]*?1 << 8[\s\S]*?setCollectionBehavior[\s\S]*?orderFrontRegardless/,
  'macOS capsule should join fullscreen Spaces as an auxiliary window before showing without activation',
);

for (const forbidden of ['window.show()', 'set_focus', 'NSApp.activate', 'makeKeyAndOrderFront']) {
  if (executableMacosNoActivateFunction.includes(forbidden)) {
    throw new Error(`macOS capsule no-activate path must not call ${forbidden}`);
  }
}
