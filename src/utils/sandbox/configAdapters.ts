/**
 * Sandbox configuration shape adapters.
 *
 * 抽离自 sandbox-adapter.ts 以让单测可独立 import（sandbox-adapter.ts 经
 * tools/BashTool/toolName.js 等链路连带 AgentTool.tsx 等 MACRO build-time
 * 依赖，bun:test 不能直接 import）。本文件只依赖 SandboxRuntimeConfig 类型，
 * 0 副作用，单测可裸跑。
 *
 * stub @acosmi-ai/sandbox-runtime 退场后，13 个 fs/network/socket getter
 * 改走 settings 派生 + convertToSandboxRuntimeConfig 派出
 * `SandboxRuntimeConfig` (filesystem.{denyRead,allowRead,allowWrite,denyWrite})；
 * 但 caller 解构形是 BaseSandboxManager 历史接口
 * ({denyOnly,allowWithinDeny} / {allowOnly,denyWithinAllow} /
 * {allowedHosts:NetworkHostPattern[]})。
 *
 * 三个 derive helper 桥接接口形态——19 caller groups 0 改动。
 */
import type {
  NetworkHostPattern,
  SandboxRuntimeConfig,
} from './types.js'

export function deriveFsReadConfig(rc: SandboxRuntimeConfig): {
  denyOnly: string[]
  allowWithinDeny: string[]
} {
  return {
    denyOnly: rc.filesystem?.denyRead ?? [],
    allowWithinDeny: rc.filesystem?.allowRead ?? [],
  }
}

export function deriveFsWriteConfig(rc: SandboxRuntimeConfig): {
  allowOnly: string[]
  denyWithinAllow: string[]
} {
  return {
    allowOnly: rc.filesystem?.allowWrite ?? [],
    denyWithinAllow: rc.filesystem?.denyWrite ?? [],
  }
}

export function deriveNetworkRestrictionConfig(rc: SandboxRuntimeConfig): {
  allowedHosts: NetworkHostPattern[]
  deniedHosts: NetworkHostPattern[]
} {
  return {
    allowedHosts: (rc.network?.allowedDomains ?? []).map(d => ({ host: d })),
    deniedHosts: (rc.network?.deniedDomains ?? []).map(d => ({ host: d })),
  }
}
