#!/usr/bin/env bun

import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync, rmSync } from 'node:fs'
import { mkdtemp, stat } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { createPersistentStreamPoller } from './persistent-stream-poller.mjs'
import { classifyRuntimeStderr } from './release-runtime-stderr.mjs'
import {
  verifyTuiRuntimeArtifactBinding,
  verifyTuiRuntimeBuildBinding,
} from './tui-runtime-source-binding.mjs'
import {
  assertCommandCatalogChangedRequest,
  commandCatalogChangedAck,
  commandCatalogChangedSubtype,
  expectedTuiRuntimeIdentity,
} from './tui-runtime-smoke-contract.mjs'

const root = resolve(
  process.env.CRABCODE_SMOKE_PACKAGE_ROOT ?? resolve(import.meta.dir, '..'),
)
const runtime = join(root, 'dist/tui-runtime/index.js')
const runtimeMetafile = join(root, 'dist/tui-runtime/metafile.json')
const runtimeExecutable = resolve(
  process.env.CRABCODE_SMOKE_BUN ?? process.execPath,
)
const releaseMaterialsPath = join(root, 'release-materials.json')
const releaseMaterials = existsSync(releaseMaterialsPath)
  ? JSON.parse(readFileSync(releaseMaterialsPath, 'utf8'))
  : null
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
const runtimeMetafilePayload = JSON.parse(
  readFileSync(runtimeMetafile, 'utf8'),
)
verifyTuiRuntimeBuildBinding(
  runtimeMetafilePayload,
  releaseMaterials ? expectedTuiRuntimeIdentity(releaseMaterials) : {},
)
verifyTuiRuntimeArtifactBinding(runtime, runtimeMetafilePayload)
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

const captureDarwinSample = () => {
  if (
    process.platform !== 'darwin' ||
    process.env.CRABCODE_SMOKE_CAPTURE_DARWIN_SAMPLE !== '1' ||
    !Number.isSafeInteger(child.pid)
  ) {
    return null
  }
  const sample = spawnSync('/usr/bin/sample', [String(child.pid), '2', '1'], {
    encoding: 'utf8',
    timeout: 10_000,
    maxBuffer: 4 * 1024 * 1024,
  })
  const output = `${sample.stdout ?? ''}${sample.stderr ?? ''}`
  return {
    status: sample.status,
    signal: sample.signal,
    error: sample.error?.message ?? null,
    output: output.slice(0, 64 * 1024),
    truncated: output.length > 64 * 1024,
  }
}

process.once('exit', cleanup)
process.once('SIGINT', () => process.exit(130))
process.once('SIGTERM', () => process.exit(143))

const initializeId = 'tui-runtime-smoke-initialize'
const contextUsageId = 'tui-runtime-smoke-context-usage'
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
const pollStdout = createPersistentStreamPoller(stdout)
const deadline = Date.now() + timeoutMs
const frameTypes = []
let buffer = ''
let initializePayload
let contextUsagePayload
let compactFailureResult
let compactFailureRendered = false
let compactProgressFrames = 0
const turnResults = []
let endAcknowledged = false
let rendererContextAcknowledged = false
let compactSubmitted = false
let costTurnsSubmitted = 0
let commandCatalogRefreshesAcknowledged = 0
let latestCommandCatalogSize = null

const submitUserText = async content => {
  await writeFrame({
    type: 'user',
    message: { role: 'user', content },
    parent_tool_use_id: null,
  })
}

const assertContextUsagePayload = payload => {
  if (
    !payload ||
    typeof payload !== 'object' ||
    !Number.isFinite(payload.totalTokens) ||
    payload.totalTokens < 0 ||
    !Number.isFinite(payload.rawMaxTokens) ||
    payload.rawMaxTokens <= 0 ||
    !Number.isFinite(payload.maxTokens) ||
    payload.maxTokens <= 0 ||
    !Number.isFinite(payload.percentage) ||
    payload.percentage < 0 ||
    !Array.isArray(payload.categories) ||
    payload.categories.some(
      category =>
        !category ||
        typeof category !== 'object' ||
        typeof category.name !== 'string' ||
        !Number.isFinite(category.tokens) ||
        category.tokens < 0 ||
        typeof category.color !== 'string' ||
        (category.isDeferred !== undefined &&
          typeof category.isDeferred !== 'boolean'),
    ) ||
    !Array.isArray(payload.gridRows) ||
    payload.gridRows.some(row => !Array.isArray(row)) ||
    typeof payload.model !== 'string' ||
    !Array.isArray(payload.memoryFiles) ||
    !Array.isArray(payload.mcpTools) ||
    !Array.isArray(payload.agents) ||
    typeof payload.isAutoCompactEnabled !== 'boolean' ||
    !Object.hasOwn(payload, 'apiUsage')
  ) {
    throw new Error(
      `get_context_usage returned an invalid payload: ${JSON.stringify(payload)}`,
    )
  }
}

// Rust emits the existing SDK initialize exactly once as soon as its writer is
// available. The renderer-session router must stash this exact line until the
// process-private setup exchange has completed; no second ready subtype exists.
await writeInitialize()

