/**
 * Types for the LSP (Language Server Protocol) service layer.
 */

/**
 * Configuration for a single LSP server, as loaded from a plugin's
 * .lsp.json file or manifest `lspServers` field.
 */
export interface LspServerConfig {
  /** Command to execute the LSP server (e.g. "typescript-language-server"). */
  command: string
  /** Command-line arguments to pass to the server. */
  args?: string[]
  /** Mapping from file extension to LSP language ID. */
  extensionToLanguage: Record<string, string>
  /** Communication transport mechanism (default: 'stdio'). */
  transport?: 'stdio' | 'socket'
  /** Environment variables to set when starting the server. */
  env?: Record<string, string>
  /** Initialization options passed to the server during initialization. */
  initializationOptions?: unknown
  /** Settings passed via workspace/didChangeConfiguration. */
  settings?: unknown
  /** Workspace folder path to use for the server. */
  workspaceFolder?: string
  /** Maximum time to wait for server startup (milliseconds). */
  startupTimeout?: number
  /** Maximum time to wait for graceful shutdown (milliseconds). */
  shutdownTimeout?: number
  /** Whether to restart the server if it crashes. */
  restartOnCrash?: boolean
  /** Maximum number of restart attempts before giving up. */
  maxRestarts?: number
}

/**
 * An LSP server config enriched with plugin scope information.
 * Used when merging servers from multiple plugins to avoid name conflicts.
 */
export type LspServerState = 'starting' | 'running' | 'stopped' | 'stopping' | 'error'

export interface ScopedLspServerConfig extends LspServerConfig {
  /** The ConfigScope from which this server originates (e.g. 'dynamic'). */
  scope: string
  /** The source plugin identifier (e.g. 'slack@acosmi'). */
  source: string
}
