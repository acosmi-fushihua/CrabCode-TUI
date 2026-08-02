import {
  getDefaultEnvironment,
  type StdioServerParameters,
} from '@modelcontextprotocol/sdk/client/stdio.js'
import { ReadBuffer, serializeMessage } from '@modelcontextprotocol/sdk/shared/stdio.js'
import type { Transport } from '@modelcontextprotocol/sdk/shared/transport.js'
import type { JSONRPCMessage } from '@modelcontextprotocol/sdk/types.js'
import { spawn, type ChildProcess } from 'node:child_process'
import { win32 as pathWin32 } from 'node:path'
import { PassThrough, type Stream } from 'node:stream'

const GRACEFUL_TREE_EXIT_MS = 2_000
const TREE_EXIT_POLL_MS = 25

function errorCode(error: unknown): string | undefined {
  return typeof error === 'object' &&
    error !== null &&
    'code' in error &&
    typeof error.code === 'string'
    ? error.code
    : undefined
}

/**
 * MCP stdio transport with an exact process-tree ownership contract.
 *
 * Unix children lead a detached process group and are signalled through the
 * negative PGID. Windows children are always launched through the native
 * `process-tree-exec` Job owner; absence of that helper is a hard spawn error.
 * `close()` resolves only after the complete tree and the direct child's stdio
 * have reached a terminal witness.
 */
export class ExactStdioClientTransport implements Transport {
  private readonly readBuffer = new ReadBuffer()
  private readonly stderrStream: PassThrough | null
  private child: ChildProcess | undefined
  private ownerPid: number | undefined
  private directClosed = false
  private spawnFailedTerminal = false
  private directClosePromise: Promise<void> | null = null
  private resolveDirectClose: (() => void) | null = null
  private treeSettled = false
  private closePromise: Promise<void> | null = null

  onclose?: () => void
  onerror?: (error: Error) => void
  onmessage?: (message: JSONRPCMessage) => void

  constructor(private readonly server: StdioServerParameters) {
    this.stderrStream =
      server.stderr === 'pipe' || server.stderr === 'overlapped'
        ? new PassThrough()
        : null
  }

  get stderr(): Stream | null {
    return this.stderrStream ?? this.child?.stderr ?? null
  }

  get pid(): number | null {
    return this.ownerPid ?? null
  }

  async start(): Promise<void> {
    if (this.child || this.ownerPid || this.treeSettled) {
      throw new Error('ExactStdioClientTransport already started')
    }

    let command = this.server.command
    let args = this.server.args ?? []
    const env = {
      ...getDefaultEnvironment(),
      ...this.server.env,
    }
    if (process.platform === 'win32') {
      const helper = env.CRABCODE_PROCESS_TREE_EXECUTABLE
      if (typeof helper !== 'string' || !pathWin32.isAbsolute(helper)) {
        throw new Error(
          'race-free Windows process-tree helper is unavailable; refusing to spawn an unowned MCP stdio tree',
        )
      }
      command = helper
      args = ['process-tree-exec', '--', this.server.command, ...args]
    }

    this.directClosePromise = new Promise<void>(resolve => {
      this.resolveDirectClose = resolve
    })
    const child = spawn(command, args, {
      env,
      stdio: ['pipe', 'pipe', this.server.stderr ?? 'inherit'],
      shell: false,
      windowsHide: process.platform === 'win32',
      cwd: this.server.cwd,
      detached: process.platform !== 'win32',
    })
    this.child = child
    child.once('close', () => {
      this.directClosed = true
      this.resolveDirectClose?.()
      this.resolveDirectClose = null
      // If the leader crashes while descendants survive, immediately start
      // exact group/Job settlement instead of leaving a PID-reuse window until
      // a later cache cleanup.
      void this.close().catch(error => {
        this.onerror?.(error as Error)
      })
      this.onclose?.()
    })

    await new Promise<void>((resolve, reject) => {
      const onSpawn = (): void => {
        cleanup()
        if (!child.pid) {
          reject(new Error('MCP stdio process spawned without a PID'))
          return
        }
        this.ownerPid = child.pid
        resolve()
      }
      const onError = (error: Error): void => {
        cleanup()
        this.spawnFailedTerminal = child.pid === undefined
        if (this.spawnFailedTerminal) {
          this.directClosed = true
          this.resolveDirectClose?.()
          this.resolveDirectClose = null
        }
        reject(error)
        this.onerror?.(error)
      }
      const cleanup = (): void => {
        child.off('spawn', onSpawn)
        child.off('error', onError)
      }
      child.once('spawn', onSpawn)
      child.once('error', onError)
    })

    child.on('error', error => {
      this.onerror?.(error)
    })
    child.stdin?.on('error', error => {
      this.onerror?.(error)
    })
    child.stdout?.on('data', chunk => {
      this.readBuffer.append(chunk)
      this.processReadBuffer()
    })
    child.stdout?.on('error', error => {
      this.onerror?.(error)
    })
    if (this.stderrStream && child.stderr) {
      child.stderr.pipe(this.stderrStream)
    }
  }

