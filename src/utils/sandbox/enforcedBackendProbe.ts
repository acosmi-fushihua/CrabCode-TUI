/**
 * 沙箱执行后端探测（W-SANDBOX-ENFORCED-DEADCODE PR-0）
 *
 * 回答一个问题：**这个构建里，沙箱到底能不能真的执行？**
 *
 * 事故背景：`sandbox.enabled=true` 时 Bash 首跑必撞
 * `SandboxRuntimeUnavailableError`（enforced 执行后端从未接线），而系统提示词
 * 教模型立刻用 `dangerouslyDisableSandbox:true` 绕过 —— 用户以为开了沙箱，
 * 实际每一条命令都在裸跑。本模块是「诚实降级层」的判据来源：
 * 后端没接线时，宽松档**自动降级 + 如实披露**，严格档**确定性拒绝**，
 * 绝不再默默裸跑。
 *
 * 判据（与 `crates/crabcode-cli/src/sandbox_probe.rs` 是同一份合同）：
 *   `version === 1 && backends.bash.available === true` ⇒ wired
 *   其余一切（spawn 失败 / 超时 / 非零退出 / JSON 坏 / 版本不符 / 缺字段）
 *   ⇒ **fail-closed 判 not-wired**。未知状态一律当「没有沙箱」，
 *   宁可降级也不能假装有沙箱 —— 假沙箱比没沙箱危险得多。
 *
 * ## 依赖纪律：本文件只准 import Node 内置模块
 *
 * TUI 主 TS 进程会从 `sandbox-adapter.ts` 直接 import 它。保持零仓内依赖可以
 * 避免探测时引入模块加载副作用，也确保探测判据只有一个真源。
 *
 * 新增仓内 import 前必须重新审查探测的启动时机和副作用。
 */

import { spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { dirname, join } from 'node:path'

/**
 * 探测子命令的 argv（Rust 端 `main.rs::try_dispatch_sandbox_probe_before_public_parse`
 * 精确匹配）。
 *
 * **`--json` 是版本偏斜守卫，不要删。** 不认识 `sandbox-probe` 的旧 `crabcode`
 * 会把它当成 prompt 位置参数，然后**启动一整个 TUI 会话**来"回答"一次探测。
 * 多带一个旧 clap 不认识的 flag，它在 `UnknownArgument` 分支立刻 exit(2)，
 * 走不到会话启动；我们读到非零退出 ⇒ not wired。
 */
export const SANDBOX_PROBE_ARGV: readonly string[] = ['sandbox-probe', '--json']

/**
 * 探测报告的 wire 版本，精确相等校验。
 * 对位 `sandbox_probe.rs::SANDBOX_PROBE_VERSION`。
 */
export const SANDBOX_PROBE_PROTOCOL_VERSION = 1

/**
 * 探测超时。helper 只做能力判断不做 I/O，正常是毫秒级；给 5s 是为了容忍
 * Windows 上 AV 扫描新进程的首跑延迟。到点即判 not-wired（fail-closed）。
 */
export const SANDBOX_PROBE_TIMEOUT_MS = 5_000

/** 显式指定 helper 二进制的 env（运维/测试注入面，优先级最高）。 */
export const SANDBOX_HELPER_BIN_ENV = 'CRABCODE_SANDBOX_HELPER_BIN'

/**
 * 后端标识（PR-4）。
 *
 * **它命名的是「哪条执行车道 + helper 自己报的平台」，不是内核机制。**
 * TS 侧能证明的只有这两件事：命令是我们前缀化成 `crabcode sandbox-exec` 跑的，
 * 以及探测报告里 `platform` 字段写着什么。至于最终生效的是 Landlock 还是
 * seccomp 还是 Seatbelt 还是受限令牌 —— 那是 helper 进程里的事，探测报告
 * **没有**这个字段（`crates/acosmi-sandbox/src/platform.rs::ExecBackendProbe`
 * 只有 `available` + `reason`）。想要机制级的名字必须先让 Rust 侧报出来，
 * 那是一次跨语言合同变更，不在本 PR 范围内。
 *
 * 取 helper 报的 `platform` 而不是 `process.platform`：探测跑的是哪个二进制、
 * 那个二进制认为自己在哪个平台上，才是「谁在隔离我」的答案。两者理论上恒等，
 * 但恒等的东西读两遍就有漂移的余地。
 *
 * 值域**刻意封闭**：它进遥测（`tengu_sandbox_backend_probe` /
 * `tengu_sandbox_exec`）与 `sandbox_runtime_kind`，开放集会让基数无界。
 */
export type SandboxBackendId =
  | 'sandbox-exec-linux'
  | 'sandbox-exec-darwin'
  | 'sandbox-exec-win32'
  | 'sandbox-exec-other'

export type EnforcedBackendProbeResult = {
  /** 后端是否真的接线可用。 */
  readonly wired: boolean
  /** `wired=false` 时的具体原因；`wired=true` 时恒为 null。 */
  readonly reason: string | null
  /**
   * `wired=true` 时的后端标识；不可用时恒为 null。
   *
   * 不可用时**不给名字**是有意的：没有后端就没有后端名，填一个
   * 「本来应该是这个」的猜测会让 UI 与遥测把降级态显示成正常态。
   */
  readonly backend: SandboxBackendId | null
}

let cachedProbe: EnforcedBackendProbeResult | undefined
let cachedHelperBin: string | null | undefined

/**
 * 解析 `crabcode` helper 二进制路径；解析不出返回 `null`（⇒ 不 spawn，直接
 * 判 `helper-not-found`）。
 *
 * 顺序：
 *   1. `CRABCODE_SANDBOX_HELPER_BIN`（存在即用，**刻意不验存在性** ——
 *      测试要能注入一个 fixture 路径，而"这个路径能不能跑"由 spawn 自己回答）
 *   2. `CRABCODE_CLI_BIN`（仓内既有的 CLI 覆盖名，见
 *      `src/utils/cronEnsure.ts::resolveCrabcodeCli`；验存在性）
 *   3. `dirname(process.execPath)` 下的 `crabcode{,.exe}`（发行包布局契约：
 *      TUI 跑在 `<runtimeDir>/bun` 下，`crabcode` 是它的 sibling；同上 anchor）
 *
 * **刻意没有「兜底交给 PATH」这一步。** PATH 上的 `crabcode` 可能是任何一个
 * 旧版本，而旧版本会把 `sandbox-probe` 当 prompt、启动一整个会话来回答一次
 * 探测（本机实测 PATH 上就有这样一个二进制）。`--json` 偏斜守卫能挡住已经被
 * spawn 的那一个，但更好的做法是**根本不去 spawn 一个我们没法先验证的东西**：
 * 探测的默认答案本来就是「没有沙箱」，为它冒启动一个陌生进程的风险不划算。
 *
 * 进程级 memoize：一次会话内二进制布局不会变。`resetEnforcedBackendProbeCache()` 清。
 */
export function resolveSandboxHelperBin(): string | null {
  if (cachedHelperBin !== undefined) return cachedHelperBin
  cachedHelperBin = resolveSandboxHelperBinUncached()
  return cachedHelperBin
}

function resolveSandboxHelperBinUncached(): string | null {
  const explicit = process.env[SANDBOX_HELPER_BIN_ENV]
  if (explicit !== undefined && explicit.length > 0) {
    return explicit
  }

  const exeName = process.platform === 'win32' ? 'crabcode.exe' : 'crabcode'

  // 与 cronEnsure.ts::resolveCrabcodeCli 同一条布局契约（那边是 cron daemon
  // 的 ensure 路径）。两处刻意各自实现：本模块必须保持零仓内 import（见文件
  // 头「依赖纪律」）。改这条链时两边一起改。
  const cliOverride = process.env.CRABCODE_CLI_BIN
  if (cliOverride !== undefined && cliOverride.length > 0 && existsSync(cliOverride)) {
    return cliOverride
  }

  const sibling = join(dirname(process.execPath), exeName)
  if (existsSync(sibling)) {
    return sibling
  }

  return null
}

/**
 * 探测沙箱执行后端是否接线。进程级 memoize（一次会话探一次）。
 *
 * 永不 throw：任何异常都被翻译成 `{wired:false, reason}`。
 */
export function probeEnforcedBackend(): EnforcedBackendProbeResult {
  if (cachedProbe !== undefined) return cachedProbe
  cachedProbe = runProbe()
  return cachedProbe
}

function notWired(reason: string): EnforcedBackendProbeResult {
  return { wired: false, reason, backend: null }
}

/**
 * 探测结论是否已经落定（memoize 命中）。
 *
 * 存在的唯一理由：遥测要「每次真探测发一条」而不是「每次读结论发一条」，
 * 而本模块必须保持零仓内 import（见文件头依赖纪律）⇒ 不能在这里调
 * `logEvent`。调用方（`sandbox-adapter.ts`）先问这一句，再决定发不发事件。
 */
export function isEnforcedBackendProbeResolved(): boolean {
  return cachedProbe !== undefined
}

/**
 * 把 `reason` 脱敏成一个**有界**的遥测 slug。
 *
 * 为什么必须脱敏：`probe-failed:${error.message}` 里的 message 可能是
 * `spawnSync /Users/<name>/.local/share/crabcode/crabcode ENOENT` —— **带用户名
 * 和绝对路径**。遥测面禁止路径与 PII，把原 reason 直接发出去就是把它们发出去。
 *
 * 规则：取 `:` 前的头部（我们自己的固定枚举），尾部只在它是单 token 的
 * `[a-z0-9-]+`（Rust 侧 `PROBE_REASON_*` / `sandbox_exec.rs` 的固定 slug）时保留，
 * 否则丢掉。任何不认识的头部一律 `other` —— 宁可少一个维度也不泄一个路径。
 */
export function probeReasonTelemetrySlug(reason: string | null): string {
  if (reason === null || reason.length === 0) return 'none'
  const separator = reason.indexOf(':')
  const head = separator === -1 ? reason : reason.slice(0, separator)
  const tail = separator === -1 ? '' : reason.slice(separator + 1)
  switch (head) {
    case 'helper-not-found':
    case 'probe-timeout':
      return head
    case 'probe-failed':
    case 'probe-version-mismatch':
      // 尾部是错误文本 / 版本号，都可能带路径或无界基数 —— 只留头部。
      return head
    case 'backend-unavailable':
    case 'runtime-failure':
      // 尾部是 Rust 侧的固定 slug 枚举（landlock-unavailable /
      // windows-sandbox-too-slow / sandbox-apply-failed …），单 token 才留。
      return /^[a-z0-9-]+$/.test(tail) ? `${head}:${tail}` : head
    default:
      return 'other'
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function runProbe(): EnforcedBackendProbeResult {
  const bin = resolveSandboxHelperBin()
  if (bin === null) {
    // 解析不出可验证的 helper ⇒ 不 spawn 任何东西（见 resolveSandboxHelperBin
    // 里「刻意没有 PATH 兜底」那段）。
    return notWired('helper-not-found')
  }

  let outcome: ReturnType<typeof spawnSync>
  try {
    outcome = spawnSync(bin, [...SANDBOX_PROBE_ARGV], {
      encoding: 'utf8',
      timeout: SANDBOX_PROBE_TIMEOUT_MS,
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
    })
  } catch (error) {
    // spawnSync 正常把失败放进 .error，但 Bun/Node 在参数层面出错时仍会 throw。
    return notWired(`probe-failed:${errorText(error)}`)
  }

  if (outcome.error) {
    const code = (outcome.error as NodeJS.ErrnoException).code
    if (code === 'ENOENT') return notWired('helper-not-found')
    if (code === 'ETIMEDOUT') return notWired('probe-timeout')
    return notWired(`probe-failed:${errorText(outcome.error)}`)
  }
  if (outcome.signal) {
    // 超时在部分平台只体现为 signal（没有 .error），单独兜住。
    return notWired(`probe-failed:killed-by-${outcome.signal}`)
  }
  if (outcome.status !== 0) {
    return notWired(`probe-failed:exit-${String(outcome.status)}`)
  }

  const stdout = typeof outcome.stdout === 'string' ? outcome.stdout : ''
  let parsed: unknown
  try {
    parsed = JSON.parse(stdout.trim())
  } catch {
    return notWired('probe-failed:invalid-json')
  }

  return interpretProbePayload(parsed)
}

/**
 * 把探测 payload 翻成结论。抽出来是为了让单测无需 spawn 就能钉住
 * 「哪些形态算 wired」这条合同本身。
 */
export function interpretProbePayload(
  payload: unknown,
): EnforcedBackendProbeResult {
  if (!isRecord(payload)) {
    return notWired('probe-failed:invalid-shape')
  }
  if (payload.version !== SANDBOX_PROBE_PROTOCOL_VERSION) {
    return notWired(`probe-version-mismatch:${String(payload.version)}`)
  }
  const backends = payload.backends
  if (!isRecord(backends)) {
    return notWired('backend-unavailable:backends-missing')
  }
  const bash = backends.bash
  if (!isRecord(bash)) {
    return notWired('backend-unavailable:bash-missing')
  }
  if (bash.available !== true) {
    const reason =
      typeof bash.reason === 'string' && bash.reason.length > 0
        ? bash.reason
        : 'unspecified'
    return notWired(`backend-unavailable:${reason}`)
  }
  return {
    wired: true,
    reason: null,
    backend: backendIdFromProbePlatform(payload.platform),
  }
}

/**
 * `platform` 字段（helper 用 TS 的写法报的：win32 / darwin / linux，其它 OS
 * 原样透传——见 `sandbox_probe.rs::probe_platform`）→ 封闭的后端标识。
 *
 * 认不出来的平台落 `sandbox-exec-other` 而不是 null：后端**是**可用的
 * （能走到这里说明 `available:true`），只是我们没有它的名字。报 null 会让
 * 上层把「有沙箱但叫不出名」误读成「没沙箱」。
 */
function backendIdFromProbePlatform(platform: unknown): SandboxBackendId {
  switch (platform) {
    case 'linux':
      return 'sandbox-exec-linux'
    case 'darwin':
      return 'sandbox-exec-darwin'
    case 'win32':
      return 'sandbox-exec-win32'
    default:
      return 'sandbox-exec-other'
  }
}

function errorText(error: unknown): string {
  if (error instanceof Error) return error.message
  return String(error)
}

/**
 * 把本会话的后端结论翻成「不可用」。
 *
 * 只允许由宿主进程直接观察到的可信事实触发（例如探测后 helper 路径消失）。
 * 绝不能由子进程退出码或 stdout/stderr 触发：那些字节属于不可信命令，后台
 * 命令还可能读取同 UID 的共享临时目录并协作伪造任何基于文件 nonce 的协议。
 *
 * 翻成不可用之后：`isSandboxingEnabled()` 的第 4 道门归 false ⇒ 提示词沙箱节
 * 消失、宽免收回、`getSandboxUnavailableReason()` 开始如实披露、严格档
 * `validateInput` 开始确定性拒绝。**下一条命令不会再走 helper。**
 *
 * 不做「过一会儿自动重试」：瞬态与确定性由用户自己的下一次会话
 * （或 `SandboxManager.reset()`）重新探测来区分。会话中途自动恢复意味着同一个
 * 会话里沙箱状态反复横跳，而每一次横跳都在改变审批语义。
 */
export function markEnforcedBackendDegraded(reason: string): void {
  cachedProbe = {
    wired: false,
    reason: `runtime-failure:${reason}`,
    // 降级即无后端。留着上一次探测的后端名会让 UI/遥测继续报一个已经不在
    // 服役的名字。
    backend: null,
  }
}

/**
 * 清空 memoize（helper 路径 + 探测结论），下次调用重新探测。
 *
 * 生产调用点 = `SandboxManager.reset()`（与它清 `checkDependencies` /
 * `isSupportedPlatform` 同生命周期）；测试也直接用。故**不带 `ForTest` 后缀** ——
 * 那个后缀会谎报「只有测试会碰它」，下一个人就会以为可以随便改它的语义。
 */
export function resetEnforcedBackendProbeCache(): void {
  cachedProbe = undefined
  cachedHelperBin = undefined
}

/**
 * 直接覆写探测结论（不 spawn）。测试用来构造 wired / not-wired 两侧矩阵。
 * 传 `undefined` 等同 reset 掉结论缓存。
 */
export function __setEnforcedBackendProbeResultForTest(
  result: EnforcedBackendProbeResult | undefined,
): void {
  cachedProbe = result
}
