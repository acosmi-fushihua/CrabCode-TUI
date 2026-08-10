/**
 * 隔离保真度 —— **这台机器上，我们答应的规则里有哪几条兑现不了**
 * （W-SANDBOX-ENFORCED-DEADCODE PR-5 硬前提）。
 *
 * ## 为什么它必须是平台感知的
 *
 * PR-1 的第一版判据只看域名级规则（`network.allowedDomains` 等），于是一台
 * 「fs 规则一条都强制不了」的 Windows 机器只要没配域名白名单就报 `full`。
 * PR-5 要把 auto-allow 免审批宽免绑到 `fidelity.level === 'full'` 上，那一刻
 * 这个报告就从「一句自述」变成了**授权判据** —— 报错了 = 用户被免掉审批，
 * 而沙箱其实没拦住任何东西。所以绑之前必须先让它说真话。
 *
 * ## 三平台的 fs 强制能力（各自有 Rust 侧实证锚点）
 *
 * | 平台 | fs allow | fs deny | network policy | 依据 |
 * |---|---|---|---|---|
 * | macOS | ✅ | ✅ | ✅ | `macos/seatbelt.rs` 生成 deny 段，施加后逐条 `sandbox_check` 运行期实证 |
 * | Linux / WSL2 | ✅ | ⚠️ 仅在 allow 之外 | ✅ | `platform.rs::apply_exec_plan_to_self` 的 `UNENFORCEABLE(linux)` 锚点 |
 * | Windows | ❌ | ❌ | ❌ | `platform.rs::run_exec_plan_as_child` 的 `UNENFORCEABLE(windows)` 锚点 |
 *
 * **Linux 的那条 ⚠️ 是 Landlock 的模型决定的**：它是纯 allow-grant，放行一棵子树
 * 之后没有任何办法在里面挖洞（空权限集会被内核直接拒绝）。而
 * `sandbox-adapter.ts::convertToSandboxRuntimeConfig` 恒把 `.` 推进 `allowWrite`，
 * `settings.json` / `.crabcode/skills` / bare-repo 路径全落在它下面 —— 所以
 * **典型会话必有 deny 落在 allow 内**。
 *
 * ## 域名级过滤是第二个维度（PR-8 Slice 4 接入）
 *
 * 上表那一列 `network policy` 说的是**内核三档**（none/restricted/host），与
 * 「能不能按域名 allow/deny」是两件事。域名判据住在用户态代理里
 * （`networkFilter.ts` + `filteringProxy.ts`）。当前实现仍无法证明所有网络出口都
 * 必经该代理：Linux 的端口规则不绑定目的地址，macOS 仍保留 DNS 出口，Windows
 * 没有网络过滤面。因此三平台都保守报告域名过滤未完全强制，避免据此免除审批。
 *
 * ## 判据只用「deny 表非空」这一位，刻意不复刻 Rust 的重叠判定
 *
 * Rust 侧 `exec_rules.rs::shadowing_grant` 会逐条算「这条 deny 是否真的落在某个
 * 已放行子树里」。在 TS 这边再算一遍就是 SoT §4 E5 点名的**第二判定脑**：两套
 * 路径归一化、两套 glob 语义、零 parity 闸门，漂移是时间问题而不是可能性问题。
 * 这里只问一个不会漂的问题 ——「有没有 deny 规则」—— 答案是「有」就标 partial。
 * 方向宁严：多标一次 partial 只让 auto-allow 少发一次宽免（用户多按一次批准），
 * 少标一次则是把免审批建立在一条没生效的规则上。
 *
 * ## 收录原则（沿用 PR-1）
 *
 * 只收「不兑现会让沙箱比承诺**更弱**」的项。`network.allowUnixSockets` /
 * `allowLocalBinding` 不兑现的方向是更**严**（全拦），那是功能缺口不是安全缺口，
 * 收进来只会让一批本来很安全的用户白白丢掉 auto-allow。
 *
 * 本模块**保持纯函数、零副作用、零 I/O**：它的两个消费者
 * （`sandboxExecConfig.ts` 的 per-command 派生、`sandbox-adapter.ts` 的会话级
 * 授权判据）必须得到逐字节相同的结论，共享同一个实现是保证这件事的唯一办法。
 */

import type { Platform } from '../platform.js'

/**
 * 隔离保真度报告。
 *
 * `unenforced` 里的字符串是**规则名**（点分路径，对齐 wire 字段名），供人读与
 * 日志归类；它随配置文件下传到 Rust 端的 `FidelityReport.unenforced`
 * （`Vec<String>`），**加新取值不改 wire schema**（`deny_unknown_fields` 管的是
 * 字段名，不管数组元素）。
 */
export type SandboxFidelity = {
  level: 'full' | 'partial'
  /** 承诺了但当前无法强制的规则名（点分路径，对齐 wire 字段名）。 */
  unenforced: string[]
}