  async close(): Promise<void> {
    if (this.treeSettled) return
    if (this.closePromise) return this.closePromise

    const attempt = this.closeInternal()
    this.closePromise = attempt
    try {
      await attempt
    } catch (error) {
      // Retain the PID/child authority and allow the cleanup registry to retry
      // a failed signal/probe attempt.
      if (this.closePromise === attempt) this.closePromise = null
      throw error
    }
  }

  async send(message: JSONRPCMessage): Promise<void> {
    const stdin = this.child?.stdin
    if (!stdin || this.treeSettled) throw new Error('Not connected')
    const serialized = serializeMessage(message)
    if (stdin.write(serialized)) return
    await new Promise<void>(resolve => stdin.once('drain', resolve))
  }

  private processReadBuffer(): void {
    while (true) {
      try {
        const message = this.readBuffer.readMessage()
        if (message === null) break
        this.onmessage?.(message)
      } catch (error) {
        this.onerror?.(error as Error)
      }
    }
  }

  private async closeInternal(): Promise<void> {
    const child = this.child
    if (!child) {
      if (!this.spawnFailedTerminal) {
        throw new Error('MCP stdio transport lost process ownership before close')
      }
      this.markTreeSettled()
      return
    }

    try {
      child.stdin?.end()
    } catch {
      // Continue to the process-tree witness.
    }

    if (await this.waitForTreeExit(GRACEFUL_TREE_EXIT_MS)) {
      this.markTreeSettled()
      return
    }

    this.signalTree('SIGTERM')
    if (await this.waitForTreeExit(GRACEFUL_TREE_EXIT_MS)) {
      this.markTreeSettled()
      return
    }

    this.signalTree('SIGKILL')
    // No failsafe converts a live tree into success.
    await this.waitForTreeExit()
    this.markTreeSettled()
  }

  private signalTree(signal: NodeJS.Signals): void {
    if (this.treeSettled) return
    if (process.platform === 'win32') {
      if (this.directClosed) return
      const accepted = this.child?.kill(signal)
      if (!accepted && !this.directClosed) {
        throw new Error(`Windows MCP process-tree owner rejected ${signal}`)
      }
      return
    }

    const pgid = this.ownerPid
    if (!pgid) {
      if (this.spawnFailedTerminal) return
      throw new Error('Unix MCP process group has no authoritative PGID')
    }
    try {
      process.kill(-pgid, signal)
    } catch (error) {
      if (errorCode(error) !== 'ESRCH') throw error
    }
  }

  private unixTreeExists(): boolean {
    const pgid = this.ownerPid
    if (!pgid) return !this.spawnFailedTerminal
    try {
      process.kill(-pgid, 0)
      return true
    } catch (error) {
      if (errorCode(error) === 'ESRCH') return false
      if (errorCode(error) === 'EPERM') return true
      throw error
    }
  }

  private async waitForTreeExit(timeoutMs?: number): Promise<boolean> {
    const deadline = timeoutMs === undefined ? undefined : Date.now() + timeoutMs
    while (true) {
      const treeExists =
        process.platform === 'win32'
          ? !this.directClosed
          : this.unixTreeExists()
      if (!treeExists) {
        if (!this.directClosed && this.directClosePromise) {
          await this.directClosePromise
        }
        return true
      }
      if (deadline !== undefined && Date.now() >= deadline) return false
      await new Promise(resolve => setTimeout(resolve, TREE_EXIT_POLL_MS))
    }
  }

  private markTreeSettled(): void {
    this.treeSettled = true
    this.child = undefined
    this.ownerPid = undefined
    this.readBuffer.clear()
  }
}
