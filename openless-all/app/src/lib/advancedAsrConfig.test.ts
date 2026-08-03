import {
  parseAdvancedAsrConfig,
  serializeAdvancedAsrConfig,
} from './advancedAsrConfig'

function assertEqual(actual: unknown, expected: unknown, name: string) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`${name}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`)
  }
}

assertEqual(
  parseAdvancedAsrConfig(null),
  { verboseJson: false, chunkDurationMs: null },
  'null raw falls back to conservative defaults',
)

assertEqual(
  parseAdvancedAsrConfig(''),
  { verboseJson: false, chunkDurationMs: null },
  'empty raw falls back to conservative defaults',
)

assertEqual(
  parseAdvancedAsrConfig('not-json'),
  { verboseJson: false, chunkDurationMs: null },
  'invalid JSON falls back to conservative defaults',
)

assertEqual(
  parseAdvancedAsrConfig('[1,2]'),
  { verboseJson: false, chunkDurationMs: null },
  'non-object JSON falls back to conservative defaults',
)

assertEqual(
  parseAdvancedAsrConfig('{"verboseJson":true}'),
  { verboseJson: true, chunkDurationMs: null },
  'missing chunkDurationMs stays null',
)

assertEqual(
  parseAdvancedAsrConfig('{"chunkDurationMs":0}'),
  { verboseJson: false, chunkDurationMs: null },
  'zero chunk duration means no chunking',
)

assertEqual(
  parseAdvancedAsrConfig('{"chunkDurationMs":30000.9,"verboseJson":false}'),
  { verboseJson: false, chunkDurationMs: 30000 },
  'chunk duration is floored to integer',
)

assertEqual(
  parseAdvancedAsrConfig('{"verboseJson":"yes"}'),
  { verboseJson: false, chunkDurationMs: null },
  'non-boolean verboseJson falls back to false',
)

assertEqual(
  parseAdvancedAsrConfig(serializeAdvancedAsrConfig({ verboseJson: true, chunkDurationMs: 30000 })),
  { verboseJson: true, chunkDurationMs: 30000 },
  'serialize/parse round-trip preserves config',
)
