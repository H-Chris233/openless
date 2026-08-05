import type { DictationSession, StylePack } from './types';

/**
 * 「用原风格重试」要用的风格包 id。
 *
 * 优先取产生这条记录的风格包（session.stylePackId）——重试的目的是跟上次结果做
 * A/B 对照，必须用同一套风格，否则判断不了是模型抖动还是风格差异。包已被删除、
 * 旧历史没有 stylePackId、或顶层包列表尚未加载（allPacks 为 null）时返回 undefined，
 * 由调用方回落当前激活风格包（repolish 省略 stylePackId 的行为）。
 *
 * 注意查的是 allPacks（含已禁用包）：历史可能出自后来被禁用的包，只要包还在就能重试。
 */
export function resolveRepolishRetryPackId(
  session: Pick<DictationSession, 'stylePackId'>,
  allPacks: StylePack[] | null,
): string | undefined {
  if (!session.stylePackId || !allPacks) return undefined;
  return allPacks.some(pack => pack.id === session.stylePackId)
    ? session.stylePackId
    : undefined;
}
