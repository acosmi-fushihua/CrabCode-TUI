import { describe, expect, test } from 'bun:test'

import type { SettingsJson } from '../../src/utils/settings/types.js'
import {
  extractDangerousSettings,
  formatDangerousSettingsList,
  hasDangerousSettings,
  hasDangerousSettingsChanged,
} from '../../src/services/remoteManagedSettings/securityReviewCore.js'

function settings(value: unknown): SettingsJson {
  return value as SettingsJson
}

describe('direct native TUI managed-settings security boundary', () => {
  test('classifies executable shell, sensitive/unknown environment and hooks', () => {
    const dangerous = extractDangerousSettings(
      settings({
        apiKeyHelper: '/trusted/helper',
        env: {
          BASH_DEFAULT_TIMEOUT_MS: '30000',
          ACOSMI_API_KEY: 'secret',
          FUTURE_UNCLASSIFIED_SETTING: 'review-me',
          DISABLE_TELEMETRY: '1',
        },
        hooks: {
          PreToolUse: [
            {
              hooks: [{ type: 'command', command: '/trusted/hook' }],
            },
          ],
        },
      }),
    )

    expect(dangerous.shellSettings).toEqual({
      apiKeyHelper: '/trusted/helper',
    })
    expect(dangerous.envVars).toEqual({
      ACOSMI_API_KEY: 'secret',
      FUTURE_UNCLASSIFIED_SETTING: 'review-me',
    })
    expect(dangerous.hasHooks).toBe(true)
    expect(formatDangerousSettingsList(dangerous)).toEqual([
      'apiKeyHelper',
      'ACOSMI_API_KEY',
      'FUTURE_UNCLASSIFIED_SETTING',
      'hooks',
    ])
    expect(hasDangerousSettings(dangerous)).toBe(true)
  })

  test('known ordinary controls do not create a false executable review', () => {
    const dangerous = extractDangerousSettings(
      settings({
        env: {
          BASH_DEFAULT_TIMEOUT_MS: '30000',
          DISABLE_TELEMETRY: '1',
        },
      }),
    )

    expect(dangerous).toEqual({
      shellSettings: {},
      envVars: {},
      hasHooks: false,
      hooks: undefined,
    })
    expect(hasDangerousSettings(dangerous)).toBe(false)
    expect(hasDangerousSettingsChanged(null, settings({}))).toBe(false)
  })

  test('unchanged cached danger is stable while every changed danger blocks', () => {
    const cached = settings({
      apiKeyHelper: '/trusted/helper-v1',
      env: { ACOSMI_API_KEY: 'secret-v1' },
    })
    expect(hasDangerousSettingsChanged(cached, cached)).toBe(false)
    expect(
      hasDangerousSettingsChanged(cached, settings({
        ...cached,
        apiKeyHelper: '/trusted/helper-v2',
      })),
    ).toBe(true)
    expect(
      hasDangerousSettingsChanged(cached, settings({
        ...cached,
        env: { ACOSMI_API_KEY: 'secret-v2' },
      })),
    ).toBe(true)
    expect(
      hasDangerousSettingsChanged(cached, settings({
        ...cached,
        hooks: { SessionStart: [] },
      })),
    ).toBe(true)
    expect(
      hasDangerousSettingsChanged(cached, settings({
        ...cached,
        hooks: {
          SessionStart: [
            {
              hooks: [{ type: 'command', command: '/trusted/hook' }],
            },
          ],
        },
      })),
    ).toBe(true)
  })
})
