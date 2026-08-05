import { resolveRepolishRetryPackId } from './history-repolish';
import type { StylePack } from './types';

function assert(condition: boolean, message: string) {
  if (!condition) throw new Error(message);
}

function pack(id: string, enabled: boolean): StylePack {
  return {
    id,
    name: `包 ${id}`,
    description: '',
    version: '1.0.0',
    kind: 'imported',
    baseMode: 'structured',
    selectionPrompt: '',
    prompt: '',
    examples: [],
    tags: [],
    enabled,
    active: false,
  };
}

const allPacks: StylePack[] = [
  pack('builtin.structured', true),
  pack('custom-alive', true),
  pack('custom-disabled', false),
];

// 原风格包存在（启用）→ 返回该 id。
assert(
  resolveRepolishRetryPackId({ stylePackId: 'custom-alive' }, allPacks) === 'custom-alive',
  'retry should use the original pack id when the pack still exists',
);

// 原风格包已被禁用 → 仍返回该 id（历史可能出自后来被禁用的包，只要包还在就能重试）。
assert(
  resolveRepolishRetryPackId({ stylePackId: 'custom-disabled' }, allPacks) === 'custom-disabled',
  'retry should use the original pack id even when the pack is disabled',
);

// 内置包同样按原 id 重试。
assert(
  resolveRepolishRetryPackId({ stylePackId: 'builtin.structured' }, allPacks) === 'builtin.structured',
  'retry should use the builtin pack id as-is',
);

// 包已被删除 → 回落（undefined，调用方走当前激活包）。
assert(
  resolveRepolishRetryPackId({ stylePackId: 'deleted-pack' }, allPacks) === undefined,
  'retry should fall back when the original pack was deleted',
);

// 旧历史没有 stylePackId → 回落。
assert(
  resolveRepolishRetryPackId({ stylePackId: null }, allPacks) === undefined,
  'retry should fall back when the record has no stylePackId',
);

// 顶层包列表尚未加载（null）→ 回落。
assert(
  resolveRepolishRetryPackId({ stylePackId: 'custom-alive' }, null) === undefined,
  'retry should fall back while style packs are still loading',
);
