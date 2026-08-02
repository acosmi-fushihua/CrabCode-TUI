/**
 * Sandbox runtime fail-loud guard error.
 *
 * The retired `@acosmi-ai/sandbox-runtime` stub is not used;
 * 沙箱真隔离走 `Shell.ts` enforced routing → ExecBridge IPC → Rust
 * `acosmi-sandbox::AsyncSandboxRunner`。
 *
 * 本 error 在 enforced routing 不可达但 caller 仍调 `wrapWithSandbox` 时抛出。
 * The routing pre-hook (`computeUseEnforcedRouting`) must select the
 * enforced 路径；任一不满足时 `Shell.ts:379` 仍会调 `wrapWithSandbox`，此时无
 * 真后端可调 → 抛此错误给清晰诊断（避免静默走 host 直跑造成安全 footgun）。
 *
 * 触发场景（`shouldUseSandbox=true` AND `useEnforcedRouting=false`）：
 * 1. `isExecBridgeFallback=true` — 循环防御 sentinel（spawnManaged catch 兜底回路）
 * 2. `shellType==='powershell'` — PowerShell sandbox macOS/Linux 暂未接通（R5 独立门）
 * 3. `!isIpcAvailable` — Rust supervisor 离线（异常环境）
 */
export class SandboxRuntimeUnavailableError extends Error {
  readonly missingMethod: string

  constructor(missingMethod: string) {
    super(
      `Sandbox enforced routing unavailable for ${missingMethod}(): Rust ` +
        `acosmi-sandbox backend not reachable via ExecBridge IPC. ` +
        `Triggered when computeUseEnforcedRouting returns false but ` +
        `shouldUseSandbox=true (likely causes: IPC offline / PowerShell shell / ` +
        `fallback sentinel).`,
    )
    this.name = 'SandboxRuntimeUnavailableError'
    this.missingMethod = missingMethod
  }
}

export type SandboxRuntimeKind = 'stub' | 'rust' | 'unknown'
