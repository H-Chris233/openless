import { reconcileLessComputerReplay } from './lessComputerReplay';
import type { LessComputerEvent, LessComputerSyncResult } from './types';

function assertDeepEqual(actual: unknown, expected: unknown, name: string) {
  const actualJson = JSON.stringify(actual);
  const expectedJson = JSON.stringify(expected);
  if (actualJson !== expectedJson) {
    throw new Error(`${name}: expected ${expectedJson}, got ${actualJson}`);
  }
}

const replay: LessComputerSyncResult = {
  events: [
    { kind: 'user', text: 'question', fresh: true, seq: 4 },
    { kind: 'started', seq: 5 },
  ],
  oldestSequence: 2,
  latestSequence: 5,
  truncated: false,
};
const pending: LessComputerEvent[] = [
  { kind: 'started', seq: 5 },
  { kind: 'delta', text: 'answer', seq: 6 },
];

assertDeepEqual(reconcileLessComputerReplay(3, replay, pending), {
  events: [
    { kind: 'user', text: 'question', fresh: true, seq: 4 },
    { kind: 'started', seq: 5 },
    { kind: 'delta', text: 'answer', seq: 6 },
  ],
  latestAppliedSequence: 6,
  reset: false,
}, 'merges pending events after replay without duplicates');

assertDeepEqual(
  reconcileLessComputerReplay(
    99,
    { ...replay, truncated: true },
    [{ kind: 'delta', text: 'tail without sequence' }],
  ),
  {
    events: [
      { kind: 'user', text: 'question', fresh: true, seq: 4 },
      { kind: 'started', seq: 5 },
      { kind: 'delta', text: 'tail without sequence' },
    ],
    latestAppliedSequence: 5,
    reset: true,
  },
  'rebuilds state from a truncated replay',
);

console.log('lessComputerReplay.test.ts passed');
