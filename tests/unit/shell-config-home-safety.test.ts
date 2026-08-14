import { describe, expect, test } from 'bun:test'
import { mkdtempSync, mkdirSync, rmSync, symlinkSync } from 'fs'
import { tmpdir } from 'os'
import { join } from 'path'

import {
  getEmptyToolPermissionContext,
  type ToolPermissionContext,
  type ToolUseContext,
} from '../../src/Tool.js'
import { getDefaultAppState } from '../../src/state/AppStateStore.js'
import { BashTool } from '../../src/tools/BashTool/BashTool.js'
import { PowerShellTool } from '../../src/tools/PowerShellTool/PowerShellTool.js'
import { runWithCwdOverride } from '../../src/utils/cwd.js'
import { evaluateBypassImmunePermissionFloor } from '../../src/utils/permissions/permissions.js'
import {
  checkShellMutationSafetyFloor,
  type ShellMutationCommand,
} from '../../src/utils/permissions/shellMutationSafety.js'

function bypassPermissionContext(): ToolPermissionContext {
  return {
    ...getEmptyToolPermissionContext(),
    mode: 'bypassPermissions',
    isBypassPermissionsModeAvailable: true,
  }
}

function bypassToolUseContext(): ToolUseContext {
  let appState = getDefaultAppState()
  appState = {
    ...appState,
    toolPermissionContext: bypassPermissionContext(),
  }
  return {
    abortController: new AbortController(),
    getAppState: () => appState,
    setAppState: update => {
      appState = update(appState)
    },
    options: { tools: [] },
  } as unknown as ToolUseContext
}

function check(
  argv: readonly string[],
  cwd: string,
  inspectUnknownArguments = true,
) {
  const command: ShellMutationCommand = { argv, inspectUnknownArguments }
  return checkShellMutationSafetyFloor(
    [command],
    cwd,
    bypassPermissionContext(),
  )
}

