import { describe, expect, test } from 'bun:test'

import {
  DIRECT_TUI_BUG_REPORT_ENDPOINT,
  DirectTuiBugReportUnconfirmedError,
  submitDirectTuiBugReport,
  type DirectTuiBugReportClient,
} from '../../src/cli/directTuiBugReportRuntimeAction.js'
import {
  CRABCODE_TUI_RUNTIME_ACTION_TYPE,
  CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
  routeDirectTuiRuntimeAction,
} from '../../src/cli/directTuiRuntimeActions.js'

describe('direct native TUI bug reporting', () => {
  test('the closed action returns a correlated feedback id', async () => {
    const routed = await routeDirectTuiRuntimeAction(
      {
        type: CRABCODE_TUI_RUNTIME_ACTION_TYPE,
        protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
        request_id: 'bug-1',
        action: {
          kind: 'bug_report_submit',
          description: 'terminal layout is broken',
        },
      },
      {
        bugReportDependencies: {
          submitBugReport: async description => {
            expect(description).toBe('terminal layout is broken')
            return { feedbackId: 'feedback-123' }
          },
        },
      },
    )

    expect(routed).toEqual({
      handled: true,
      response: {
        type: 'crabcode_tui_runtime_result',
        protocol_version: 1,
        request_id: 'bug-1',
        result: {
          kind: 'bug_report_submitted',
          feedback_id: 'feedback-123',
        },
      },
    })
  })

  test('the report body contains no transcript, request body, or logs', async () => {
    let observed:
      | { method: string; path: string; body: unknown | null }
      | undefined
    const client: DirectTuiBugReportClient = {
      async doJSON<T>(method: string, path: string, body: unknown | null) {
        observed = { method, path, body }
        return { feedback_id: 'feedback-minimal' } as T
      },
    }

    await expect(
      submitDirectTuiBugReport(client, 'only the user description', {
        platform: 'darwin',
        terminal: 'WezTerm\n\u001b]8;;bad',
        version: '1.2.3',
        datetime: '2026-08-10T01:02:03.000Z',
      }),
    ).resolves.toEqual({ feedbackId: 'feedback-minimal' })

    expect(observed).toEqual({
      method: 'POST',
      path: DIRECT_TUI_BUG_REPORT_ENDPOINT,
      body: {
        content: JSON.stringify({
          description: 'only the user description',
          platform: 'darwin',
          terminal: 'WezTerm ]8;;bad',
          version: '1.2.3',
          datetime: '2026-08-10T01:02:03.000Z',
        }),
      },
    })
    const report = JSON.parse(
      (observed?.body as { content: string }).content,
    ) as Record<string, unknown>
    expect(report).not.toHaveProperty('transcript')
    expect(report).not.toHaveProperty('errors')
    expect(report).not.toHaveProperty('request')
    expect(report).not.toHaveProperty('messages')
  })

  test('malformed descriptions and invalid response ids fail closed', async () => {
    for (const [requestId, description] of [
      ['bug-invalid-empty', ''],
      ['bug-invalid-bytes', '界'.repeat(3_000)],
    ]) {
      expect(
        await routeDirectTuiRuntimeAction({
          type: CRABCODE_TUI_RUNTIME_ACTION_TYPE,
          protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
          request_id: requestId,
          action: { kind: 'bug_report_submit', description },
        }),
      ).toEqual({
        handled: true,
        response: {
          type: 'crabcode_tui_runtime_result',
          protocol_version: 1,
          request_id: requestId,
          result: { kind: 'runtime_action_error', code: 'invalid_request' },
        },
      })
    }

    for (const unsafeId of [
      '\u001b]8;;unsafe',
      '\u009b31munsafe',
      'feedback-\u061cspoofed',
      'feedback-\u200espoofed',
      'feedback-\u200fspoofed',
      'feedback-\u202espoofed',
      'feedback-\u2066spoofed',
    ]) {
      const client: DirectTuiBugReportClient = {
        async doJSON<T>() {
          return { feedback_id: unsafeId } as T
        },
      }
      await expect(
        submitDirectTuiBugReport(client, 'valid description', {
          platform: 'darwin',
          terminal: 'terminal',
          version: '1.2.3',
          datetime: '2026-08-10T01:02:03.000Z',
        }),
      ).rejects.toThrow('bug-report-response-missing-feedback-id')
    }

    expect(
      await routeDirectTuiRuntimeAction(
        {
          type: CRABCODE_TUI_RUNTIME_ACTION_TYPE,
          protocol_version: CRABCODE_TUI_RUNTIME_PROTOCOL_VERSION,
          request_id: 'bug-unconfirmed',
          action: {
            kind: 'bug_report_submit',
            description: 'valid description',
          },
        },
        {
          bugReportDependencies: {
            submitBugReport: async () => {
              throw new DirectTuiBugReportUnconfirmedError()
            },
          },
        },
      ),
    ).toMatchObject({
      handled: true,
      response: {
        request_id: 'bug-unconfirmed',
        result: { kind: 'bug_report_unconfirmed' },
      },
    })
  })
})
