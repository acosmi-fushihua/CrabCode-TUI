import { describe, expect, test } from 'bun:test'

import {
  SDKControlInitializeRequestSchema,
  SDKControlPermissionRequestSchema,
  SDK_PERMISSION_DECISION_REASON_CODES,
} from '../../src/entrypoints/sdk/controlSchemas.js'
import {
  buildCanUseToolControlRequest,
  isPermissionRequestHookAllowAuthoritative,
  permissionDecisionReasonCode,
  StructuredIO,
} from '../../src/cli/structuredIO.js'
import { negotiateQuestionPreviewFormat } from '../../src/cli/print/sdkControlHandlers.js'
import { getDefaultAppState } from '../../src/state/AppStateStore.js'

async function* noInput(): AsyncGenerator<string> {}

describe('native TUI permission protocol', () => {
  test('keeps question negotiation minimal and forward-compatible', () => {
    expect(() =>
      SDKControlInitializeRequestSchema().parse({
        subtype: 'initialize',
        askUserQuestion: {
          version: 3,
          previewFormats: ['markdown'],
        },
      }),
    ).not.toThrow()
    const parsed = SDKControlInitializeRequestSchema().parse({
        subtype: 'initialize',
        askUserQuestion: {
          version: 2,
          supportsOther: false,
        },
    })
    expect(parsed.askUserQuestion).toEqual({ version: 2 })
    expect(negotiateQuestionPreviewFormat(undefined)).toBeUndefined()
    expect(
      negotiateQuestionPreviewFormat({
        version: 1,
        previewFormats: ['html'],
      }),
    ).toBeUndefined()
    expect(
      negotiateQuestionPreviewFormat({
        version: 2,
        previewFormats: ['html'],
      }),
    ).toBe('html')
    expect(
      negotiateQuestionPreviewFormat({
        version: 3,
        previewFormats: ['html', 'markdown'],
      }),
    ).toBe('markdown')
  })

  test('defines a closed, stable reason-code vocabulary', () => {
    expect(SDK_PERMISSION_DECISION_REASON_CODES).toEqual([
      'rule',
      'mode',
      'subcommand_results',
      'permission_prompt_tool',
      'hook',
      'async_agent',
      'classifier',
      'working_directory',
      'safety_check',
      'other',
    ])
    expect(
      permissionDecisionReasonCode({
        type: 'safetyCheck',
        reason: 'protected configuration',
        classifierApprovable: true,
      }),
    ).toBe('safety_check')
  })

  test('requires a correlated human host for question answers', () => {
    expect(
      isPermissionRequestHookAllowAuthoritative('AskUserQuestion'),
    ).toBe(false)
    expect(isPermissionRequestHookAllowAuthoritative('Bash')).toBe(true)
  })

  test('can_use_tool carries a stable reason code and renderer-visible message', () => {
    const request = buildCanUseToolControlRequest(
      { name: 'FileEdit' },
      { file_path: '/tmp/.crabcode/settings.json' },
      {
        behavior: 'ask',
        message: 'protected configuration',
        blockedPath: '/tmp/.crabcode/settings.json',
        decisionReason: {
          type: 'safetyCheck',
          reason: 'Editing CrabCode settings requires explicit approval.',
          classifierApprovable: true,
        },
      },
      'tool-use-1',
      'agent-1',
    )

    expect(request).toMatchObject({
      subtype: 'can_use_tool',
      tool_name: 'FileEdit',
      tool_use_id: 'tool-use-1',
      agent_id: 'agent-1',
      decision_reason_code: 'safety_check',
      decision_reason: 'Editing CrabCode settings requires explicit approval.',
    })
    expect(request.description).toContain('protected configuration')
    expect(request.description).toContain(
      'Editing CrabCode settings requires explicit approval.',
    )
    expect(() =>
      SDKControlPermissionRequestSchema().parse(request),
    ).not.toThrow()
  })

  test('rule reasons remain displayable even though the legacy free-form field is absent', () => {
    const request = buildCanUseToolControlRequest(
      { name: 'Bash' },
      { command: 'true' },
      {
        behavior: 'ask',
        message: 'rule requires approval',
        decisionReason: {
          type: 'rule',
          rule: {
            source: 'userSettings',
            ruleBehavior: 'ask',
            ruleValue: { toolName: 'Bash' },
          },
        },
      },
      'tool-use-2',
    )

    expect(request.decision_reason_code).toBe('rule')
    expect(request.decision_reason).toBeUndefined()
    expect(request.description).toContain('requires approval')
    expect(request.description).toContain('Bash')
  })

  test('registers correlation before a bridge can answer synchronously', async () => {
    const io = new StructuredIO(noInput())
    const cancellations: unknown[] = []
    io.write = async message => {
      cancellations.push(message)
    }
    io.setOnControlRequestSent(request => {
      io.injectControlResponse({
        type: 'control_response',
        response: {
          subtype: 'success',
          request_id: request.request_id,
          response: {
            behavior: 'allow',
            updatedInput: { approved: true },
            toolUseID: 'tool-use-sync',
          },
        },
      })
    })

    const result = await io.requestDirectPermission({
      subtype: 'can_use_tool',
      tool_name: 'Bash',
      input: { command: 'true' },
      tool_use_id: 'tool-use-sync',
    })

    expect(result).toMatchObject({
      behavior: 'allow',
      updatedInput: { approved: true },
    })
    expect(io.getPendingPermissionRequests()).toEqual([])
    expect((await io.outbound.next()).value).toMatchObject({
      type: 'control_request',
      request: { subtype: 'can_use_tool', tool_use_id: 'tool-use-sync' },
    })
    expect(cancellations).toEqual([
      expect.objectContaining({ type: 'control_cancel_request' }),
    ])
  })

  test('ignores a late duplicate by request id even without a tool-use id', async () => {
    let releaseLateLine: ((line: string) => void) | undefined
    const lateLine = new Promise<string>(resolve => {
      releaseLateLine = resolve
    })
    async function* delayedInput(): AsyncGenerator<string> {
      yield await lateLine
    }

    const io = new StructuredIO(delayedInput())
    io.write = async () => {}
    const inputCompletion = io.structuredInput.next()
    let resolvedRequestId = ''
    io.setOnControlRequestSent(request => {
      resolvedRequestId = request.request_id
      io.injectControlResponse({
        type: 'control_response',
        response: {
          subtype: 'success',
          request_id: request.request_id,
          response: {
            behavior: 'allow',
            updatedInput: { approved: true },
          },
        },
      })
    })

    await io.requestDirectPermission({
      subtype: 'can_use_tool',
      tool_name: 'Bash',
      input: { command: 'true' },
      tool_use_id: 'tool-use-duplicate',
    })
    let unexpectedResponseCount = 0
    io.setUnexpectedResponseCallback(async () => {
      unexpectedResponseCount += 1
    })
    releaseLateLine?.(
      JSON.stringify({
        type: 'control_response',
        response: {
          subtype: 'error',
          request_id: resolvedRequestId,
          error: 'late duplicate without toolUseID',
        },
      }),
    )

    expect(await inputCompletion).toEqual({ done: true, value: undefined })
    expect(unexpectedResponseCount).toBe(0)
  })

  test('a pre-aborted request is never published or left pending', async () => {
    const io = new StructuredIO(noInput())
    const controller = new AbortController()
    controller.abort()
    let promptCount = 0

    const decision = await io.createCanUseTool(() => {
      promptCount += 1
    })(
      { name: 'Bash' } as never,
      { command: 'true' },
      { abortController: controller } as never,
      {} as never,
      'tool-use-pre-aborted',
      { behavior: 'ask', message: 'needs approval' },
    )
    expect(decision).toMatchObject({
      behavior: 'deny',
      message: expect.stringContaining('cancelled before'),
    })
    expect(promptCount).toBe(0)

    expect(
      await io.handleElicitation(
        'server',
        'private prompt',
        undefined,
        controller.signal,
      ),
    ).toEqual({ action: 'cancel' })
    expect(io.getPendingPermissionRequests()).toEqual([])

    const outboundState = await Promise.race([
      io.outbound.next().then(() => 'published' as const),
      new Promise<'empty'>(resolve => setTimeout(() => resolve('empty'), 10)),
    ])
    expect(outboundState).toBe('empty')
  })

  test('does not commit a host winner after the owning turn is aborted', async () => {
    const io = new StructuredIO(noInput())
    const controller = new AbortController()
    let appState = getDefaultAppState()
    let appStateUpdateCount = 0
    io.write = async () => {}
    io.setOnControlRequestSent(request => {
      io.injectControlResponse({
        type: 'control_response',
        response: {
          subtype: 'success',
          request_id: request.request_id,
          response: {
            behavior: 'allow',
            updatedInput: { command: 'echo changed' },
            updatedPermissions: [
              {
                type: 'setMode',
                mode: 'acceptEdits',
                destination: 'session',
              },
            ],
            toolUseID: 'tool-use-stale',
          },
        },
      })
      controller.abort()
    })

    const decision = await io.createCanUseTool()(
      { name: 'Bash', userFacingName: () => 'Bash' } as never,
      { command: 'true' },
      {
        abortController: controller,
        getAppState: () => appState,
        setAppState: update => {
          appStateUpdateCount += 1
          appState = update(appState)
        },
        options: { tools: [] },
      } as never,
      {} as never,
      'tool-use-stale',
      { behavior: 'ask', message: 'needs approval' },
    )

    expect(decision).toMatchObject({
      behavior: 'deny',
      message: expect.stringContaining('cancelled before'),
    })
    expect(appStateUpdateCount).toBe(0)
    expect(appState.toolPermissionContext.mode).toBe('default')
  })
})