describe('shell config-home bypass-immune safety floor', () => {
  // The PowerShell permission path cold-starts its bounded AST-parser process.
  // Its own retry budget can legitimately exceed Bun's 5-second test default
  // on a loaded CI host, so the outer test must not preempt that fail-closed path.
  test('covers hidden Bash/native outputs and delegated execution without flagging safe values', async () => {
    const root = mkdtempSync(join(tmpdir(), 'crabcode-shell-floor-'))
    const workspace = join(root, 'workspace')
    const physicalConfig = join(root, 'physical-config')
    const selectedConfig = join(root, 'selected-config')
    const alternateAlias = join(root, 'config:alias')
    const urlAliasParent = join(root, 'config:')
    const urlShapedAlias = `${urlAliasParent}//alias`
    const ordinaryTarget = join(workspace, 'build', 'output.txt')
    const sensitiveTarget = join(selectedConfig, 'plugins', 'demo', 'plugin.json')
    const aliasTarget = join(alternateAlias, 'settings.json')
    const previousConfig = process.env.CRABCODE_CONFIG_DIR

    mkdirSync(join(workspace, 'build'), { recursive: true })
    mkdirSync(physicalConfig, { recursive: true })
    mkdirSync(urlAliasParent, { recursive: true })
    symlinkSync(physicalConfig, selectedConfig, 'dir')
    symlinkSync(physicalConfig, alternateAlias, 'dir')
    symlinkSync(physicalConfig, join(urlAliasParent, 'alias'), 'dir')
    process.env.CRABCODE_CONFIG_DIR = selectedConfig

    try {
      const unsafeCommands: readonly (readonly string[])[] = [
        ['tee', sensitiveTarget],
        ['tee', `${urlShapedAlias}/settings.json`],
        ['env', 'tee', aliasTarget],
        ['install', 'source.txt', ordinaryTarget],
        ['rsync', '-e', 'sh -c id', 'source.txt', ordinaryTarget],
        ['dd', 'if=/dev/null', `of=${sensitiveTarget}`],
        ['truncate', '-s', '0', sensitiveTarget],
        ['ln', sensitiveTarget, ordinaryTarget],
        ['ln', '-s', sensitiveTarget, ordinaryTarget],
        ['tar', '-xf', 'archive.tar', '-C', join(workspace, 'build')],
        ['unzip', 'archive.zip', '-d', join(workspace, 'build')],
        ['7z', 'x', 'archive.7z', '-l'],
        ['curl', '-o', sensitiveTarget, 'https://example.com/file'],
        ['curl', '-o', `${urlShapedAlias}/curl.out`, 'https://example.com/file'],
        ['curl', `-sSo${sensitiveTarget}`, 'https://example.com/file'],
        ['curl', '-K', 'curl.conf', 'https://example.com/file'],
        ['curl', '--remote-name-all', 'https://example.com/file'],
        ['curl', `-sw%output{${sensitiveTarget}}`, 'https://example.com/file'],
        ['curl', '--expand-output', sensitiveTarget, 'https://example.com/file'],
        ['curl', '--expand-write-out=%output{target}', 'https://example.com/file'],
        ['wget', '-O', sensitiveTarget, 'https://example.com/file'],
        ['wget', `-qO${sensitiveTarget}`, 'https://example.com/file'],
        ['wget', '--spider', '--use-askpass=helper', 'https://example.com/file'],
        ['wget', '--spider', '--background', 'https://example.com/file'],
        ['wget', '--warc-tempdir', sensitiveTarget, 'https://example.com/file'],
        ['wget', 'https://example.com/file'],
        ['scp', '-S', '/tmp/untrusted-ssh', 'host:file', ordinaryTarget],
        ['awk', `BEGIN { print "x" > "${sensitiveTarget}" }`],
        ['sed', '-i', 's/x/y/', sensitiveTarget],
        ['sort', '-o', sensitiveTarget, 'input.txt'],
        ['git', 'clone', 'https://example.com/repo.git', sensitiveTarget],
        ['git', 'archive', '-o', sensitiveTarget, 'HEAD'],
        ['git', 'format-patch', '--output-directory', sensitiveTarget, 'HEAD~1'],
        ['git', 'bundle', 'create', sensitiveTarget, 'HEAD'],
        ['git', '-c', `alias.x=!tee ${sensitiveTarget}`, 'x'],
        ['rg', '--pre', 'sh -c id', 'needle', '.'],
        ['python3', 'script.py'],
        ['opaque-mutator', aliasTarget],
      ]

      for (const argv of unsafeCommands) {
        expect(check(argv, workspace), argv.join(' ')).toMatchObject({
          behavior: 'ask',
          decisionReason: { type: 'safetyCheck' },
        })
      }

      const safeCommands: readonly (readonly string[])[] = [
        ['echo', sensitiveTarget],
        ['printf', '%s', sensitiveTarget],
        ['tee', ordinaryTarget],
        ['tee', 'https://example.com/local-output'],
        ['dd', 'if=/dev/null', `of=${ordinaryTarget}`],
        ['rsync', '-av', 'source.txt', ordinaryTarget],
        ['scp', '-q', 'host:file', ordinaryTarget],
        ['curl', '-D', ordinaryTarget, 'https://example.com/file'],
        ['wget', '--spider', 'https://example.com/file'],
        ['tar', '-tf', 'archive.tar'],
        ['unzip', '-l', 'archive.zip'],
        ['7z', 'l', 'archive.7z'],
        ['git', 'clone', 'https://example.com/repo.git', join(workspace, 'build', 'repo')],
        ['git', 'commit', '-m', sensitiveTarget],
        ['git', 'commit', `--message=${sensitiveTarget}`],
        ['awk', '$1 > 3 { print $1 }', 'data.txt'],
        ['sed', 's/x/y/', sensitiveTarget],
        ['sort', sensitiveTarget],
        ['opaque-mutator', 'https://example.com/.crabcode/settings.json'],
      ]

      for (const argv of safeCommands) {
        expect(check(argv, workspace), argv.join(' ')).toMatchObject({
          behavior: 'passthrough',
        })
      }

      // PowerShell native applications use the same argv floor (.exe names
      // are normalized); cmdlet value arguments remain outside the unknown
      // native-application fallback.
      expect(check(['curl.exe', '-o', aliasTarget, 'https://example.com'], workspace)).toMatchObject({
        behavior: 'ask',
        decisionReason: { type: 'safetyCheck' },
      })
      expect(check(['opaque-mutator.exe', aliasTarget], workspace)).toMatchObject({
        behavior: 'ask',
        decisionReason: { type: 'safetyCheck' },
      })
      expect(check(['Write-Output', sensitiveTarget], workspace, false)).toMatchObject({
        behavior: 'passthrough',
      })

      // Exercise the real BashTool -> central full-access floor, not only the
      // helper. A protected hidden output must project to prompt, while value
      // text and an ordinary explicit destination stay auto-allowable.
      await runWithCwdOverride(workspace, async () => {
        expect(
          await evaluateBypassImmunePermissionFloor(
            BashTool,
            { command: `tee '${sensitiveTarget}'` },
            bypassToolUseContext(),
          ),
        ).toMatchObject({
          kind: 'prompt',
          decision: {
            behavior: 'ask',
            decisionReason: { type: 'safetyCheck' },
          },
        })
        expect(
          await evaluateBypassImmunePermissionFloor(
            BashTool,
            { command: 'tee "$UNRESOLVED_OUTPUT"' },
            bypassToolUseContext(),
          ),
        ).toMatchObject({
          kind: 'prompt',
          decision: {
            behavior: 'ask',
            decisionReason: { type: 'safetyCheck' },
          },
        })
        expect(
          await evaluateBypassImmunePermissionFloor(
            BashTool,
            { command: `opaque-mutator '${aliasTarget}'` },
            bypassToolUseContext(),
          ),
        ).toMatchObject({
          kind: 'prompt',
          decision: {
            behavior: 'ask',
            decisionReason: { type: 'safetyCheck' },
          },
        })
        expect(
          await evaluateBypassImmunePermissionFloor(
            BashTool,
            { command: `echo '${sensitiveTarget}'` },
            bypassToolUseContext(),
          ),
        ).toEqual({ kind: 'allow' })
        expect(
          await evaluateBypassImmunePermissionFloor(
            BashTool,
            { command: `tee '${ordinaryTarget}'` },
            bypassToolUseContext(),
          ),
        ).toEqual({ kind: 'allow' })

        expect(
          await evaluateBypassImmunePermissionFloor(
            PowerShellTool,
            {
              command: `curl.exe -o '${aliasTarget}' https://example.com/file`,
            },
            bypassToolUseContext(),
          ),
        ).toMatchObject({
          kind: 'prompt',
          decision: {
            behavior: 'ask',
            decisionReason: { type: 'safetyCheck' },
          },
        })
        expect(
          await evaluateBypassImmunePermissionFloor(
            PowerShellTool,
            { command: `Write-Output '${sensitiveTarget}'` },
            bypassToolUseContext(),
          ),
        ).toEqual({ kind: 'allow' })
      })
    } finally {
      if (previousConfig === undefined) delete process.env.CRABCODE_CONFIG_DIR
      else process.env.CRABCODE_CONFIG_DIR = previousConfig
      rmSync(root, { recursive: true, force: true })
    }
  }, 30_000)
})
