/**
 * Sandbox runtime fail-loud guard error —— **构造性不可达 tripwire**。
 *
 * W-SANDBOX-ENFORCED-DEADCODE PR-0（2026-08-08）重写文案。旧文案说这是
 * 「Rust acosmi-sandbox backend not reachable via the retired mediated route」，暗示是
 * 一个运行期连接问题（a transient transport outage）。那是假的：那条
 * enforced 执行后端**从未接线**，所以命中这个 throw 的真正原因永远是「这个
 * 构建没有沙箱执行能力」，不是「链路断了」。把它写成连接问题会把每一个读者
 * 送去查网络与进程，查不出任何东西。（死链本体已于 PR-6 删除。）
 *
 * PR-0 之后本类**在生产路径上不应再被命中**：`isSandboxingEnabled()` 的第 4
 * 道门（`isEnforcedBackendWired()`）在后端不可用时把 `shouldUseSandbox` 整条
 * 链拉成 false，`wrapWithSandbox` 因此不再有调用者。所以：
 *
 *   **命中即 bug —— 说明有一条路径绕过了第 4 道门，请上报。**
 *
 * 类保留不删，正是为了在那种情况下大声喊出来，而不是静默裸跑。
 *
 * 详 `the sandbox direct-execution contract`
 */
export class SandboxRuntimeUnavailableError extends Error {
  readonly missingMethod: string

  constructor(missingMethod: string) {
    super(
      `Sandbox execution backend is not wired for ${missingMethod}(): this ` +
        `CrabCode build ships no sandbox execution backend, so there is nothing ` +
        `to isolate the command with. ` +
        `This path is meant to be constructively unreachable — SandboxManager.isSandboxingEnabled() ` +
        `already returns false when the backend probe reports unavailable, so reaching ` +
        `here means some caller bypassed that gate. Please report it. ` +
        `See the sandbox direct-execution contract`,
    )
    this.name = 'SandboxRuntimeUnavailableError'
    this.missingMethod = missingMethod
  }
}

import type { SandboxBackendId } from './enforcedBackendProbe.js'

/**
 * 本会话里「到底是谁在隔离命令」。
 *
 * W-SANDBOX-ENFORCED-DEADCODE PR-4 重定义。**旧值域 `'stub' | 'rust' | 'unknown'`
 * 已删除，不留兼容别名** —— 旧实现 `getSandboxRuntimeKind()` 是一行 `return 'rust'`，
 * 恒真，注释还写着"Phase 3 闭环后默认返 'rust'：enforced 路径三平台均经
 * the retired mediated route"。那条链路一次都没接通过（the retired-path audit），所以每一条
 * `sandbox_runtime_kind: 'rust'` 的遥测都在报告一件没发生的事。指向已死语义的
 * 常量是 `MAIN_LOOP_FALLBACK_MODEL` 型的雷，保留别名只会让它在下一次读者手里
 * 复活。
 *
 * 三档语义互斥且穷尽：
 * - `'off'` —— 这一轮命令不会被隔离，且**不是因为后端坏了**（用户没开 / 平台
 *   不在 enabledPlatforms / 依赖缺失）
 * - `'degraded'` —— 用户开了沙箱，但后端探测说跑不出来 ⇒ 诚实降级为不隔离
 * - `SandboxBackendId` —— 真在隔离，值即后端标识
 */
export type SandboxRuntimeKind = 'off' | 'degraded' | SandboxBackendId
