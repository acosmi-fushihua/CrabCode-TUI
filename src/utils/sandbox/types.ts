/**
 * Sandbox runtime type definitions.
 *
 * 历史上从 `@acosmi-ai/sandbox-runtime` import；该 stub package 在
 * 已退场，本文件 inline 一份本地类型供
 * sandbox-adapter.ts / configAdapters.ts / SandboxManager namespace 接口签名使用。
 *
 * 真 Rust runtime config 由 `sandboxExecConfig.ts` 从设置直接派生并写入一次性
 * 0600 配置文件，再由 `Shell.ts` 调本地 `crabcode sandbox-exec` helper 消费。
 * 本文件只保留 `SandboxRuntimeConfig` 形供转换层使用。
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
