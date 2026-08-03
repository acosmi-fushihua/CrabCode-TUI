#!/usr/bin/env bun

import { rmSync } from 'node:fs'
import { mkdtemp, stat } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const root = resolve(
  process.env.CRABCODE_SMOKE_PACKAGE_ROOT ?? resolve(import.meta.dir, '..'),
)
const runtime = join(root, 'dist/tui-runtime/index.js')
const runtimeExecutable = resolve(
  process.env.CRABCODE_SMOKE_BUN ?? process.execPath,
)
const timeoutMs = 45_000
const rendererNotificationChannels = new Set([
  'auto',
  'iterm2',
  'iterm2_with_bell',
  'terminal_bell',
  'kitty',
  'ghostty',
  'notifications_disabled',
])
const rendererThemeSettings = new Set([
  'auto',
  'dark',
  'light',
  'light-daltonized',
  'dark-daltonized',
  'light-ansi',
  'dark-ansi',
])

await stat(runtime)
const configDir = await mkdtemp(join(tmpdir(), 'crabcode-tui-smoke-'))
const child = Bun.spawn({
  cmd: [
    runtimeExecutable,
    runtime,
    '--bare',
    '--no-session-persistence',
  ],
  cwd: root,
  env: {
    ...process.env,
    CRABCODE_CONFIG_DIR: configDir,
    CRABCODE_SIMPLE: '1',
    CRABCODE_DISABLE_AUTO_MEMORY: '1',
    DISABLE_AUTO_COMPACT: '1',
    // This smoke owns no interactive renderer. Skip first-run onboarding via
    // its established demo authority so the fixture can exercise the runtime
    // lifecycle without synthesizing user choices.
    IS_DEMO: '1',
  },
  stdin: 'pipe',
  stdout: 'pipe',
  stderr: 'pipe',
})

const cleanup = () => {
  try {
    child.kill()
  } catch {
    // The child has already exited.
  }
  rmSync(configDir, { recursive: true, force: true })
}
process.once('exit', cleanup)
process.once('SIGINT', () => process.exit(130))
process.once('SIGTERM', () => process.exit(143))

const initializeId = 'tui-runtime-smoke-initialize'
const endId = 'tui-runtime-smoke-end'
const writeFrame = async value => {
  child.stdin.write(`${JSON.stringify(value)}\n`)
  await child.stdin.flush()
}

const writeReverseControlSuccess = async (frame, response) => {
  await writeFrame({
    type: 'control_response',
    response: {
      subtype: 'success',
      request_id: frame.request_id,
      response,
    },
  })
}

const writeInitialize = async () => {
  await writeFrame({
    type: 'control_request',
    request_id: initializeId,
    request: {
      subtype: 'initialize',
      promptSuggestions: true,
      agentProgressSummaries: true,
    },
  })
}

const decoder = new TextDecoder()
const stdout = child.stdout.getReader()
const stderrPromise = new Response(child.stderr).text()
const deadline = Date.now() + timeoutMs
const frameTypes = []
let buffer = ''
let initializePayload
const turnResults = []
let endAcknowledged = false
let rendererContextAcknowledged = false
let turnSubmitted = false

// Rust emits the existing SDK initialize exactly once as soon as its writer is
// available. The renderer-session router must stash this exact line until the
// process-private setup exchange has completed; no second ready subtype exists.
await writeInitialize()

