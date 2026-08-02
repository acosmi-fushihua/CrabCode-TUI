import { describe, expect, test } from 'bun:test'
import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import {
  applyCustomModelRegistryEdit,
  type CustomModelRegistryEdit,
} from '../../src/services/customModel/registryAuthority.js'

const ROOT = resolve(import.meta.dir, '../..')

function source(path: string): string {
  return readFileSync(resolve(ROOT, path), 'utf8')
}

describe('pure TUI backend authority boundaries', () => {
  test('direct runtime installs the local chat lifecycle from the in-process authority', () => {
    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    expect(bootstrap).toContain(
      'installLocalModelServerStatusProvider(',
    )
    expect(bootstrap).toContain(
      'createDefaultLocalModelDirectClient().serverStatus()',
    )
    expect(bootstrap).not.toContain(
      "appServer/installLocalModelChatStream",
    )
  })

  test('logout preserves the local-model shutdown safety boundary without AppServer', () => {
    const logout = source('src/services/auth/logout.ts')
    const sdkLogout = logout.indexOf('await acosmiLogout()')
    const stopLocalModel = logout.indexOf(
      'await stopLocalModelServerOnLogoutDirect()',
    )
    const clearAuth = logout.indexOf(
      'await clearLocalAuthState({ clearOnboarding })',
    )

    expect(sdkLogout).toBeGreaterThanOrEqual(0)
    expect(stopLocalModel).toBeGreaterThan(sdkLogout)
    expect(clearAuth).toBeGreaterThan(stopLocalModel)
    expect(logout).not.toContain('stopLocalModelServerOnLogout()')
    expect(logout).not.toMatch(
      /from ['"][^'"]*appServer|AppServerJsonRpcClient|ensureTuiAppServerDaemon/,
    )
  })

  test('direct custom-model validation preserves the backend INVALID_PARAMS error contract', async () => {
    const edit = {
      type: 'addCustomModel',
      brand: 'custom',
      protocol: 'openai-compatible',
      baseUrl: 'not a URL',
      modelId: 'model',
      contextWindow: 4096,
      maxOutputTokens: 1024,
      enabled: true,
    } satisfies CustomModelRegistryEdit

    await expect(
      applyCustomModelRegistryEdit(
        edit,
        {
          updateSettingsForSource() {
            throw new Error('validation must run before settings mutation')
          },
        },
        async () => {
          throw new Error('validation must run before authority loading')
        },
      ),
    ).rejects.toMatchObject({
      name: 'WorkerError',
      code: -32602,
      message: 'addCustomModel.baseUrl must be a valid URL',
    })
  })

  test('cron execution targets only the existing direct TUI route', () => {
    expect(existsSync(resolve(ROOT, 'src/utils/cronRuntimeTarget.ts'))).toBe(
      false,
    )

    const createTool = source(
      'src/tools/ScheduleCronTool/CronCreateTool.ts',
    )
    const tasks = source('src/utils/cronTasks.ts')

    for (const [path, text] of [
      ['src/tools/ScheduleCronTool/CronCreateTool.ts', createTool],
      ['src/utils/cronTasks.ts', tasks],
    ] as const) {
      expect(text, path).not.toMatch(
        /cronRuntimeTarget|supportsImmediateWake|supportsTeammateContinuation|hosted (?:session|worker|teammate)|['"]gui['"]/,
      )
    }

    expect(tasks).toContain(
      'const connectionId = `tui:${process.pid}:${String(getSessionId())}`',
    )
    expect(tasks).toContain("const surface = 'tui' as const")
  })

})
