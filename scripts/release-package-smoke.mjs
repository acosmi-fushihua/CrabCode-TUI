#!/usr/bin/env bun

import { createHash } from 'node:crypto'
import {
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { createConnection } from 'node:net'
import { tmpdir } from 'node:os'
import { basename, dirname, isAbsolute, join, relative, resolve, sep, win32 } from 'node:path'
import { spawnSync } from 'node:child_process'
import { unzipSync } from 'fflate'
import { comparePortablePaths } from './release-path-order.mjs'
import { assertTuiRuntimeSmokeSuccess } from './tui-runtime-smoke-contract.mjs'

const repositoryRoot = resolve(import.meta.dir, '..')
const incidentSchema = 'crabcode-memory-ipc-v1-20260725'
const packageProcessNames = new Set([
  'crabcode',
  'crabcode.exe',
  'crabcode-tui',
  'crabcode-tui.exe',
  'acosmi-memory-orchestrator',
  'acosmi-memory-orchestrator.exe',
  'bun',
  'bun.exe',
])

function fail(message) {
  throw new Error(`release package smoke: ${message}`)
}

function spawnText(value) {
  return value == null ? '' : String(value)
}

function spawnFailure(result) {
  return [result.error?.message, spawnText(result.stderr).trim(), result.signal ? `signal=${result.signal}` : '']
    .filter(Boolean)
    .join('; ') || `status=${String(result.status)}`
}

function parseArguments(values) {
  const parsed = { archive: null, installer: null, iterations: 100 }
  for (let index = 0; index < values.length; index += 1) {
    const argument = values[index]
    if (argument === '--archive') parsed.archive = values[++index]
    else if (argument === '--installer') parsed.installer = values[++index]
    else if (argument === '--iterations') parsed.iterations = Number(values[++index])
    else fail(`unsupported argument ${argument}`)
  }
  if (!parsed.archive) fail('--archive is required')
  parsed.archive = resolve(parsed.archive)
  if (!existsSync(parsed.archive) || !statSync(parsed.archive).isFile()) {
    fail(`archive is not a regular file: ${parsed.archive}`)
  }
  if (parsed.installer) {
    parsed.installer = resolve(parsed.installer)
    if (!existsSync(parsed.installer) || !statSync(parsed.installer).isFile()) {
      fail(`installer is not a regular file: ${parsed.installer}`)
    }
  }
  if (!Number.isSafeInteger(parsed.iterations) || parsed.iterations < 1 || parsed.iterations > 100) {
    fail('--iterations must be an integer from 1 through 100')
  }
  return parsed
}

function portable(path) {
  return path.split(sep).join('/')
}

function within(root, candidate) {
  const rel = relative(realpathSync(root), realpathSync(candidate))
  return rel === '' || (!rel.startsWith(`..${sep}`) && rel !== '..' && !isAbsolute(rel))
}

function sha256File(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex')
}

function walkFiles(root) {
  const files = []
  const visit = directory => {
    for (const name of readdirSync(directory).sort()) {
      const path = join(directory, name)
      const info = lstatSync(path)
      if (info.isSymbolicLink()) fail(`package contains a symlink: ${path}`)
      if (info.isDirectory()) visit(path)
      else if (info.isFile() && info.size > 0) files.push(path)
      else fail(`package contains a special or empty file: ${path}`)
    }
  }
  visit(root)
  return files
}

function extractArchive(archive, destination) {
  mkdirSync(destination, { recursive: true })
  if (archive.endsWith('.zip')) {
    const entries = unzipSync(new Uint8Array(readFileSync(archive)))
    for (const [rawName, bytes] of Object.entries(entries)) {
      const normalized = rawName.replaceAll('\\', '/')
      const parts = normalized.split('/').filter(Boolean)
      if (
        normalized.startsWith('/') ||
        /^[A-Za-z]:/u.test(normalized) ||
        parts.length === 0 ||
        parts.some(part => part === '.' || part === '..')
      ) {
        fail(`unsafe ZIP member: ${rawName}`)
      }
      const output = join(destination, ...parts)
      if (!output.startsWith(`${destination}${sep}`)) fail(`ZIP member escaped root: ${rawName}`)
      if (normalized.endsWith('/')) mkdirSync(output, { recursive: true })
      else {
        mkdirSync(dirname(output), { recursive: true })
        writeFileSync(output, bytes)
      }
    }
  } else if (archive.endsWith('.tar.gz')) {
    const extracted = spawnSync('tar', ['-xzf', archive, '-C', destination], {
      encoding: 'utf8',
      timeout: 120_000,
    })
    if (extracted.status !== 0) fail(`tar extraction failed: ${extracted.stderr}`)
  } else {
    fail(`unsupported archive extension: ${archive}`)
  }

  const roots = readdirSync(destination)
    .map(name => join(destination, name))
    .filter(path => lstatSync(path).isDirectory())
  if (roots.length !== 1 || readdirSync(destination).length !== 1) {
    fail('archive must contain exactly one top-level package directory')
  }
  return realpathSync(roots[0])
}

function verifyManifest(packageRoot) {
  const manifestPath = join(packageRoot, 'release-manifest.json')
  const digestPath = join(packageRoot, 'release-manifest.digest.json')
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'))
  const digest = JSON.parse(readFileSync(digestPath, 'utf8'))
  if (
    digest.schemaVersion !== 1 ||
    digest.scheme !== 'sha256' ||
    digest.manifestSha256 !== sha256File(manifestPath)
  ) {
    fail('release manifest digest binding is invalid')
  }
  const actual = walkFiles(packageRoot)
    .map(path => ({
      path: portable(relative(packageRoot, path)),
      sha256: sha256File(path),
      size: statSync(path).size,
    }))
    .filter(entry => !['release-manifest.json', 'release-manifest.digest.json'].includes(entry.path))
    .sort((left, right) => comparePortablePaths(left.path, right.path))
  if (JSON.stringify(actual) !== JSON.stringify(manifest.files)) {
    fail('extracted package inventory differs from release-manifest.json')
  }
  return manifest
}

function snapshotState(root) {
  if (!existsSync(root)) return JSON.stringify({ present: false })
  const records = []
  const visit = path => {
    const info = lstatSync(path)
    const rel = portable(relative(root, path)) || '.'
    if (info.isSymbolicLink()) {
      records.push({ path: rel, kind: 'symlink', target: readlinkSync(path) })
      return
    }
    if (info.isDirectory()) {
      records.push({ path: rel, kind: 'directory', mode: info.mode, mtimeMs: info.mtimeMs })
      for (const name of readdirSync(path).sort()) visit(join(path, name))
      return
    }
    if (info.isFile()) {
      records.push({
        path: rel,
        kind: 'file',
        mode: info.mode,
        size: info.size,
        mtimeMs: info.mtimeMs,
        sha256: sha256File(path),
      })
      return
    }
    records.push({ path: rel, kind: 'special', mode: info.mode, mtimeMs: info.mtimeMs })
  }
  visit(root)
  return JSON.stringify({ present: true, records })
}

function snapshotPathSummary(snapshot) {
  const parsed = JSON.parse(snapshot)
  const records = Array.isArray(parsed.records) ? parsed.records : []
  return {
    count: records.length,
    entries: records.slice(0, 32).map(record => ({ path: record.path, kind: record.kind })),
  }
}

function createTextCapture(stream) {
  const reader = stream.getReader()
  const decoder = new TextDecoder()
  let text = ''
  let cancelled = false
  let settled = false
  const completion = (async () => {
    try {
      while (true) {
        const read = await reader.read()
        if (read.done) break
        text += decoder.decode(read.value, { stream: true })
      }
      text += decoder.decode()
      return { closed: !cancelled }
    } finally {
      settled = true
      try {
        reader.releaseLock()
      } catch {
        // A cancelled stream may already have released the reader.
      }
    }
  })()
  completion.catch(() => {})

  return {
    completion,
    snapshot: () => text,
    cancel: async reason => {
      if (settled) return
      cancelled = true
      try {
        await reader.cancel(reason)
      } catch {
        // The stream may close between the deadline and cancellation.
      }
      await Promise.allSettled([completion])
    },
  }
}

async function cancelProcessText(captures, reason) {
  await Promise.all(captures.map(capture => capture.cancel(reason)))
}

async function collectProcessText(captures, timeoutMs, options = {}) {
  let timer
  const outcome = await Promise.race([
    Promise.all(captures.map(capture => capture.completion)).then(
      results => ({ type: 'complete', results }),
      error => ({ type: 'error', error }),
    ),
    new Promise(resolveTimeout => {
      timer = setTimeout(
        () => resolveTimeout({ type: 'timeout' }),
        timeoutMs,
      )
    }),
  ])
  clearTimeout(timer)

  if (outcome.type === 'error') {
    await cancelProcessText(captures, outcome.error)
    throw outcome.error
  }
  if (outcome.type === 'timeout') {
    if (options.cancelOnTimeout !== false) {
      await cancelProcessText(
        captures,
        new Error(`stdio drain exceeded ${timeoutMs}ms`),
      )
    }
    return {
      closed: false,
      texts: captures.map(capture => capture.snapshot()),
    }
  }
  return {
    closed: outcome.results.every(result => result.closed),
    texts: captures.map(capture => capture.snapshot()),
  }
}

function createProcessObserverLease(command, child, captures, expectedTexts) {
  let retainedChild = child
  let active = true
  const releaseChildReference = () => {
    const childReference = retainedChild
    retainedChild = null
    if (childReference) void childReference.pid
  }
  return {
    async finalize(timeoutMs = 5_000) {
      if (!active) fail(`${basename(command)} process observer lease was already released`)
      let captured
      try {
        captured = await collectProcessText(captures, timeoutMs)
      } finally {
        active = false
        // Retain the subprocess observation object and its capture readers
        // until the package daemon has completed promotion and exited.
        releaseChildReference()
      }
      const [stdout, stderr] = captured.texts
      if (!captured.closed) {
        fail(
          `${basename(command)} descendants kept stdio open after the package lifecycle completed`,
        )
      }
      if (stdout !== expectedTexts[0] || stderr !== expectedTexts[1]) {
        fail(
          `${basename(command)} emitted data after its stdout/stderr contract was accepted; stdout=${diagnosticTail(stdout)}; stderr=${diagnosticTail(stderr)}`,
        )
      }
    },
    async cancel(reason = new Error('process observer lease cancelled')) {
      if (!active) return
      active = false
      try {
        await cancelProcessText(captures, reason)
      } finally {
        releaseChildReference()
      }
    },
  }
}

function terminateTimedOutProcess(child) {
  if (process.platform === 'win32' && Number.isSafeInteger(child.pid)) {
    const result = spawnSync(
      'taskkill',
      ['/PID', String(child.pid), '/T', '/F'],
      {
        encoding: 'utf8',
        timeout: 10_000,
        windowsHide: true,
      },
    )
    if (result.status === 0) return 'taskkill-tree'
  }
  try {
    child.kill()
    return 'child-kill'
  } catch {
    return 'already-exited'
  }
}

function diagnosticTail(value) {
  const limit = 4 * 1024
  return value.length <= limit ? value : `<truncated>${value.slice(-limit)}`
}

export async function runProcess(command, args, options = {}) {
  const child = Bun.spawn({
    cmd: [command, ...args],
    cwd: options.cwd,
    env: options.env,
    stdin: 'ignore',
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const stdoutCapture = createTextCapture(child.stdout)
  const stderrCapture = createTextCapture(child.stderr)
  let timer
  const timeoutMs = options.timeoutMs ?? 60_000
  const outcome = await Promise.race([
    child.exited.then(
      code => ({ type: 'exit', code }),
      error => ({ type: 'exit-error', error }),
    ),
    new Promise(resolveTimeout => {
      timer = setTimeout(() => resolveTimeout({ type: 'timeout' }), timeoutMs)
    }),
  ])
  clearTimeout(timer)
  const termination = outcome.type === 'timeout' ? terminateTimedOutProcess(child) : null
  let captured
  try {
    captured = await collectProcessText(
      [stdoutCapture, stderrCapture],
      options.streamDrainTimeoutMs ?? 5_000,
      { cancelOnTimeout: false },
    )
  } catch (error) {
    fail(
      `${basename(command)} output capture failed: ${error instanceof Error ? error.message : String(error)}`,
    )
  }
  const [stdout, stderr] = captured.texts
  if (outcome.type === 'timeout') {
    if (!captured.closed) {
      await cancelProcessText(
        [stdoutCapture, stderrCapture],
        new Error(`${basename(command)} execution timed out`),
      )
    }
    fail(
      `${basename(command)} timed out after ${timeoutMs}ms; termination=${termination}; stdout=${diagnosticTail(stdout)}; stderr=${diagnosticTail(stderr)}`,
    )
  }
  if (outcome.type === 'exit-error') {
    if (!captured.closed) {
      await cancelProcessText(
        [stdoutCapture, stderrCapture],
        new Error(`${basename(command)} exit observation failed`),
      )
    }
    fail(
      `${basename(command)} exit observation failed: ${outcome.error instanceof Error ? outcome.error.message : String(outcome.error)}; stdout=${diagnosticTail(stdout)}; stderr=${diagnosticTail(stderr)}`,
    )
  }
  if (outcome.code !== 0) {
    if (!captured.closed) {
      await cancelProcessText(
        [stdoutCapture, stderrCapture],
        new Error(`${basename(command)} exited ${outcome.code}`),
      )
    }
    fail(`${basename(command)} exited ${outcome.code}: ${stderr.trim() || stdout.trim()}`)
  }
  if (!captured.closed) {
    if (!options.allowInheritedPipeHandles) {
      await cancelProcessText(
        [stdoutCapture, stderrCapture],
        new Error(`${basename(command)} inherited stdio is unauthorized`),
      )
      fail(
        `${basename(command)} exited 0 but descendant processes kept stdio open; stdout=${diagnosticTail(stdout)}; stderr=${diagnosticTail(stderr)}`,
      )
    }
    if (!stdout.endsWith(options.requiredStdoutSuffix ?? '\n')) {
      await cancelProcessText(
        [stdoutCapture, stderrCapture],
        new Error(`${basename(command)} stdout contract is incomplete`),
      )
      fail(`${basename(command)} left stdio open before stdout completed its contract`)
    }
    if (options.requireEmptyStderrWhenPipesOpen !== false && stderr.length > 0) {
      await cancelProcessText(
        [stdoutCapture, stderrCapture],
        new Error(`${basename(command)} stderr contract is not empty`),
      )
      fail(
        `${basename(command)} left stdio open with stderr: ${diagnosticTail(stderr)}`,
      )
    }
  }
  return {
    pid: child.pid,
    stdout,
    stderr,
    streamsClosed: captured.closed,
    processObserverLease:
      options.retainProcessObserverUntilReleased === true || !captured.closed
        ? createProcessObserverLease(
          command,
          child,
          [stdoutCapture, stderrCapture],
          [stdout, stderr],
        )
        : null,
  }
}

function writeIterationPhase(iteration, phase, detail = '') {
  process.stderr.write(
    `release package smoke phase: ${iteration} ${phase}${detail ? ` ${detail}` : ''}\n`,
  )
}

function fnv1a32(value) {
  let hash = 0x811c9dc5
  for (const byte of new TextEncoder().encode(value)) {
    hash ^= byte
    hash = Math.imul(hash, 0x01000193) >>> 0
  }
  return hash.toString(16).padStart(8, '0')
}

function sanitizeWindowsUser(value) {
  const mapped = [...value]
    .map(character => (/^[A-Za-z0-9._-]$/u.test(character) ? character : '_'))
    .join('')
  return mapped === value ? mapped : `${mapped}-${fnv1a32(value)}`
}

function memoryEndpoint(stateRoot) {
  const canonical = realpathSync(stateRoot)
  if (process.platform === 'win32') {
    let normalized = canonical.replaceAll('\\', '/').toLowerCase()
    if (normalized.startsWith('//?/unc/')) normalized = `//${normalized.slice(8)}`
    else if (normalized.startsWith('//?/')) normalized = normalized.slice(4)
    const namespace = createHash('sha256')
      .update('crabcode-memory-pipe-v1\0')
      .update(normalized)
      .digest('hex')
      .slice(0, 32)
    const user = sanitizeWindowsUser(process.env.USERNAME ?? 'user')
    return `npipe:\\\\.\\pipe\\crabcode-${user}-${namespace}-memory-orchestrator`
  }
  let socket = join(canonical, 'run', 'memory-orchestrator.sock')
  if (Buffer.byteLength(socket) > 100) {
    const namespace = createHash('sha256')
      .update('crabcode-memory-uds-v1\0')
      .update(canonical)
      .digest('hex')
      .slice(0, 32)
    socket = join('/tmp', `crabcode-memory-${namespace}`, 'memory-orchestrator.sock')
  }
  return `unix:${socket}`
}

function shortMemoryNamespace(endpoint) {
  if (process.platform === 'win32' || !endpoint.startsWith('unix:')) return null
  const socket = endpoint.slice('unix:'.length)
  const parent = dirname(socket)
  if (
    basename(socket) !== 'memory-orchestrator.sock' ||
    dirname(parent) !== '/tmp' ||
    !/^crabcode-memory-[a-f0-9]{32}$/u.test(basename(parent))
  ) {
    return null
  }
  return parent
}

async function endpointRequest(endpoint, value, timeoutMs = 2_000) {
  const path = endpoint.replace(/^(?:unix|npipe):/u, '')
  return await new Promise((resolveRequest, rejectRequest) => {
    const socket = createConnection(path)
    let buffer = ''
    const timer = setTimeout(() => {
      socket.destroy()
      rejectRequest(new Error(`endpoint request timed out: ${endpoint}`))
    }, timeoutMs)
    const finish = callback => value => {
      clearTimeout(timer)
      socket.destroy()
      callback(value)
    }
    socket.setEncoding('utf8')
    socket.on('connect', () => socket.write(`${JSON.stringify(value)}\n`))
    socket.on('data', chunk => {
      buffer += chunk
      const newline = buffer.indexOf('\n')
      if (newline !== -1) {
        try {
          finish(resolveRequest)(JSON.parse(buffer.slice(0, newline)))
        } catch (error) {
          finish(rejectRequest)(error)
        }
      }
    })
    socket.on('error', finish(rejectRequest))
    socket.on('end', () => {
      if (buffer.trim().length > 0) {
        try {
          finish(resolveRequest)(JSON.parse(buffer))
        } catch (error) {
          finish(rejectRequest)(error)
        }
      }
    })
  })
}

function executableForPid(pid) {
  try {
    if (process.platform === 'linux') return realpathSync(`/proc/${pid}/exe`)
    if (process.platform === 'darwin') {
      const result = spawnSync('lsof', ['-a', '-p', String(pid), '-d', 'txt', '-Fn'], {
        encoding: 'utf8',
        timeout: 5_000,
      })
      if (result.status !== 0) return null
      const path = spawnText(result.stdout)
        .split(/\r?\n/u)
        .find(line => line.startsWith('n'))
        ?.slice(1)
      return path ? realpathSync(path) : null
    }
    const script = `(Get-CimInstance Win32_Process -Filter "ProcessId=${pid}").ExecutablePath`
    const result = spawnSync('powershell', ['-NoProfile', '-Command', script], {
      encoding: 'utf8',
      timeout: 5_000,
    })
    if (result.status !== 0) return null
    const path = spawnText(result.stdout).trim()
    return path ? realpathSync(path) : null
  } catch {
    return null
  }
}

export function parseWindowsProcessInventory(stdout) {
  const parsed = stdout.trim() ? JSON.parse(stdout) : []
  return (Array.isArray(parsed) ? parsed : [parsed]).flatMap(item => {
    const pid = Number(item?.ProcessId)
    const executable = item?.ExecutablePath
    if (
      !Number.isSafeInteger(pid) ||
      pid <= 0 ||
      typeof executable !== 'string' ||
      executable.length === 0 ||
      !packageProcessNames.has(win32.basename(executable).toLowerCase())
    ) {
      return []
    }
    return [{ pid, executable }]
  })
}

function listPackageProcesses(packageRoot) {
  let candidates = []
  if (process.platform === 'win32') {
    const script = 'Get-CimInstance Win32_Process | Select-Object ProcessId,ExecutablePath | ConvertTo-Json -Compress'
    const result = spawnSync('powershell', ['-NoProfile', '-Command', script], {
      encoding: 'utf8',
      timeout: 15_000,
    })
    if (result.status !== 0) fail(`process inventory failed: ${spawnFailure(result)}`)
    // The single CIM snapshot already contains canonicalization candidates.
    // Filter it before ownership checks instead of launching one PowerShell
    // process for every system PID on every replay cleanup probe.
    candidates = parseWindowsProcessInventory(spawnText(result.stdout))
  } else {
    // `lsof -p` once for every system PID made one replay take minutes on
    // macOS. Pre-filter by the kernel process command, then resolve and
    // canonicalize only package-named candidates below. The final ownership
    // decision still uses the executable realpath inside packageRoot.
    const result = spawnSync('ps', ['-axo', 'pid=,comm='], {
      encoding: 'utf8',
      timeout: 10_000,
    })
    if (result.status !== 0) fail(`process inventory failed: ${spawnFailure(result)}`)
    candidates = spawnText(result.stdout)
      .split(/\r?\n/u)
      .flatMap(line => {
        const match = /^\s*(\d+)\s+(.+?)\s*$/u.exec(line)
        if (!match || !packageProcessNames.has(basename(match[2]).toLowerCase())) return []
        return [{ pid: Number(match[1]), executable: null }]
      })
  }
  return candidates.flatMap(candidate => {
    const { pid } = candidate
    try {
      const executable = candidate.executable
        ? realpathSync(candidate.executable)
        : executableForPid(pid)
      if (
        !executable ||
        !packageProcessNames.has(basename(executable).toLowerCase())
      ) {
        return []
      }
      return within(packageRoot, executable) ? [{ pid, executable }] : []
    } catch {
      return []
    }
  })
}

async function waitForPackageProcessesToExit(packageRoot, endpoint, timeoutMs) {
  const deadline = Date.now() + timeoutMs
  const shortNamespace = shortMemoryNamespace(endpoint)
  while (Date.now() < deadline) {
    const processes = listPackageProcesses(packageRoot)
    const socketGone = process.platform === 'win32' || !existsSync(endpoint.slice('unix:'.length))
    const namespaceGone = !shortNamespace || !existsSync(shortNamespace)
    if (processes.length === 0 && socketGone && namespaceGone) return
    await Bun.sleep(100)
  }
  fail(
    `package processes, endpoint, or short namespace remained after promotion: ${JSON.stringify({
      processes: listPackageProcesses(packageRoot),
      endpoint,
      shortNamespace,
    })}`,
  )
}

async function terminateOwnedProcesses(packageRoot) {
  const owned = listPackageProcesses(packageRoot)
  for (const processInfo of owned) {
    const executable = executableForPid(processInfo.pid)
    if (!executable || executable !== processInfo.executable || !within(packageRoot, executable)) continue
    try {
      process.kill(processInfo.pid, 'SIGTERM')
    } catch {
      // Process exited between validation and signal.
    }
  }
  await Bun.sleep(500)
  for (const processInfo of listPackageProcesses(packageRoot)) {
    const executable = executableForPid(processInfo.pid)
    if (!executable || executable !== processInfo.executable || !within(packageRoot, executable)) continue
    try {
      process.kill(processInfo.pid, 'SIGKILL')
    } catch {
      // Process exited between validation and signal.
    }
  }
}

function memoryLifecycleFailure(
  endpoint,
  method,
  reason,
  stateRoot,
  ownershipRoot,
) {
  let processes
  try {
    processes = listPackageProcesses(ownershipRoot)
  } catch (inventoryError) {
    processes = {
      inventoryError:
        inventoryError instanceof Error
          ? inventoryError.message
          : String(inventoryError),
    }
  }
  const logPath = join(stateRoot, 'logs', 'memory-orchestrator.log')
  let logTail = '<missing>'
  try {
    if (existsSync(logPath)) logTail = diagnosticTail(readFileSync(logPath, 'utf8'))
  } catch (logError) {
    logTail = `<unreadable: ${logError instanceof Error ? logError.message : String(logError)}>`
  }
  fail(
    `Memory ${method} failed at ${endpoint}: ${reason}; packageProcesses=${JSON.stringify(processes)}; logTail=${logTail}`,
  )
}

async function memoryLifecycleRequest(endpoint, request, stateRoot, ownershipRoot) {
  try {
    return await endpointRequest(endpoint, request)
  } catch (error) {
    memoryLifecycleFailure(
      endpoint,
      String(request.method),
      error instanceof Error ? error.message : String(error),
      stateRoot,
      ownershipRoot,
    )
  }
}

async function runIteration(packageRoot, scratchRoot, iteration, options = {}) {
  const extension = process.platform === 'win32' ? '.exe' : ''
  const launcher = options.launcher ?? join(packageRoot, `crabcode${extension}`)
  const ownershipRoot = options.ownershipRoot ?? packageRoot
  const packagedBun = join(packageRoot, `bun${extension}`)
  const stateRoot = mkdtempSync(join(scratchRoot, `state-${iteration}-`))
  const endpoint = memoryEndpoint(stateRoot)
  const baseEnvironment = { ...process.env, ...options.extraEnvironment }
  let packageProcessesExited = false
  let launcherObserverLease = null
  try {
    writeIterationPhase(iteration, 'launcher-start')
    const launcherResult = await runProcess(launcher, ['__release-package-smoke'], {
      cwd: packageRoot,
      timeoutMs: 30_000,
      streamDrainTimeoutMs: 500,
      allowInheritedPipeHandles: true,
      retainProcessObserverUntilReleased: true,
      requiredStdoutSuffix: '\n',
      env: {
        ...baseEnvironment,
        CRABCODE_CONFIG_DIR: stateRoot,
        CRABCODE_RELEASE_PACKAGE_SMOKE: '1',
      },
    })
    launcherObserverLease = launcherResult.processObserverLease
    if (!launcherObserverLease) {
      fail('launcher process observer lease was not retained')
    }
    const replay = JSON.parse(launcherResult.stdout.trim())
    if (
      replay.incident_sequence !== 6034 ||
      replay.incident_bytes !== 63 ||
      replay.incident_disposition !== 'presentation_noop' ||
      replay.turns_completed !== 2 ||
      replay.runtime_stop !== false
    ) {
      fail(`packaged Rust replay returned an invalid result: ${launcherResult.stdout}`)
    }
    writeIterationPhase(
      iteration,
      'launcher-complete',
      `observer=retained stdio=${launcherResult.streamsClosed ? 'closed' : 'open'}`,
    )

    writeIterationPhase(iteration, 'runtime-start')
    const runtimeResult = await runProcess(packagedBun, [join(repositoryRoot, 'scripts/tui-runtime-smoke.mjs')], {
      cwd: packageRoot,
      timeoutMs: 60_000,
      env: {
        ...baseEnvironment,
        CRABCODE_SMOKE_PACKAGE_ROOT: packageRoot,
        CRABCODE_SMOKE_BUN: packagedBun,
      },
    })
    const runtime = JSON.parse(runtimeResult.stdout)
    try {
      assertTuiRuntimeSmokeSuccess(runtime)
    } catch {
      fail(`packaged TypeScript runtime did not complete two turns: ${runtimeResult.stdout}`)
    }
    writeIterationPhase(iteration, 'runtime-complete')

    const identity = await memoryLifecycleRequest(
      endpoint,
      { method: 'memory.ping' },
      stateRoot,
      ownershipRoot,
    )
    if (
      identity.ok !== true ||
      identity.protocol_version !== 1 ||
      identity.schema_id !== incidentSchema ||
      typeof identity.build_id !== 'string' ||
      identity.build_id.length === 0 ||
      !Array.isArray(identity.capabilities) ||
      !identity.capabilities.includes('coordinator-promote-owner-bind-v2') ||
      !Number.isSafeInteger(identity.pid)
    ) {
      memoryLifecycleFailure(
        endpoint,
        'memory.ping',
        `invalid response ${JSON.stringify(identity)}`,
        stateRoot,
        ownershipRoot,
      )
    }
    writeIterationPhase(iteration, 'memory-ping-complete')
    const promotion = await memoryLifecycleRequest(
      endpoint,
      {
        method: 'memory.coordinator.promote',
        payload: {
          successor_build_id: '2.0.0+release-smoke-cleanup',
          expected_current_build_id: identity.build_id,
          expected_current_pid: identity.pid,
          protocol_version: 1,
          schema_id: incidentSchema,
        },
      },
      stateRoot,
      ownershipRoot,
    )
    if (
      promotion.ok !== true ||
      promotion.promote !== true ||
      promotion.current_build_id !== identity.build_id ||
      promotion.current_pid !== identity.pid ||
      promotion.successor_build_id !== '2.0.0+release-smoke-cleanup' ||
      promotion.protocol_version !== 1 ||
      promotion.schema_id !== incidentSchema
    ) {
      memoryLifecycleFailure(
        endpoint,
        'memory.coordinator.promote',
        `invalid response ${JSON.stringify(promotion)}`,
        stateRoot,
        ownershipRoot,
      )
    }
    writeIterationPhase(iteration, 'memory-promotion-acknowledged')
    await waitForPackageProcessesToExit(ownershipRoot, endpoint, 10_000)
    packageProcessesExited = true
    writeIterationPhase(iteration, 'package-processes-exited')
    if (launcherObserverLease) {
      const lease = launcherObserverLease
      launcherObserverLease = null
      await lease.finalize()
      writeIterationPhase(iteration, 'launcher-observer-finalized')
    }
  } finally {
    try {
      if (!packageProcessesExited) {
        writeIterationPhase(iteration, 'failure-cleanup-start')
        await terminateOwnedProcesses(ownershipRoot)
        writeIterationPhase(iteration, 'failure-cleanup-complete')
      }
    } finally {
      if (launcherObserverLease) {
        await launcherObserverLease.cancel(
          new Error(`release package iteration ${iteration} ended before observer finalization`),
        )
        launcherObserverLease = null
        writeIterationPhase(iteration, 'launcher-observer-cancelled')
      }
      rmSync(stateRoot, { recursive: true, force: true })
    }
  }
  if (existsSync(stateRoot)) fail(`iteration state root survived cleanup: ${stateRoot}`)
  const lingering = listPackageProcesses(ownershipRoot)
  if (lingering.length > 0) fail(`package processes survived iteration ${iteration}: ${JSON.stringify(lingering)}`)
  writeIterationPhase(iteration, 'isolation-cleanup-complete')
}

async function runInstallerSmoke(archive, installer, version, scratchRoot, baseEnvironment) {
  if (!/^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?$/u.test(version)) {
    fail(`package manifest version is not canonical SemVer: ${version}`)
  }
  const assetDirectory = join(scratchRoot, 'installer-assets')
  const installRoot = join(scratchRoot, 'installer-root')
  const dataHome = join(installRoot, 'data')
  const stateHome = join(installRoot, 'state')
  const binDirectory = join(installRoot, 'bin')
  mkdirSync(assetDirectory, { recursive: true })
  const localArchive = join(assetDirectory, basename(archive))
  copyFileSync(archive, localArchive)
  writeFileSync(
    join(assetDirectory, 'checksums-sha256.txt'),
    `${sha256File(localArchive)}  ${basename(localArchive)}\n`,
  )

  const environment = {
    ...baseEnvironment,
    CRABCODE_ASSET_DIR: assetDirectory,
    CRABCODE_BIN_DIR: binDirectory,
    CRABCODE_VERSION: `v${version}`,
    XDG_DATA_HOME: dataHome,
    XDG_STATE_HOME: stateHome,
  }
  if (process.platform === 'win32') {
    await runProcess('powershell', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', installer], {
      cwd: repositoryRoot,
      env: environment,
      timeoutMs: 120_000,
    })
  } else {
    await runProcess('sh', [installer], {
      cwd: repositoryRoot,
      env: environment,
      timeoutMs: 120_000,
    })
  }

  const destination = realpathSync(join(dataHome, 'crabcode', 'versions', version))
  verifyManifest(destination)
  const extension = process.platform === 'win32' ? '.exe' : ''
  const stableLauncher = realpathSync(join(binDirectory, `crabcode${extension}`))
  await runIteration(destination, scratchRoot, 'installer', {
    launcher: stableLauncher,
    ownershipRoot: installRoot,
    extraEnvironment: environment,
  })
}

async function main() {
  const args = parseArguments(process.argv.slice(2))
  const scratchRoot = mkdtempSync(join(tmpdir(), 'crabcode-release-package-smoke-'))
  const extractRoot = join(scratchRoot, 'package')
  // Redirect every default home/config convention into the owned scratch root.
  // The fallback .crabcode state must remain absent: any product component that
  // ignores CRABCODE_CONFIG_DIR will create it and fail this gate deterministically.
  // Third-party runtimes may create their own cache entries elsewhere in HOME.
  const fallbackHome = join(scratchRoot, 'forbidden-default-home')
  const fallbackStateRoot = join(fallbackHome, '.crabcode')
  const xdgConfigHome = join(scratchRoot, 'xdg', 'config')
  const xdgDataHome = join(scratchRoot, 'xdg', 'data')
  const xdgStateHome = join(scratchRoot, 'xdg', 'state')
  const xdgCacheHome = join(scratchRoot, 'xdg', 'cache')
  const appDataHome = join(scratchRoot, 'windows', 'roaming')
  const localAppDataHome = join(scratchRoot, 'windows', 'local')
  for (const directory of [
    fallbackHome,
    xdgConfigHome,
    xdgDataHome,
    xdgStateHome,
    xdgCacheHome,
    appDataHome,
    localAppDataHome,
  ]) {
    mkdirSync(directory, { recursive: true })
  }
  const isolatedEnvironment = {
    ...process.env,
    HOME: fallbackHome,
    USERPROFILE: fallbackHome,
    XDG_CONFIG_HOME: xdgConfigHome,
    XDG_DATA_HOME: xdgDataHome,
    XDG_STATE_HOME: xdgStateHome,
    XDG_CACHE_HOME: xdgCacheHome,
    APPDATA: appDataHome,
    LOCALAPPDATA: localAppDataHome,
  }
  const fallbackBefore = snapshotState(fallbackStateRoot)
  let packageRoot
  let primaryError
  let cleanupError
  let isolationError
  let scratchCleanupError
  try {
    packageRoot = extractArchive(args.archive, extractRoot)
    const manifest = verifyManifest(packageRoot)
    for (let iteration = 1; iteration <= args.iterations; iteration += 1) {
      process.stderr.write(
        `release package smoke iteration start: ${iteration}/${args.iterations}\n`,
      )
      await runIteration(packageRoot, scratchRoot, iteration, {
        extraEnvironment: isolatedEnvironment,
      })
      process.stderr.write(
        `release package smoke iteration complete: ${iteration}/${args.iterations}\n`,
      )
      if (iteration % 10 === 0 || iteration === args.iterations) {
        process.stderr.write(`release package smoke progress: ${iteration}/${args.iterations}\n`)
      }
    }
    if (args.installer) {
      await runInstallerSmoke(
        args.archive,
        args.installer,
        manifest.version,
        scratchRoot,
        isolatedEnvironment,
      )
    }
  } catch (error) {
    primaryError = error
  } finally {
    try {
      if (packageRoot) await terminateOwnedProcesses(packageRoot)
    } catch (error) {
      cleanupError = error
    }
    try {
      const fallbackAfter = snapshotState(fallbackStateRoot)
      if (fallbackBefore !== fallbackAfter) {
        isolationError = new Error(
          `default CrabCode state fallback was touched: ${fallbackStateRoot} ${JSON.stringify(snapshotPathSummary(fallbackAfter))}`,
        )
      }
    } catch (error) {
      isolationError = error
    }
    try {
      rmSync(scratchRoot, { recursive: true, force: true })
    } catch (error) {
      scratchCleanupError = error
    }
  }
  const secondaryErrors = [cleanupError, isolationError, scratchCleanupError].filter(Boolean)
  if (primaryError) {
    if (primaryError instanceof Error && secondaryErrors.length > 0 && primaryError.cause === undefined) {
      primaryError.cause = new AggregateError(secondaryErrors, 'release smoke cleanup failures')
    }
    throw primaryError
  }
  if (secondaryErrors.length === 1) throw secondaryErrors[0]
  if (secondaryErrors.length > 1) throw new AggregateError(secondaryErrors, 'release smoke cleanup failures')
  process.stdout.write(
    `${JSON.stringify({
      schema_version: 1,
      archive: basename(args.archive),
      iterations: args.iterations,
      incident_replays: `${args.iterations}/${args.iterations}`,
      successor_turns: `${args.iterations * 2}/${args.iterations * 2}`,
      runtime_stops: 0,
      package_process_leaks: 0,
      memory_namespace_leaks: 0,
      installer_replays: args.installer ? '1/1' : '0/0',
      default_home_changes: 0,
      real_state_changes: 0,
    })}\n`,
  )
}

if (import.meta.main) await main()
