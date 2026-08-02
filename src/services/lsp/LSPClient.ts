import { type ChildProcess, spawn } from 'child_process'
import {
  createMessageConnection,
  type MessageConnection,
  StreamMessageReader,
  StreamMessageWriter,
  Trace,
} from 'vscode-jsonrpc/node.js'
import type {
  InitializeParams,
  InitializeResult,
  ServerCapabilities,
} from 'vscode-languageserver-protocol'
import { logForDebugging } from '../../utils/debug.js'
import { errorMessage } from '../../utils/errors.js'
import { logError } from '../../utils/log.js'
import { subprocessEnv } from '../../utils/subprocessEnv.js'

const DEFAULT_LSP_SHUTDOWN_TIMEOUT_MS = 5_000

async function waitUntilDeadline<T>(
  promise: Promise<T>,
  deadlineMs: number,
  timeoutMessage: string,
): Promise<T> {
  const remainingMs = Math.max(0, deadlineMs - Date.now())
  let timer: ReturnType<typeof setTimeout> | undefined
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => reject(new Error(timeoutMessage)), remainingMs)
      }),
    ])
  } finally {
    if (timer) clearTimeout(timer)
  }
}
/**
 * LSP client interface.
 */
export type LSPClient = {
  readonly capabilities: ServerCapabilities | undefined
  readonly isInitialized: boolean
  start: (
    command: string,
    args: string[],
    options?: {
      env?: Record<string, string>
      cwd?: string
    },
  ) => Promise<void>
  initialize: (params: InitializeParams) => Promise<InitializeResult>
  sendRequest: <TResult>(method: string, params: unknown) => Promise<TResult>
  sendNotification: (method: string, params: unknown) => Promise<void>
  onNotification: (method: string, handler: (params: unknown) => void) => void
  onRequest: <TParams, TResult>(
    method: string,
    handler: (params: TParams) => TResult | Promise<TResult>,
  ) => void
  stop: (shutdownTimeoutMs?: number) => Promise<void>
}

/**
 * Create an LSP client wrapper using vscode-jsonrpc.
 * Manages communication with an LSP server process via stdio.
 *
 * @param onCrash - Called when the server process exits unexpectedly (non-zero
 *   exit code during operation, not during intentional stop). Allows the owner
 *   to propagate crash state so the server can be restarted on next use.
 */
