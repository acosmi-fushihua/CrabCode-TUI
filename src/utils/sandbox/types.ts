/**
 * Sandbox runtime type definitions.
 *
 * 历史上从 `@acosmi-ai/sandbox-runtime` import；该 stub package 在
 * 已退场，本文件 inline 一份本地类型供
 * sandbox-adapter.ts / configAdapters.ts / SandboxManager namespace 接口签名使用。
 *
 * 真 Rust runtime config 经 ExecBridge sandboxMode='enforced' 链路下传
 * (Shell.ts:349-362 useEnforcedRouting → ExecBridge.spawnManaged → Rust
 * acosmi-supervisor::ipc → AsyncSandboxRunner)；TS 端只需 SandboxRuntimeConfig
 * 形供 convertToSandboxRuntimeConfig 派出。
 */

export type FsReadRestrictionConfig = {
  denyOnly: string[]
  allowWithinDeny?: string[]
}

export type FsWriteRestrictionConfig = {
  allowOnly: string[]
  denyWithinAllow: string[]
}

export type IgnoreViolationsConfig = unknown

export type NetworkHostPattern = { host: string; [key: string]: unknown }

export type NetworkRestrictionConfig = {
  allowedHosts?: NetworkHostPattern[]
  deniedHosts?: NetworkHostPattern[]
  [key: string]: unknown
}

export type SandboxAskCallback = (
  hostPattern: NetworkHostPattern,
) => Promise<boolean> | boolean

export type SandboxDependencyCheck = {
  errors: string[]
  warnings: string[]
}

export interface SandboxRuntimeConfig {
  network?: {
    allowedDomains?: string[]
    deniedDomains?: string[]
    allowUnixSockets?: string[]
    allowAllUnixSockets?: boolean
    allowLocalBinding?: boolean
    httpProxyPort?: number
    socksProxyPort?: number
  }
  filesystem?: {
    denyRead?: string[]
    allowRead?: string[]
    allowWrite?: string[]
    denyWrite?: string[]
  }
  ignoreViolations?: IgnoreViolationsConfig
  enableWeakerNestedSandbox?: boolean
  enableWeakerNetworkIsolation?: boolean
  ripgrep?: {
    command?: string
    args?: string[]
    argv0?: string
  }
  [key: string]: unknown
}

export interface SandboxViolationEvent {
  type: string
  [key: string]: unknown
}

/**
 * SandboxViolationStore — minimal subscribe/getTotalCount surface used by
 * SandboxPromptFooterHint.tsx + SandboxViolationExpandedView.tsx.
 *
 * R-Sandbox Phase 4 wave 2 path B：永远返 STUB（subscribe noop，count 0）。
 * Phase 5 acosmi-protocol 接通后由 Rust 端经 IPC 推送 violation 事件。
 */
export type SandboxViolationStore = {
  subscribe: (
    listener: (events: SandboxViolationEvent[]) => void,
  ) => () => void
  getTotalCount: () => number
}