/**
 * 计算保真度所需的最小输入。刻意用 `readonly string[]` 结构化声明而不是 import
 * wire 类型 —— 让本模块不依赖 wire schema，也让会话级调用方不必先造一份完整
 * 配置文档才能问出「这台机器兑现得了吗」。
 */
export type SandboxFidelityInput = {
  platform: Platform
  filesystem: {
    readonly allowRead: readonly string[]
    readonly allowWrite: readonly string[]
    readonly denyRead: readonly string[]
    readonly denyWrite: readonly string[]
  }
  network: {
    readonly allowedDomains: readonly string[]
    readonly deniedDomains: readonly string[]
    /** 策略级承诺（`policySettings.sandbox.network.allowManagedDomainsOnly`）。 */
    readonly allowManagedDomainsOnly: boolean
    /**
     * live 过滤代理端口；`0` = 此刻没有代理在跑 ⇒ 域名规则这一刻兑现不了。
     *
     * **必填**（刻意不给可选默认值）：漏传就等于替调用方按「没有代理」回答，
     * 方向虽然安全，却会造出一个恒 partial 的死判据 —— 与恒 full 一样是判据脱
     * 离真值，只是没人会来报 bug。必填逼每个调用方自己去看一眼代理起没起。
     */
    readonly httpProxyPort: number
  }
}

/**
 * Linux/WSL2 专用的复合名：不是某张表整体不强制，而是**deny 与 allow 的重叠部分**
 * 兑现不了。与 allow 不重叠的 deny 由 Landlock 的默认拒绝天然兑现。
 *
 * 名字沿用仓内既有词汇（`prompt.ts` 的 `filesystemConfig.write.denyWithinAllow`），
 * 不新造术语。
 */
export const FIDELITY_LINUX_DENY_WITHIN_ALLOW = 'filesystem.denyWithinAllow'

/**
 * 平台对 fs 四表 / network 档位的强制能力。`false` = 这一类规则被携带、被校验、
 * 然后**不施加**（Rust 侧会为每条留下 `UNENFORCEABLE(<platform>)` notice）。
 */
type PlatformEnforcement = {
  /** allow 表能把命令收窄到这些位置吗。 */
  fsAllow: boolean
  /** deny 表能在 allow 之外挡住吗。 */
  fsDenyOutsideAllow: boolean
  /** deny 表能在**已放行的子树内部**挖洞吗（Landlock 恰恰不能）。 */
  fsDenyWithinAllow: boolean
  /** 内核三档网络策略（none/restricted/host）真的施加吗。 */
  networkPolicy: boolean
  /**
   * 能不能把 TCP 出口**锁死在一个 loopback 代理端口上**。
   *
   * 这是用户态域名过滤器升格成权威判据的唯一途径：绕过代理直接 `socket()` 的
   * 程序被内核拒绝，于是「不经代理就出不去」＝「代理的判据就是最终判据」。
   * 没有这把锁时代理只是一句劝告（`HTTPS_PROXY` 只劝得动守规矩的客户端）。
   */
  networkDomainFilter: boolean
}

function platformEnforcement(platform: Platform): PlatformEnforcement {
  switch (platform) {
    case 'macos':
      // Seatbelt 的 SBPL 同时表达 allow 与 deny，且施加后逐条 `sandbox_check`
      // 运行期实证（证不出即 125 sandbox-verify-failed，绝不当成功）。
      return {
        fsAllow: true,
        fsDenyOutsideAllow: true,
        fsDenyWithinAllow: true,
        networkPolicy: true,
        // TCP 被收窄到本机代理，但 DNS UDP 仍是独立出口；不能据此宣称域名规则
        // 完全强制，也不能触发免审批。
        networkDomainFilter: false,
      }
    case 'linux':
    case 'wsl':
      // Landlock V4 + Seccomp。allow 是真的，deny 只有落在放行子树**之外**时
      // 才由默认拒绝兑现；落在里面的挖不了洞。
      return {
        fsAllow: true,
        fsDenyOutsideAllow: true,
        fsDenyWithinAllow: false,
        networkPolicy: true,
        // Landlock 当前只按目的端口授权，不能把同端口连接绑定到 loopback 地址；
        // UDP 也不在该规则面内。即使 canary 通过，仍保守视为非完整域名强制。
        networkDomainFilter: false,
      }
    case 'windows':
      // 受限令牌 + Job 对象：隔离的是令牌与作业，不是路径，也没有网络过滤器。
      // 没有出口锁 ⇒ 代理只能靠 env 劝，域名规则永远不算强制。
      return {
        fsAllow: false,
        fsDenyOutsideAllow: false,
        fsDenyWithinAllow: false,
        networkPolicy: false,
        networkDomainFilter: false,
      }
    default:
      // 'unknown'：没有任何实证说得出这台机器能兑现什么。**不认识的平台一律按
      // 什么都强制不了处理** —— fail-closed 的方向是少发宽免，不是多发。
      return {
        fsAllow: false,
        fsDenyOutsideAllow: false,
        fsDenyWithinAllow: false,
        networkPolicy: false,
        networkDomainFilter: false,
      }
  }
}

