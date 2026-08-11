import { describe, expect, test } from 'bun:test'

import {
  assertTuiRuntimeSmokeSuccess,
  assertCommandCatalogChangedRequest,
  commandCatalogChangedAck,
  commandCatalogChangedSubtype,
  commandNameRoundTrips,
  expectedTuiRuntimeIdentity,
} from '../../scripts/tui-runtime-smoke-contract.mjs'
import { parseSlashCommand } from '../../src/utils/slashCommandParsing.js'

const command = (name: string, extra: Record<string, unknown> = {}) => ({
  name,
  description: 'description',
  argumentHint: '',
  ...extra,
})

const frame = (commands: unknown[]) => ({
  type: 'control_request',
  request_id: 'catalog-1',
  request: {
    subtype: commandCatalogChangedSubtype,
    protocol_version: 1,
    commands,
  },
})

describe('TUI runtime smoke private command-catalog contract', () => {
  test('requires an exact non-empty release identity', () => {
    expect(
      expectedTuiRuntimeIdentity({
        version: '1.0.30',
        buildId: '1.0.30+release-commit',
      }),
    ).toEqual({
      version: '1.0.30',
      buildId: '1.0.30+release-commit',
    })
    expect(() => expectedTuiRuntimeIdentity({ buildId: 'present' })).toThrow(
      'non-empty version and buildId',
    )
    expect(() => expectedTuiRuntimeIdentity({ version: '1.0.30' })).toThrow(
      'non-empty version and buildId',
    )
  })

  test('shares the exact successful runtime report fields with release consumers', () => {
    const successful = {
      rendererContext: 'received',
      initialize: 'success',
      costTurns: '2/2 success',
      endSession: 'success',
      exitCode: 0,
    }
    expect(assertTuiRuntimeSmokeSuccess(successful)).toBe(successful)
    expect(() =>
      assertTuiRuntimeSmokeSuccess({
        ...successful,
        costTurns: undefined,
        turns: '2/2 success',
      }),
    ).toThrow('not successful')
    expect(() =>
      assertTuiRuntimeSmokeSuccess({ ...successful, exitCode: 1 }),
    ).toThrow('not successful')
  })

  test('matches the production slash parser for ordinary and MCP names', () => {
    const names = [
      'cost',
      '(MCP)',
      'mcp:tool (MCP)',
      'mcp:tool',
      '',
      ' two',
      'two ',
      'two words',
      'two\twords',
      'two\u00a0words',
      'tool  (MCP)',
      'tool\t(MCP)',
      'tool (MCP) arg',
    ]
    for (const name of names) {
      const parsed = parseSlashCommand(`/${name}`)
      const expected =
        parsed !== null &&
        parsed.commandName === name &&
        parsed.args === ''
      expect(commandNameRoundTrips(name)).toBe(expected)
    }

    expect(() =>
      assertCommandCatalogChangedRequest(
        frame([
          command('cost', { builtin: true }),
          command('mcp:tool (MCP)', { hidden: true }),
        ]),
      ),
    ).not.toThrow()
  })

  test('rejects loose metadata, duplicates, and non-literal flags', () => {
    expect(() =>
      assertCommandCatalogChangedRequest({
        ...frame([command('cost')]),
        request_id: '',
      }),
    ).toThrow('invalid request_id')
    expect(() =>
      assertCommandCatalogChangedRequest({
        ...frame([command('cost')]),
        request: {
          ...frame([]).request,
          commands: [command('cost')],
          extra: true,
        },
      }),
    ).toThrow('invalid fields')
    expect(() =>
      assertCommandCatalogChangedRequest(
        frame([command('cost'), command('cost')]),
      ),
    ).toThrow('is invalid')
    expect(() =>
      assertCommandCatalogChangedRequest(
        frame([command('cost', { hidden: false })]),
      ),
    ).toThrow('is invalid')
    expect(() =>
      assertCommandCatalogChangedRequest(frame([command('two words')])),
    ).toThrow('is invalid')
  })

  test('returns the exact correlated ACK payload', () => {
    expect(commandCatalogChangedAck()).toEqual({
      protocol_version: 1,
      received: true,
    })
    expect(Object.keys(commandCatalogChangedAck()).sort()).toEqual([
      'protocol_version',
      'received',
    ])
  })
})
