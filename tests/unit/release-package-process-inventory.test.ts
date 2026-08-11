import { describe, expect, test } from 'bun:test'
import { parseWindowsTasklistInventory } from '../../scripts/release-package-smoke.mjs'

describe('release package Windows process inventory', () => {
  test('prefilters the native tasklist snapshot to package-named candidates', () => {
    const inventory = [
      '"CrabCode.EXE","41","Console","1","12,000 K"',
      '"bun.exe","42","Console","1","8,000 K"',
      '"powershell.exe","43","Console","1","30,000 K"',
      '"crabcode-tui.exe","not-a-pid","Console","1","10,000 K"',
    ].join('\r\n')

    expect(parseWindowsTasklistInventory(inventory)).toEqual([
      { pid: 41, executable: null },
      { pid: 42, executable: null },
    ])
  })

  test('accepts all shipped executable names and empty tasklist output', () => {
    expect(
      parseWindowsTasklistInventory(
        '"acosmi-memory-orchestrator.exe","51","Console","1","9,000 K"',
      ),
    ).toEqual([
      {
        pid: 51,
        executable: null,
      },
    ])
    expect(parseWindowsTasklistInventory('')).toEqual([])
  })

  test('ignores malformed native inventory rows', () => {
    expect(parseWindowsTasklistInventory('INFO: no tasks are running')).toEqual([])
  })
})