while (Date.now() < deadline && !endAcknowledged) {
  const read = await pollStdout(1_000)
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
        type: 'control_request',
        request_id: contextUsageId,
        request: { subtype: 'get_context_usage' },
      })
      continue
    }

    if (
      frame.type === 'control_response' &&
      frame.response?.request_id === contextUsageId
    ) {
      if (frame.response.subtype !== 'success') {
        throw new Error(`get_context_usage rejected: ${line}`)
      }
      if (contextUsagePayload || compactSubmitted) {
        throw new Error(`duplicate get_context_usage response: ${line}`)
      }
      assertContextUsagePayload(frame.response.response)
      contextUsagePayload = frame.response.response
      await submitUserText('/compact')
      compactSubmitted = true
      continue
    }

    if (
      frame.type === 'control_request' &&
      frame.request?.subtype === commandCatalogChangedSubtype
    ) {
      if (!initializePayload) {
        throw new Error(
          `command-catalog refresh arrived before initialize completed: ${line}`,
        )
      }
      latestCommandCatalogSize = assertCommandCatalogChangedRequest(frame)
      await writeReverseControlSuccess(frame, commandCatalogChangedAck())
      commandCatalogRefreshesAcknowledged += 1
      continue
    }

    if (frame.type === 'control_request') {
      throw new Error(
        `runtime emitted an unhandled reverse control request: ${line}`,
      )
    }

    if (
      frame.type === 'system' &&
      frame.subtype === 'local_command' &&
      typeof frame.content === 'string' &&
      frame.content.includes('<local-command-stderr>')
    ) {
      compactFailureRendered = true
    }

    if (
      frame.type === 'progress' &&
      frame.data?.type === 'compact_progress'
    ) {
      compactProgressFrames += 1
    }

    if (frame.type === 'result') {
      if (!compactFailureResult) {
        if (
          frame.subtype !== 'error_during_execution' ||
          frame.is_error !== true ||
          frame.stop_reason !== null ||
          !Array.isArray(frame.errors) ||
          !frame.errors.some(error =>
            ['没有可压缩的消息', 'No messages to compact'].some(expected =>
              String(error).includes(expected),
            ),
          ) ||
          !compactFailureRendered
        ) {
          throw new Error(
            `empty-history /compact did not preserve its failure lifecycle: ${line}`,
          )
        }
        compactFailureResult = frame
        await submitUserText('/cost')
        costTurnsSubmitted += 1
        continue
      }
      if (frame.subtype !== 'success' || frame.is_error !== false) {
        throw new Error(`fixture /cost turn failed: ${line}`)
      }
      turnResults.push(frame)
      if (turnResults.length === 1) {
        await submitUserText('/cost')
        costTurnsSubmitted += 1
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
  !contextUsagePayload ||
  !compactSubmitted ||
  !compactFailureResult ||
  !compactFailureRendered ||
  compactProgressFrames !== 0 ||
  costTurnsSubmitted !== 2 ||
  turnResults.length !== 2 ||
  !endAcknowledged
) {
  const darwinSample = captureDarwinSample()
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
      contextUsage: Boolean(contextUsagePayload),
      compactSubmitted,
      compactFailure: Boolean(compactFailureResult),
      compactFailureRendered,
      compactProgressFrames,
      costTurnsSubmitted,
      costTurnsCompleted: turnResults.length,
      endAcknowledged,
      frameTypes,
      exitAfterKill,
      stderr,
      darwinSample,
    })}`,
  )
}

const exitCode = await Promise.race([
  child.exited,
  new Promise(resolveExit => setTimeout(() => resolveExit('timeout'), 10_000)),
])
if (exitCode === 'timeout') {
  const darwinSample = captureDarwinSample()
  child.kill()
  throw new Error(
    `runtime did not exit after end_session: ${JSON.stringify({ darwinSample })}`,
  )
}
if (exitCode !== 0) {
  throw new Error(`runtime exited with code ${exitCode}`)
}

const stderr = await stderrPromise
const stderrDisposition = classifyRuntimeStderr(stderr, releaseMaterials)
if (stderrDisposition === 'unexpected') {
  throw new Error(`runtime emitted stderr during smoke:\n${stderr}`)
}

process.stdout.write(
  `${JSON.stringify(
    {
      rendererContext: 'received',
      workspaceTrust: 'skipped-by-established-demo-authority',
      initialize: 'success',
      contextUsage: 'renderer-owned /context control: typed success',
      compactEmptyHistory: 'rendered error + terminal failure',
      compactProgressFrames,
      costTurns: '2/2 success',
      endSession: 'success',
      commands: Array.isArray(initializePayload.commands)
        ? initializePayload.commands.length
        : null,
      commandCatalogRefreshesAcknowledged,
      latestCommandCatalogSize,
      agents: Array.isArray(initializePayload.agents)
        ? initializePayload.agents.length
        : null,
      models: Array.isArray(initializePayload.models)
        ? initializePayload.models.length
        : null,
      outputStyle: initializePayload.output_style,
      stderr: stderrDisposition,
      frameTypes,
      exitCode,
    },
    null,
    2,
  )}\n`,
)