export function createLSPClient(
  serverName: string,
  onCrash?: (error: Error) => void,
): LSPClient {
  // State variables in closure
  let process: ChildProcess | undefined
  let connection: MessageConnection | undefined
  let capabilities: ServerCapabilities | undefined
  let isInitialized = false
  let startFailed = false
  let startError: Error | undefined
  let isStopping = false // Track intentional shutdown to avoid spurious error logging
  let processClosed = false
  // Queue handlers registered before connection ready (lazy initialization support)
  const pendingHandlers: Array<{
    method: string
    handler: (params: unknown) => void
  }> = []
  const pendingRequestHandlers: Array<{
    method: string
    handler: (params: unknown) => unknown | Promise<unknown>
  }> = []

  function checkStartFailed(): void {
    if (startFailed) {
      throw startError || new Error(`LSP server ${serverName} failed to start`)
    }
  }

  return {
    get capabilities(): ServerCapabilities | undefined {
      return capabilities
    },

    get isInitialized(): boolean {
      return isInitialized
    },

    async start(
      command: string,
      args: string[],
      options?: {
        env?: Record<string, string>
        cwd?: string
      },
    ): Promise<void> {
      try {
        // 1. Spawn LSP server process
        process = spawn(command, args, {
          stdio: ['pipe', 'pipe', 'pipe'],
          env: { ...subprocessEnv(), ...options?.env },
          cwd: options?.cwd,
          // Prevent visible console window on Windows (no-op on other platforms)
          windowsHide: true,
        })
        processClosed = false
        process.once('close', () => {
          processClosed = true
        })

        if (!process.stdout || !process.stdin) {
          throw new Error('LSP server process stdio not available')
        }

        // 1.5. Wait for process to successfully spawn before using streams
        // This is CRITICAL: spawn() returns immediately, but the 'error' event
        // (e.g., ENOENT for command not found) fires asynchronously.
        // If we use the streams before confirming spawn succeeded, we get
        // unhandled promise rejections when writes fail on invalid streams.
        const spawnedProcess = process // Capture for closure
        await new Promise<void>((resolve, reject) => {
          const onSpawn = (): void => {
            cleanup()
            resolve()
          }
          const onError = (error: Error): void => {
            cleanup()
            reject(error)
          }
          const cleanup = (): void => {
            spawnedProcess.removeListener('spawn', onSpawn)
            spawnedProcess.removeListener('error', onError)
          }
          spawnedProcess.once('spawn', onSpawn)
          spawnedProcess.once('error', onError)
        })

        // Capture stderr for server diagnostics and errors
        if (process.stderr) {
          process.stderr.on('data', (data: Buffer) => {
            const output = data.toString().trim()
            if (output) {
              logForDebugging(`[LSP SERVER ${serverName}] ${output}`)
            }
          })
        }

        // Handle process errors (after successful spawn, e.g., crash during operation)
        process.on('error', error => {
          if (!isStopping) {
            startFailed = true
            startError = error
            logError(
              new Error(
                `LSP server ${serverName} failed to start: ${error.message}`,
              ),
            )
          }
        })

        process.on('exit', (code, _signal) => {
          if (code !== 0 && code !== null && !isStopping) {
            isInitialized = false
            startFailed = false
            startError = undefined
            const crashError = new Error(
              `LSP server ${serverName} crashed with exit code ${code}`,
            )
            logError(crashError)
            onCrash?.(crashError)
          }
        })

        // Handle stdin stream errors to prevent unhandled promise rejections
        // when the LSP server process exits before we finish writing
        process.stdin.on('error', (error: Error) => {
          if (!isStopping) {
            logForDebugging(
              `LSP server ${serverName} stdin error: ${error.message}`,
            )
          }
          // Error is logged but not thrown - the connection error handler will catch this
        })

        // 2. Create JSON-RPC connection
        const reader = new StreamMessageReader(process.stdout)
        const writer = new StreamMessageWriter(process.stdin)
        connection = createMessageConnection(reader, writer)

        // 2.5. Register error/close handlers BEFORE listen() to catch all errors
        // This prevents unhandled promise rejections when the server crashes or closes unexpectedly
        connection.onError(([error, _message, _code]) => {
          // Only log if not intentionally stopping (avoid spurious errors during shutdown)
          if (!isStopping) {
            startFailed = true
            startError = error
            logError(
              new Error(
                `LSP server ${serverName} connection error: ${error.message}`,
              ),
            )
          }
        })

        connection.onClose(() => {
          // Only treat as error if not intentionally stopping
          if (!isStopping) {
            isInitialized = false
            // Don't set startFailed here - the connection may close after graceful shutdown
            logForDebugging(`LSP server ${serverName} connection closed`)
          }
        })

        // 3. Start listening for messages
        connection.listen()

        // 3.5. Enable protocol tracing for debugging
        // Note: trace() sends a $/setTrace notification which can fail if the server
        // process has already exited. We catch and log the error rather than letting
        // it become an unhandled promise rejection.
        connection
          .trace(Trace.Verbose, {
            log: (message: string) => {
              logForDebugging(`[LSP PROTOCOL ${serverName}] ${message}`)
            },
          })
          .catch((error: Error) => {
            logForDebugging(
              `Failed to enable tracing for ${serverName}: ${error.message}`,
            )
          })

        // 4. Apply any queued notification handlers
        for (const { method, handler } of pendingHandlers) {
          connection.onNotification(method, handler)
          logForDebugging(
            `Applied queued notification handler for ${serverName}.${method}`,
          )
        }
        pendingHandlers.length = 0 // Clear the queue

        // 5. Apply any queued request handlers
        for (const { method, handler } of pendingRequestHandlers) {
          connection.onRequest(method, handler)
          logForDebugging(
            `Applied queued request handler for ${serverName}.${method}`,
          )
        }
        pendingRequestHandlers.length = 0 // Clear the queue

        logForDebugging(`LSP client started for ${serverName}`)
      } catch (error) {
        const err = error as Error
        logError(
          new Error(`LSP server ${serverName} failed to start: ${err.message}`),
        )
        throw error
      }
    },

    async initialize(params: InitializeParams): Promise<InitializeResult> {
      if (!connection) {
        throw new Error('LSP client not started')
      }

      checkStartFailed()

      try {
        const result: InitializeResult = await connection.sendRequest(
          'initialize',
          params,
        )

        capabilities = result.capabilities

        // Send initialized notification
        await connection.sendNotification('initialized', {})

        isInitialized = true
        logForDebugging(`LSP server ${serverName} initialized`)

        return result
      } catch (error) {
        const err = error as Error
        logError(
          new Error(
            `LSP server ${serverName} initialize failed: ${err.message}`,
          ),
        )
        throw error
      }
    },

    async sendRequest<TResult>(
      method: string,
      params: unknown,
    ): Promise<TResult> {
      if (!connection) {
        throw new Error('LSP client not started')
      }

      checkStartFailed()

      if (!isInitialized) {
        throw new Error('LSP server not initialized')
      }

      try {
        return await connection.sendRequest(method, params)
      } catch (error) {
        const err = error as Error
        logError(
          new Error(
            `LSP server ${serverName} request ${method} failed: ${err.message}`,
          ),
        )
        throw error
      }
    },

    async sendNotification(method: string, params: unknown): Promise<void> {
      if (!connection) {
        throw new Error('LSP client not started')
      }

      checkStartFailed()

      try {
        await connection.sendNotification(method, params)
      } catch (error) {
        const err = error as Error
        logError(
          new Error(
            `LSP server ${serverName} notification ${method} failed: ${err.message}`,
          ),
        )
        // Don't re-throw for notifications - they're fire-and-forget
        logForDebugging(`Notification ${method} failed but continuing`)
      }
    },

    onNotification(method: string, handler: (params: unknown) => void): void {
      if (!connection) {
        // Queue handler for application when connection is ready (lazy initialization)
        pendingHandlers.push({ method, handler })
        logForDebugging(
          `Queued notification handler for ${serverName}.${method} (connection not ready)`,
        )
        return
      }

      checkStartFailed()

      connection.onNotification(method, handler)
    },

    onRequest<TParams, TResult>(
      method: string,
      handler: (params: TParams) => TResult | Promise<TResult>,
    ): void {
      if (!connection) {
        // Queue handler for application when connection is ready (lazy initialization)
        pendingRequestHandlers.push({
          method,
          handler: handler as (params: unknown) => unknown | Promise<unknown>,
        })
        logForDebugging(
          `Queued request handler for ${serverName}.${method} (connection not ready)`,
        )
        return
      }

      checkStartFailed()

      connection.onRequest(method, handler)
    },

    async stop(
      shutdownTimeoutMs = DEFAULT_LSP_SHUTDOWN_TIMEOUT_MS,
    ): Promise<void> {
      // Mark as stopping to prevent error handlers from logging spurious errors
      isStopping = true
      const deadlineMs = Date.now() + Math.max(0, shutdownTimeoutMs)
      const ownedProcess = process
      const closedWitness = ownedProcess
        ? processClosed
          ? Promise.resolve()
          : new Promise<void>(resolve => {
              ownedProcess.once('close', () => resolve())
            })
        : Promise.resolve()
      let gracefulFailed = false

      try {
        if (connection) {
          // Try to send shutdown request and exit notification
          await waitUntilDeadline(
            (async () => {
              await connection!.sendRequest('shutdown', {})
              await connection!.sendNotification('exit', {})
            })(),
            deadlineMs,
            `LSP server ${serverName} graceful shutdown timed out after ${shutdownTimeoutMs}ms`,
          )
        }
      } catch (error) {
        const err = error as Error
        logError(
          new Error(`LSP server ${serverName} stop failed: ${err.message}`),
        )
        gracefulFailed = true
        // Protocol failure never skips the process-level terminal witness.
      } finally {
        if (connection) {
          try {
            connection.dispose()
          } catch (error) {
            // Log but don't throw - disposal errors are less critical
            logForDebugging(
              `Connection disposal failed for ${serverName}: ${errorMessage(error)}`,
            )
          }
          connection = undefined
        }
      }

      try {
        if (ownedProcess) {
          if (ownedProcess.pid === undefined && startFailed) {
            // The spawn error itself is the terminal witness: no OS process
            // was created, so there is nothing to signal or reap.
            processClosed = true
          } else if (!processClosed && !gracefulFailed) {
            try {
              await waitUntilDeadline(
                closedWitness,
                deadlineMs,
                `LSP server ${serverName} did not exit within its ${shutdownTimeoutMs}ms graceful shutdown window`,
              )
            } catch {
              gracefulFailed = true
            }
          }

          if (!processClosed) {
            const killAccepted = ownedProcess.kill('SIGKILL')
            if (!killAccepted && !processClosed) {
              throw new Error(
                `LSP server ${serverName} force-kill was rejected before exit`,
              )
            }
            // No second timeout is converted into success. The worker host
            // owns the outer shutdown deadline; this promise resolves only
            // after Node's `close` event confirms process exit and stdio close.
            await closedWitness
          }

          ownedProcess.removeAllListeners('error')
          ownedProcess.removeAllListeners('exit')
          ownedProcess.removeAllListeners('close')
          ownedProcess.stdin?.removeAllListeners('error')
          ownedProcess.stderr?.removeAllListeners('data')
          if (process === ownedProcess) process = undefined
          processClosed = true
        }

        isInitialized = false
        capabilities = undefined
        logForDebugging(`LSP client stopped for ${serverName}`)
      } finally {
        isStopping = false // Reset for a retry/restart.
      }
    },
  }
}
