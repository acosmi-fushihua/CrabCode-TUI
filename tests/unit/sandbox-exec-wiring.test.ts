import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import { buildSandboxExecArgv } from '../../src/utils/sandbox/sandboxExecConfig.js'

function source(path: string): string {
  return readFileSync(resolve(import.meta.dir, '../..', path), 'utf8')
}

describe('direct sandbox-exec wiring', () => {
  test('helper argv keeps the command byte-for-byte behind the separator', () => {
    expect(
      buildSandboxExecArgv({
        helperBin: '/opt/crabcode',
        program: '/bin/sh',
        args: ['-c', 'printf %s "$VALUE"', '--config', 'user-value'],
      }),
    ).toEqual({
      binary: '/opt/crabcode',
      args: [
        'sandbox-exec',
        '--config-stdin',
        '--',
        '/bin/sh',
        '-c',
        'printf %s "$VALUE"',
        '--config',
        'user-value',
      ],
    })
  })

  test('Shell never lets child output change the cached sandbox policy', () => {
    const shell = source('src/utils/Shell.ts')
    expect(shell).not.toContain('parseSandboxInitFailure')
    expect(shell).not.toContain('markEnforcedBackendDegraded(initFailure)')
    expect(shell).toContain('const initFailure = null')
  })

  test('the local Shell owns config construction, helper resolution and spawn', () => {
    const shell = source('src/utils/Shell.ts')
    expect(shell).toContain('buildSandboxExecConfig')
    expect(shell).toContain('buildSandboxExecArgv')
    expect(shell).toContain('resolveSandboxHelperBin')
    expect(shell).toContain('sandboxProxyChildEnv')
    expect(shell).toContain("stdio: ['pipe', 'pipe', 'pipe']")
    expect(shell).toContain("'sandbox-commands'")
    expect(shell).toContain('generateSandboxCommandId()')
    expect(shell).toContain("open(nativeCwdFilePath, 'wx+', 0o600)")
    expect(shell).toContain('cwdProbeHandle?.fd ?? nativeCwdFilePath')
    // Both the normal result continuation and every pre-result failure path
    // converge on the same idempotent resource cleanup.
    expect(shell.match(/cleanupSandboxCommandResources\(\)/g)?.length).toBeGreaterThanOrEqual(4)
  })

  test('TUI shell tools pass the actual sandbox result into violation feedback', () => {
    for (const path of [
      'src/tools/BashTool/BashTool.ts',
      'src/tools/PowerShellTool/PowerShellTool.ts',
    ]) {
      const tool = source(path)
      expect(tool).toContain('sandboxed: ranInSandbox')
      expect(tool).toContain('sandboxBackend: ranInSandbox ?')
    }
  })
})
