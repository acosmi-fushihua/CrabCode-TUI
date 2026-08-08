import { describe, expect, test } from 'bun:test'
import { parseWindowsProcessInventory } from '../../scripts/release-package-smoke.mjs'

describe('release package Windows process inventory', () => {
  test('prefilters one CIM snapshot to package-named executable candidates', () => {
    const inventory = JSON.stringify([
      { ProcessId: 41, ExecutablePath: 'C:\\release\\CrabCode.EXE' },
      { ProcessId: 42, ExecutablePath: 'C:\\release\\bun.exe' },
      { ProcessId: 43, ExecutablePath: 'C:\\Windows\\System32\\powershell.exe' },
      { ProcessId: 44, ExecutablePath: null },
      { ProcessId: 'not-a-pid', ExecutablePath: 'C:\\release\\crabcode-tui.exe' },
    ])

    expect(parseWindowsProcessInventory(inventory)).toEqual([
      { pid: 41, executable: 'C:\\release\\CrabCode.EXE' },
      { pid: 42, executable: 'C:\\release\\bun.exe' },
    ])
  })

  test('normalizes the one-object and empty CIM JSON shapes', () => {
    expect(
      parseWindowsProcessInventory(
        JSON.stringify({
          ProcessId: 51,
          ExecutablePath: 'C:\\release\\acosmi-memory-orchestrator.exe',
        }),
      ),
    ).toEqual([
      {
        pid: 51,
        executable: 'C:\\release\\acosmi-memory-orchestrator.exe',
      },
    ])
    expect(parseWindowsProcessInventory('')).toEqual([])
  })

  test('fails closed on malformed CIM output', () => {
    expect(() => parseWindowsProcessInventory('{')).toThrow()
  })
})
