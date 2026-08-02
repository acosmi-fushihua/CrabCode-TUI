/**
 * Runtime-neutral type barrel used by the native TUI backend.
 *
 * The public repository intentionally excludes the former embedded-host SDK
 * implementations. The TUI only needs the shared protocol types and hook
 * constants below.
 */

export type {
  SDKControlRequest,
  SDKControlResponse,
} from './sdk/controlTypes.js'
export * from './sdk/coreTypes.js'
export * from './sdk/runtimeTypes.js'
export type { Settings } from './sdk/settingsTypes.generated.js'
export * from './sdk/toolTypes.js'
