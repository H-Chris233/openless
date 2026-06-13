import { readFile } from 'node:fs/promises';

function assertMatch(source, pattern, name) {
  if (!pattern.test(source)) {
    throw new Error(`${name}: pattern ${pattern} not found`);
  }
}

// 契约函数 show_capsule_window_no_activate 的实现就在编译进二进制的 coordinator.rs 里。
// （coordinator/ime_insertion.rs 曾是它的一份副本，但从未被 mod 进 crate —— 死代码，
//  已删除；契约必须校验真正编译的那份，否则会出现「测试绿、线上坏」的假信心。）
const coordinatorRs = (
  await readFile(new URL('../src-tauri/src/coordinator.rs', import.meta.url), 'utf-8')
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
  /CAN_JOIN_ALL_SPACES[\s\S]*?1 << 0[\s\S]*?setCollectionBehavior[\s\S]*?orderFrontRegardless/,
  'macOS capsule should join all Spaces via an absolute collectionBehavior write before showing without activation',
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
