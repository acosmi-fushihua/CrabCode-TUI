import { describe, expect, test } from 'bun:test'
import {
  mkdtempSync,
  mkdirSync,
  realpathSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'fs'
import { tmpdir } from 'os'
import { dirname, join } from 'path'

import {
  getEmptyToolPermissionContext,
  type ToolUseContext,
} from '../../src/Tool.js'
import { getDefaultAppState } from '../../src/state/AppStateStore.js'
import { getAutoMemPath } from '../../src/memdir/paths.js'
import { FileEditTool } from '../../src/tools/FileEditTool/FileEditTool.js'
import { FileReadTool } from '../../src/tools/FileReadTool/FileReadTool.js'
import { FileWriteTool } from '../../src/tools/FileWriteTool/FileWriteTool.js'
import { checkPathConstraints as checkPowerShellPathConstraints } from '../../src/tools/PowerShellTool/pathValidation.js'
import { getSessionId } from '../../src/bootstrap/state.js'
import type { ParsedPowerShellCommand } from '../../src/utils/powershell/parser.js'
import {
  getPlanFilePath,
  getPlansDirectory,
  setPlanSlug,
} from '../../src/utils/plans.js'
import {
  checkReadPermissionForTool,
  pathInTrustedRoot,
} from '../../src/utils/permissions/filesystem.js'
import { evaluateBypassImmunePermissionFloor } from '../../src/utils/permissions/permissions.js'
import { validatePath } from '../../src/utils/permissions/pathValidation.js'

function createBypassContext(): ToolUseContext {
  let appState = getDefaultAppState()
  appState = {
    ...appState,
    toolPermissionContext: {
      ...appState.toolPermissionContext,
      mode: 'bypassPermissions',
      isBypassPermissionsModeAvailable: true,
    },
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

async function evaluateWrite(path: string) {
  return evaluateBypassImmunePermissionFloor(
    FileWriteTool,
    { file_path: path, content: 'test' },
    createBypassContext(),
  )
}

async function evaluateEdit(path: string) {
  return evaluateBypassImmunePermissionFloor(
    FileEditTool,
    {
      file_path: path,
      old_string: 'before',
      new_string: 'after',
      replace_all: false,
    },
    createBypassContext(),
  )
}

async function evaluateRead(path: string) {
  return evaluateBypassImmunePermissionFloor(
    FileReadTool,
    { file_path: path },
    createBypassContext(),
  )
}

describe('config-home bypass-immune filesystem safety', () => {
  test('does not alias distinct private/tmp trees outside Darwin', () => {
    if (process.platform !== 'darwin' && process.platform !== 'win32') {
      expect(
        pathInTrustedRoot('/private/tmp/root/file', '/tmp/root'),
      ).toBe(false)
    }
  })

  test('evaluates deny rules on any identity and requires allow coverage for every identity', () => {
    const root = mkdtempSync(join(tmpdir(), 'crabcode-rule-identity-'))
    const physical = join(root, 'physical')
    const alias = join(root, 'alias')
    const target = join(alias, 'output.txt')
    mkdirSync(physical, { recursive: true })
    symlinkSync(physical, alias, 'dir')
    const canonicalPhysical = realpathSync(physical)

    try {
      const lexicalAllowOnly = {
        ...getEmptyToolPermissionContext(),
        alwaysAllowRules: { session: [`Edit(/${alias}/**)`] },
      }
      expect(
        validatePath(target, root, lexicalAllowOnly, 'create'),
      ).toMatchObject({ allowed: false })

      const allIdentityAllow = {
        ...getEmptyToolPermissionContext(),
        alwaysAllowRules: {
          session: [
            `Edit(/${alias}/**)`,
            `Edit(/${canonicalPhysical}/**)`,
          ],
        },
      }
      expect(
        validatePath(target, root, allIdentityAllow, 'create'),
      ).toMatchObject({
        allowed: true,
        decisionReason: { type: 'rule' },
      })

      const resolvedDenyWins = {
        ...allIdentityAllow,
        alwaysDenyRules: {
          session: [`Edit(/${canonicalPhysical}/**)`],
        },
      }
      expect(
        validatePath(target, root, resolvedDenyWins, 'create'),
      ).toMatchObject({
        allowed: false,
        decisionReason: { type: 'rule' },
      })
    } finally {
      rmSync(root, { recursive: true, force: true })
    }
  })

  test('protects a custom and symlinked config identity while retaining only narrow internal writes', async () => {
    const tempRoot = mkdtempSync(join(tmpdir(), 'crabcode-config-safety-'))
    const physicalConfig = join(tempRoot, 'custom-policy-store')
    const selectedConfig = join(tempRoot, 'selected-config')
    const alternateAlias = join(tempRoot, 'alternate-alias')
    const previousConfigDir = process.env.CRABCODE_CONFIG_DIR

    mkdirSync(physicalConfig, { recursive: true })
    symlinkSync(physicalConfig, selectedConfig, 'dir')
    symlinkSync(physicalConfig, alternateAlias, 'dir')
    process.env.CRABCODE_CONFIG_DIR = selectedConfig
    getAutoMemPath.cache.clear?.()

    // getPlansDirectory is intentionally memoized for a production session.
    // Reset it here because this isolated contract test changes the selected
    // config home after module initialization.
    getPlansDirectory.cache.clear?.()
    setPlanSlug(getSessionId(), 'permission-floor-plan')

    try {
      const sensitiveTargets = [
        selectedConfig,
        join(selectedConfig, '.credentials.json'),
        join(selectedConfig, 'plugins', 'demo', 'hooks', 'hooks.json'),
        join(selectedConfig, 'output-styles', 'injected.md'),
        join(selectedConfig, 'workflows', 'injected.md'),
        join(selectedConfig, 'templates', 'injected.md'),
        join(selectedConfig, 'known_marketplaces.json'),
        join(physicalConfig, 'mcp', 'servers.json'),
        join(alternateAlias, 'plugins', 'demo', 'plugin.json'),
      ]

      for (const path of sensitiveTargets) {
        expect(await evaluateWrite(path), path).toMatchObject({
          kind: 'prompt',
          decision: {
            behavior: 'ask',
            decisionReason: { type: 'safetyCheck' },
          },
        })
      }

      expect(await evaluateEdit(sensitiveTargets[2]!)).toMatchObject({
        kind: 'prompt',
        decision: {
          behavior: 'ask',
          decisionReason: { type: 'safetyCheck' },
        },
      })
      expect(await evaluateEdit(sensitiveTargets.at(-1)!)).toMatchObject({
        kind: 'prompt',
        decision: {
          behavior: 'ask',
          decisionReason: { type: 'safetyCheck' },
        },
      })

      const tasksDir = join(selectedConfig, 'tasks')
      const ordinaryTask = join(tasksDir, 'task-output.txt')
      const escapedTask = join(tasksDir, 'escaped-output.txt')
      const externalSecret = join(tempRoot, 'outside-secret.txt')
      mkdirSync(tasksDir, { recursive: true })
      writeFileSync(ordinaryTask, 'ordinary')
      writeFileSync(externalSecret, 'secret')
      symlinkSync(externalSecret, escapedTask)
      expect(
        checkReadPermissionForTool(
          FileReadTool,
          { file_path: ordinaryTask },
          getEmptyToolPermissionContext(),
        ),
      ).toMatchObject({ behavior: 'allow' })
      expect(
        checkReadPermissionForTool(
          FileReadTool,
          { file_path: escapedTask },
          getEmptyToolPermissionContext(),
        ),
      ).toMatchObject({
        behavior: 'ask',
        decisionReason: { type: 'workingDir' },
      })
      if (process.platform !== 'win32') {
        const caseVariantSibling = join(
          tempRoot,
          'SELECTED-CONFIG',
          'tasks',
          'task-output.txt',
        )
        expect(
          checkReadPermissionForTool(
            FileReadTool,
            { file_path: caseVariantSibling },
            getEmptyToolPermissionContext(),
          ),
        ).toMatchObject({
          behavior: 'ask',
          decisionReason: { type: 'workingDir' },
        })
      }

      const ordinaryWorkspaceTarget = join(
        process.cwd(),
        'build',
        'ordinary-output.txt',
      )
      expect(await evaluateWrite(ordinaryWorkspaceTarget)).toEqual({
        kind: 'allow',
      })

      // The validator reports canonical paths but must retain the selected
      // lexical config-root form. These two internal producer namespaces are
      // otherwise mistaken for arbitrary config writes when that root is a
      // symlink.
      const agentMemoryPath = join(
        selectedConfig,
        'agent-memory',
        'reviewer',
        'MEMORY.md',
      )
      const autoMemoryPath = join(getAutoMemPath(), 'MEMORY.md')
      for (const memoryPath of [agentMemoryPath, autoMemoryPath]) {
        mkdirSync(dirname(memoryPath), { recursive: true })
        writeFileSync(memoryPath, 'memory')
        expect(await evaluateWrite(memoryPath), memoryPath).toEqual({
          kind: 'allow',
        })
        expect(await evaluateRead(memoryPath), memoryPath).toEqual({
          kind: 'allow',
        })

        const directContext = {
          ...getEmptyToolPermissionContext(),
          mode: 'bypassPermissions' as const,
          isBypassPermissionsModeAvailable: true,
        }
        expect(
          validatePath(memoryPath, tempRoot, directContext, 'create'),
          `shared write: ${memoryPath}`,
        ).toMatchObject({ allowed: true })
        expect(
          validatePath(memoryPath, tempRoot, directContext, 'read'),
          `shared read: ${memoryPath}`,
        ).toMatchObject({ allowed: true })

        for (const [name, operation] of [
          ['Set-Content', 'write'],
          ['Get-Content', 'read'],
        ] as const) {
          const command = `${name} '${memoryPath}'`
          const parsed = {
            valid: true,
            errors: [],
            statements: [
              {
                statementType: 'PipelineAst',
                commands: [
                  {
                    name,
                    nameType: 'cmdlet',
                    elementType: 'CommandAst',
                    args: [memoryPath],
                    text: command,
                    elementTypes: ['StringConstant', 'StringConstant'],
                  },
                ],
                redirections: [],
                text: command,
              },
            ],
            variables: [],
            hasStopParsing: false,
            originalCommand: command,
          } satisfies ParsedPowerShellCommand
          expect(
            checkPowerShellPathConstraints(
              { command },
              parsed,
              directContext,
            ),
            `PowerShell ${operation}: ${memoryPath}`,
          ).toMatchObject({ behavior: 'passthrough' })
        }
      }

      // The current session's exact plan file is a proven internal producer
      // and remains auto-editable even though plans live below config home.
      const planPath = getPlanFilePath()
      expect(await evaluateWrite(planPath)).toEqual({ kind: 'allow' })

      // A nested symlink must not turn that carve-out into an arbitrary write.
      const escapedPlanPath = join(
        getPlansDirectory(),
        'permission-floor-plan-agent-escape.md',
      )
      const externalTarget = join(tempRoot, 'outside-plan-target.md')
      mkdirSync(dirname(escapedPlanPath), { recursive: true })
      writeFileSync(externalTarget, 'outside')
      symlinkSync(externalTarget, escapedPlanPath)
      expect(await evaluateWrite(escapedPlanPath)).toMatchObject({
        kind: 'prompt',
        decision: {
          behavior: 'ask',
          decisionReason: { type: 'safetyCheck' },
        },
      })
      expect(await evaluateEdit(escapedPlanPath)).toMatchObject({
        kind: 'prompt',
        decision: {
          behavior: 'ask',
          decisionReason: { type: 'safetyCheck' },
        },
      })
    } finally {
      if (previousConfigDir === undefined) {
        delete process.env.CRABCODE_CONFIG_DIR
      } else {
        process.env.CRABCODE_CONFIG_DIR = previousConfigDir
      }
      getPlansDirectory.cache.clear?.()
      getAutoMemPath.cache.clear?.()
      rmSync(tempRoot, { recursive: true, force: true })
    }
  })
})