while (Date.now() < deadline && !endAcknowledged) {
  const read = await Promise.race([
    stdout.read(),
    new Promise(resolveRead =>
      setTimeout(() => resolveRead({ timeout: true }), 1_000),
    ),
  ])
  if (read.timeout) continue
  if (read.done) break

  buffer += decoder.decode(read.value, { stream: true })
  let newline
  while ((newline = buffer.indexOf('\n')) !== -1) {
    const line = buffer.slice(0, newline)
    buffer = buffer.slice(newline + 1)
    if (!line.trim()) continue

    const frame = JSON.parse(line)
    const frameSubtype = frame.subtype ?? frame.request?.subtype
    frameTypes.push(
      `${String(frame.type)}${frameSubtype ? `:${String(frameSubtype)}` : ''}`,
    )

    if (
      frame.type === 'control_request' &&
      frame.request?.subtype === 'crabcode_tui_setup' &&
      frame.request?.kind === 'renderer_context'
    ) {
      const request = frame.request
      if (
        rendererContextAcknowledged ||
        request.protocol_version !== 1 ||
        request.kind !== 'renderer_context' ||
        typeof request.cwd !== 'string' ||
        request.cwd.length === 0 ||
        typeof request.config_verbose !== 'boolean' ||
        !rendererNotificationChannels.has(
          request.preferred_notification_channel,
        ) ||
        !Number.isSafeInteger(
          request.message_idle_notification_threshold_ms,
        ) ||
        request.message_idle_notification_threshold_ms < 0 ||
        !new Set(['zh-CN', 'en-US']).has(request.ui_language) ||
        !rendererThemeSettings.has(request.theme_setting) ||
        typeof request.syntax_highlighting_disabled !== 'boolean' ||
        Object.keys(request).sort().join(',') !==
          'config_verbose,cwd,kind,message_idle_notification_threshold_ms,preferred_notification_channel,protocol_version,subtype,syntax_highlighting_disabled,theme_setting,ui_language'
      ) {
        throw new Error(
          `renderer-context request has an invalid shape: ${line}`,
        )
      }
      await writeReverseControlSuccess(frame, {
        protocol_version: 1,
        kind: 'renderer_context',
        decision: 'received',
      })
      rendererContextAcknowledged = true
      continue
    }

    if (
      frame.type === 'control_response' &&
      frame.response?.request_id === initializeId
    ) {
      if (frame.response.subtype !== 'success') {
        throw new Error(`initialize rejected: ${line}`)
      }
      if (!rendererContextAcknowledged || initializePayload) {
        throw new Error(
          `initialize response arrived outside the single setup barrier: ${line}`,
        )
      }
      initializePayload = frame.response.response
      await writeFrame({
        type: 'user',
        message: { role: 'user', content: '/cost' },
        parent_tool_use_id: null,
      })
      turnSubmitted = true
      continue
    }

    if (frame.type === 'control_request') {
      throw new Error(
        `runtime emitted an unhandled reverse control request: ${line}`,
      )
    }

    if (frame.type === 'result') {
      if (frame.subtype !== 'success') {
        throw new Error(`fixture turn failed: ${line}`)
      }
      turnResults.push(frame)
      if (turnResults.length === 1) {
        await writeFrame({
          type: 'user',
          message: { role: 'user', content: '/cost' },
          parent_tool_use_id: null,
        })
      } else if (turnResults.length === 2) {
        await writeFrame({
          type: 'control_request',
          request_id: endId,
          request: {
            subtype: 'end_session',
            reason: 'tui-runtime-smoke-complete',
          },
        })
      } else {
        throw new Error(`runtime emitted an unexpected third result: ${line}`)
      }
      continue
    }

    if (
      frame.type === 'control_response' &&
      frame.response?.request_id === endId
    ) {
      if (frame.response.subtype !== 'success') {
        throw new Error(`end_session rejected: ${line}`)
      }
      endAcknowledged = true
      child.stdin.end()
      break
    }
  }
}

if (
  !rendererContextAcknowledged ||
  !initializePayload ||
  !turnSubmitted ||
  turnResults.length !== 2 ||
  !endAcknowledged
) {
  child.kill()
  const [exitAfterKill, stderr] = await Promise.all([
    Promise.race([
      child.exited,
      new Promise(resolveExit =>
        setTimeout(() => resolveExit('timeout-after-kill'), 5_000),
      ),
    ]),
    Promise.race([
      stderrPromise,
      new Promise(resolveStderr =>
        setTimeout(() => resolveStderr('<stderr-timeout-after-kill>'), 5_000),
      ),
    ]),
  ])
  throw new Error(
    `runtime smoke timed out: ${JSON.stringify({
      rendererContextAcknowledged,
      initialized: Boolean(initializePayload),
      turnSubmitted,
      turnsCompleted: turnResults.length,
      endAcknowledged,
      frameTypes,
      exitAfterKill,
      stderr,
    })}`,
  )
}

const exitCode = await Promise.race([
  child.exited,
  new Promise(resolveExit => setTimeout(() => resolveExit('timeout'), 10_000)),
])
if (exitCode === 'timeout') {
  child.kill()
  throw new Error('runtime did not exit after end_session')
}
if (exitCode !== 0) {
  throw new Error(`runtime exited with code ${exitCode}`)
}

const stderr = await stderrPromise
if (stderr.length > 0) {
  throw new Error(`runtime emitted stderr during smoke:\n${stderr}`)
}

process.stdout.write(
  `${JSON.stringify(
    {
      rendererContext: 'received',
      workspaceTrust: 'skipped-by-established-demo-authority',
      initialize: 'success',
      turns: '2/2 success',
      endSession: 'success',
      commands: Array.isArray(initializePayload.commands)
        ? initializePayload.commands.length
        : null,
      agents: Array.isArray(initializePayload.agents)
        ? initializePayload.agents.length
        : null,
      models: Array.isArray(initializePayload.models)
        ? initializePayload.models.length
        : null,
      outputStyle: initializePayload.output_style,
      frameTypes,
      exitCode,
    },
    null,
    2,
  )}\n`,
)