/**
 * 计算一份保真度报告。**纯函数**：同输入恒同输出，无 I/O、无副作用。
 */
export function computeSandboxFidelity(
  input: SandboxFidelityInput,
): SandboxFidelity {
  const enforcement = platformEnforcement(input.platform)
  const unenforced: string[] = []

  // ── fs：只报「有规则 × 那类规则强制不了」的交集 ──────────────────────
  //
  // 没配规则就没有承诺，也就没有兑现不了的东西 —— 一台什么都没配的 Windows
  // 机器在 fs 这一项上不欠用户任何东西。（实际上 allowWrite 恒含 `.`、
  // denyWrite 恒含 settings.json，所以下面几条在真实会话里几乎必然命中；
  // 但判据写成「按规则是否存在」而不是「按平台直接钉死」，因为规则派生
  // 将来可能变，而那时这里应该跟着变。）
  if (!enforcement.fsAllow) {
    if (input.filesystem.allowRead.length > 0) {
      unenforced.push('filesystem.allowRead')
    }
    if (input.filesystem.allowWrite.length > 0) {
      unenforced.push('filesystem.allowWrite')
    }
  }
  if (!enforcement.fsDenyOutsideAllow) {
    if (input.filesystem.denyRead.length > 0) {
      unenforced.push('filesystem.denyRead')
    }
    if (input.filesystem.denyWrite.length > 0) {
      unenforced.push('filesystem.denyWrite')
    }
  } else if (!enforcement.fsDenyWithinAllow) {
    // Linux/WSL2：deny 表本身是施加了的，只是盖不住已放行的子树。判据是
    // 「deny 表非空」这一位 —— 见文件头〈不复刻 Rust 的重叠判定〉。
    const hasDenyRule =
      input.filesystem.denyRead.length > 0 ||
      input.filesystem.denyWrite.length > 0
    if (hasDenyRule) unenforced.push(FIDELITY_LINUX_DENY_WITHIN_ALLOW)
  }

  // ── network ────────────────────────────────────────────────────────────
  //
  // 域名两表 + 策略级 allowManagedDomainsOnly 由 PR-8 的 loopback 过滤代理判定。
  // 但**代理自己不是强制**：`HTTPS_PROXY` 只劝得动守规矩的客户端，一个直接
  // `socket()` 出去的程序绕得干干净净。让它变成强制的是内核出口锁（把 TCP 出口
  // 锁死在代理端口上 ⇒「不经代理就出不去」＝「代理的判据就是最终判据」）。
  // 两者缺任一都不算强制，所以判据是两位的合取：
  //
  //   1. **代理此刻真的在跑**（`httpProxyPort > 0`）—— 刻意不是「配了域名规则」。
  //      `full` 会被 `isSandboxAutoAllowActive()` 直接换成免审批（PR-5），而代理
  //      起不来时命令照跑（fail-soft，见 `sandboxNetworkProxy.ts` 头注释）。若按
  //      「配了规则」判 full，用户拿到的就是**不过滤 + 不审批**，恰好是最坏的
  //      那一格 —— 比事故前还差一档。
  //   2. **这台机器锁得住出口**（`networkDomainFilter`）。
  //
  // TUI 直连版本在三平台都保守标记域名规则为未完全强制。代理和内核规则仍提供
  // 纵深防御，但在地址绑定与 UDP 出口得到实证前，不把它升级为免审批依据。
  const domainFilterEnforced =
    input.network.httpProxyPort > 0 && enforcement.networkDomainFilter
  if (!domainFilterEnforced) {
    if (input.network.allowedDomains.length > 0) {
      unenforced.push('network.allowedDomains')
    }
    if (input.network.deniedDomains.length > 0) {
      unenforced.push('network.deniedDomains')
    }
    if (input.network.allowManagedDomainsOnly) {
      unenforced.push('network.allowManagedDomainsOnly')
    }
  }
  // 内核三档本身在 Windows 上也没人施加。它恒有值（当前恒 `restricted`），
  // 所以这一条会让 Windows 的报告**结构性地**恒为 partial —— 那正是实话：
  // 那个后端的隔离面里没有任何路径或网络维度。
  if (!enforcement.networkPolicy) {
    unenforced.push('network.policy')
  }

  return {
    level: unenforced.length > 0 ? 'partial' : 'full',
    unenforced,
  }
}
