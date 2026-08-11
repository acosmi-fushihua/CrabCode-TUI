import { describe, expect, test } from 'bun:test'

import { parseSlashCommand } from '../../src/utils/slashCommandParsing.js'

describe('slash command parsing', () => {
  test('uses ECMAScript whitespace separators consistently with the direct TUI', () => {
    for (const separator of [' ', '\t', '\n', '\u00a0', '\u2028', '\ufeff']) {
      expect(parseSlashCommand(`/compact${separator}keep details`)).toEqual({
        commandName: 'compact',
        args: 'keep details',
        isMcp: false,
      })
    }
  })

  test('retains the historical MCP marker while accepting Unicode separators', () => {
    expect(parseSlashCommand('/server\u00a0(MCP)\u2028arg one')).toEqual({
      commandName: 'server (MCP)',
      args: 'arg one',
      isMcp: true,
    })
  })

  test('preserves the argument tail after consuming exactly one separator', () => {
    expect(parseSlashCommand('/command   spaced')).toEqual({
      commandName: 'command',
      args: '  spaced',
      isMcp: false,
    })
  })
})
