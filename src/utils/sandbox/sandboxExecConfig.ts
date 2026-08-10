/**
 * `buildSandboxExecConfig()` —— 沙箱执行配置的**唯一**出境口
 * （W-SANDBOX-ENFORCED-DEADCODE PR-1）。
 *
 * 事故形态（SoT §1.5「配置保真度缺口」）：TS 侧
 * `convertToSandboxRuntimeConfig` 明明派生了全量隔离规则（network
 * allowed/deniedDomains、fs 四表、#29316 bare-repo 防御、`.crabcode/skills`
 * denyWrite、worktree 主仓 allowWrite），**却一个字节都不出进程**；Rust 端
 * `handle_spawn_managed_sandboxed` 写死 `L1Allowlist + mounts:vec![] +
 * network_policy:None`。「Rust 每次 spawnManaged 从 settings 派生 config」这句
 * 注释宣称的事**从未发生**——一旦把执行链接通，用户拿到的就是**假隔离**。
 *
 * 本模块就是那条缺失的管道的 TS 半边：**派生 → stdin 单次写入 → 交给 helper**。
 * 另一半在 `crates/acosmi-sandbox/src/exec_config.rs`（解析 + 映射）与
 * `crates/crabcode-cli/src/sandbox_exec.rs`（有界读取 + fail-closed）。
 *
 * ## 两条设计铁律
 *
 * 1. **不重新发明规则** —— 派生全量复用 `convertToSandboxRuntimeConfig`。本模块
 *    只补 per-command 的三件事：cwd 探针文件放行、tmp 目录存在性 + `tmpDir`
 *    声明、以及保真度报告。第二套派生逻辑就是第二个漂移面。
 * 2. **所有安全语义字段必填** —— v1 wire schema 里**没有 optional**。Rust 端
 *    `#[serde(deny_unknown_fields)]` + 全字段必填 ⇒ 少一个字段就拒绝启动。
 *    这是 E1「可选安全字段链式失活」的根治面：`Option<T>` + `unwrap_or_default`
 *    的组合会让一条规则悄悄退化成「没这条规则」，而调用方毫无察觉。
 *
 * ## env 为什么不进配置文件
 *
 * 命令的环境变量走**进程继承**通道（`spawn(helper, argv, {env: spawnEnv})`，
 * helper `execvp` 后原样带给命令），不进配置文件。两条通道职责分明：**配置文件
 * 只放隔离规则，进程继承只放运行环境**。混在一起的代价是双份真源——helper 得决定
 * 文件里的 env 和自己继承到的 env 谁赢，而这个决定没有正确答案。
 * 唯一例外是 `TMPDIR`：它由 `tmpDir` 字段声明、helper 侧设置（PR-2/PR-3），
 * 因为它是**隔离规则的一部分**（tmp 必须落在放行目录内），不是用户环境。
 *
 * ## 与 wire 合同的同步义务
 *
 * `SandboxExecConfigV1` 的字段增删必须**同 PR 三处同步**：
 *   1. 本文件的类型与构造
 *   2. `tests/fixtures/sandbox-exec-config-v1.json`（跨语言 schema 闸门）
 *   3. `crates/acosmi-sandbox/src/exec_config.rs::SandboxExecConfigV1`
 * 漏任一处，`sandbox-exec-config.test.ts` 的 schema 闸门或 Rust 侧
 * `fixture_deserializes_and_maps` 必红。
 */

import { randomBytes } from 'node:crypto'
import { lstat, mkdir, realpath, rm } from 'node:fs/promises'
import { basename, dirname, resolve } from 'node:path'
import { join as posixJoin } from 'node:path/posix'

import { getCrabCodeTempDir, getCrabCodeTempDirName } from '../permissions/filesystem.js'
import { getPlatform } from '../platform.js'
import { getInitialSettings } from '../settings/settings.js'
import { computeSandboxFidelity, type SandboxFidelity } from './fidelity.js'
import {
  convertToSandboxRuntimeConfig,
  shouldAllowManagedSandboxDomainsOnly,
} from './sandbox-adapter.js'
import {
  deriveDomainFilterRules,
  ensureSandboxFilteringProxy,
} from './sandboxNetworkProxy.js'

// 保真度类型的真源在 `./fidelity.ts`（判据要被权限层复用，不能长在 wire 模块
// 里）；这里 re-export 是为了让「配置文档的字段类型」仍能从配置模块一处读全。
export type { SandboxFidelity }

/**
 * 配置文件 wire 版本。Rust 端**精确相等**校验，不匹配即拒绝启动
 * （版本兼容矩阵是新的漂移面；同 release 齐发的两半没有兼容需求）。
 */
export const SANDBOX_EXEC_CONFIG_VERSION = 1

/** fs 四表。语义与 `SandboxRuntimeConfig.filesystem` 逐字一致。 */
export type SandboxExecFilesystemRules = {
  allowRead: string[]
  allowWrite: string[]
  denyRead: string[]
  denyWrite: string[]
}

export type SandboxExecNetworkRules = {
  /** 内核三档。见 `config.rs::NetworkPolicy`。 */
  policy: 'none' | 'restricted' | 'host'
  allowedDomains: string[]
  deniedDomains: string[]
  allowUnixSockets: string[]
  allowAllUnixSockets: boolean
  allowLocalBinding: boolean
  /** 0 = 未分配（无域名规则 / 代理没起来）；代理在跑时由 `sandboxNetworkProxy.ts`
   *  在运行时填入其 loopback 端口（`deriveSandboxExecConfig` 的 override）。 */
  httpProxyPort: number
  /** 恒 0。SOCKS 面未实现（HTTP CONNECT 覆盖 curl/git/gh/npm/pip/bun）。 */
  socksProxyPort: number
}

/** 用户显式要求的**放宽**开关。两者都是「更弱」方向，故必须显式下传。 */
export type SandboxExecWeakerFlags = {
  nestedSandbox: boolean
  networkIsolation: boolean
}

/**
 * v1 wire schema。**全字段必填**（见文件头铁律 2）。
 * 字段名即 Rust 侧 camelCase serde 名，两边逐字对应。
 */
export type SandboxExecConfigV1 = {
  configVersion: number
  fidelity: SandboxFidelity
  /**
   * 隔离档位。当前恒 `allowlist`（= `SecurityLevel::L1Allowlist`，与事故前
   * 旧实现固定的档位一致）。settings 里**没有**暴露档位选择，所以这里没有
   * 可派生的输入；但仍然写进配置而不是让 Rust 硬编码——helper 不猜策略，策略
   * 全部由 TS 说了算（SoT §2.4 权限单脑）。
   */
  securityLevel: 'deny' | 'allowlist' | 'sandboxed'
  /** 本条命令的工作目录（绝对路径）。fs 规则里的 `.` 以它为基准解析。 */
  cwd: string
  /** 沙箱内 `TMPDIR` 指向的目录。已在 `filesystem.allowWrite` 内。 */
  tmpDir: string
  filesystem: SandboxExecFilesystemRules
  network: SandboxExecNetworkRules
  weaker: SandboxExecWeakerFlags
}

export type BuildSandboxExecConfigOptions = {
  /** 本条命令的 cwd（`Shell.ts::exec` 里 realpath 校验过的那个）。 */
  cwd: string
  /**
   * cwd 探针文件路径（`provider.buildExecCommand` 的 `cwdFilePath`）。
   *
   * 它由 provider 在**决定用不用沙箱之前**就烘进了命令串，所以本函数只能接收、
   * 不能自己生成——生成职责若搬进来，就得把已经拼好的命令串再改一遍。
   * 命令跑完靠写这个文件把 cwd 传播回宿主；不放行 = cd 传播静默失效。
   */
  cwdFile: string
  /** Host-resolved, per-command temp directory. */
  tmpDir?: string
  /** Equivalent lexical spellings (notably `/tmp` vs `/private/tmp`). */
  tmpDirAliases?: readonly string[]
  /**
   * PR-8 的 live 过滤代理端口；> 0 时覆盖 settings 里的那个值。缺省 / 0 ⇒ 回落到
   * settings 来源的端口（实践中仍是 0 —— settings 里没有人会去填一个只有运行时
   * 才知道的临时端口）。
   *
   * 之所以是**覆盖**而不是让本函数自己去起代理：`deriveSandboxExecConfig` 是纯
   * 函数（单测直接断言产物），起 socket 属于副作用，归 {@link buildSandboxExecConfig}。
   */
  httpProxyPortOverride?: number
}

export type BuildSandboxExecConfigResult = {
  /** One-shot JSON written to this helper's stdin, never to a shared path. */
  configJson: string
  /** 保真度报告。调用方（PR-5）据此决定要不要发放 auto-allow 宽免。 */
  fidelity: SandboxFidelity
  /**
   * live 过滤代理端口；无域名规则或代理起不来时为 0。`Shell.ts` 据此给子进程注入
   * `HTTPS_PROXY`（0 ⇒ 什么都不注入）。
   *
   * 与配置文件里的 `network.httpProxyPort` 同值，但**必须单独返回**：配置文件是
   * 给 helper 的（进程外），env 是给命令的（进程内），Shell.ts 拿不到前者的内容。
   */
  networkProxyPort: number
}

const SANDBOX_COMMAND_ID_BYTES = 16
const SANDBOX_COMMAND_ID_PATTERN = /^[0-9a-f]{32}$/

/** A cryptographically strong, 128-bit namespace for one sandbox command. */
export function generateSandboxCommandId(): string {
  return randomBytes(SANDBOX_COMMAND_ID_BYTES).toString('hex')
}

function assertPrivateSandboxCommandTempDir(tmpDir: string): void {
  if (
    basename(dirname(tmpDir)) !== 'sandbox-commands' ||
    !SANDBOX_COMMAND_ID_PATTERN.test(basename(tmpDir))
  ) {
    throw new Error(
      `Refusing unsafe sandbox command temp directory outside a 128-bit private namespace: ${tmpDir}`,
    )
  }
}

/**
 * Create the command-private temp leaf exactly once.
 *
 * `recursive: true` is intentionally limited to the host-owned parent. The
 * leaf itself uses an exclusive mkdir: an existing directory or symlink is a
 * collision/pre-occupation and fails closed instead of being reused.
 */
async function createSandboxCommandTempDir(
  tmpDir: string,
  aliases: readonly string[],
  expectedTempRoot = getCrabCodeTempDir(),
): Promise<void> {
  assertPrivateSandboxCommandTempDir(tmpDir)

  // Validate each host-owned ancestor before creating the leaf. In particular,
  // never let recursive mkdir follow a `sandbox-commands` symlink left by an
  // older sandbox. The system temp base already exists; the CrabCode root and
  // command parent are the only two levels this function may create.
  try {
    await mkdir(expectedTempRoot, { mode: 0o700 })
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'EEXIST') throw error
  }
  const rootStats = await lstat(expectedTempRoot)
  if (!rootStats.isDirectory() || rootStats.isSymbolicLink()) {
    throw new Error(
      `CrabCode temp root is not a host-owned real directory: ${expectedTempRoot}`,
    )
  }

  const expectedParent = resolve(expectedTempRoot, 'sandbox-commands')
  if (resolve(dirname(tmpDir)) !== expectedParent) {
    throw new Error(
      `Sandbox command temp directory escaped its host-owned parent: ${tmpDir}`,
    )
  }
  try {
    await mkdir(expectedParent, { mode: 0o700 })
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'EEXIST') throw error
  }
  const parentStats = await lstat(expectedParent)
  if (!parentStats.isDirectory() || parentStats.isSymbolicLink()) {
    throw new Error(
      `Sandbox command temp parent is not a host-owned real directory: ${expectedParent}`,
    )
  }
  const canonicalRoot = resolve(await realpath(expectedTempRoot))
  const canonicalParent = resolve(await realpath(expectedParent))
  if (canonicalParent !== resolve(canonicalRoot, 'sandbox-commands')) {
    throw new Error(
      `Sandbox command temp parent resolved outside the CrabCode temp root: ${expectedParent}`,
    )
  }

  let created = false
  try {
    await mkdir(tmpDir, { mode: 0o700 })
    created = true

    const stats = await lstat(tmpDir)
    if (!stats.isDirectory() || stats.isSymbolicLink()) {
      throw new Error(
        `Sandbox command temp path is not a host-created directory: ${tmpDir}`,
      )
    }

    if (getPlatform() !== 'windows') {
      const canonicalTmpDir = resolve(await realpath(tmpDir))
      for (const alias of aliases) {
        const canonicalAlias = resolve(await realpath(alias))
        if (canonicalAlias !== canonicalTmpDir) {
          throw new Error(
            `Sandbox command temp alias does not identify the private directory: ${alias}`,
          )
        }
      }
    }
  } catch (error) {
    // Never delete a pre-existing collision. Only unwind the leaf this call
    // proved it created itself.
    if (created) {
      await rm(tmpDir, { recursive: true, force: true })
    }
    throw error
  }
}

/** Test seam for ancestor substitution and exclusive-creation regressions. */
export async function _createSandboxCommandTempDirForTest(
  tmpDir: string,
  aliases: readonly string[],
  expectedTempRoot: string,
): Promise<void> {
  return createSandboxCommandTempDir(tmpDir, aliases, expectedTempRoot)
}

/** Idempotently release a host-created command-private temp directory. */
export async function cleanupSandboxCommandTempDir(
  tmpDir: string,
): Promise<void> {
  assertPrivateSandboxCommandTempDir(tmpDir)
  await rm(tmpDir, {
    recursive: true,
    force: true,
    maxRetries: 3,
    retryDelay: 10,
  })
}

/**
 * Shell.ts 烘进命令串的那个 tmp 目录写法（`/tmp/crabcode-<uid>`，**不**做
 * symlink 解析）。`getCrabCodeTempDir()` 返回的是同一目录 realpath 之后的写法
 * （macOS 上是 `/private/tmp/...`）。landlock / seatbelt 都按路径匹配，两种写法
 * 都得放行，否则 macOS 上 cwd 探针文件必被拒写。
 */
function unresolvedSandboxTmpDir(): string {
  return posixJoin(
    process.env.CRABCODE_TMPDIR || '/tmp',
    getCrabCodeTempDirName(),
  )
}

/** 保序去重——规则顺序是可读性资产，Set 反查是 O(1)。 */
function dedupe(values: readonly string[]): string[] {
  return [...new Set(values)]
}

/**
 * 从当前 settings + 本条命令的上下文派生完整隔离配置。
 *
 * 抽成纯函数（不写盘）以便单测直接断言产物；写盘在
 * {@link buildSandboxExecConfig}。
 */
export function deriveSandboxExecConfig(
  options: BuildSandboxExecConfigOptions,
): SandboxExecConfigV1 {
  // 全量复用既有派生——fs 四表 / 域名 / #29316 bare-repo 防御 /
  // `.crabcode/skills` denyWrite / worktree 主仓放行全在里面。
  const runtime = convertToSandboxRuntimeConfig(getInitialSettings())

  const tmpDir = options.tmpDir ?? getCrabCodeTempDir()
  const allowWrite = dedupe([
    ...(runtime.filesystem?.allowWrite ?? []),
    tmpDir,
    ...(options.tmpDirAliases ?? [unresolvedSandboxTmpDir()]),
    // cwd 探针文件：命令跑完往这里写 `pwd -P`，宿主读回做 cd 传播。
    options.cwdFile,
  ])

  const allowedDomains = runtime.network?.allowedDomains ?? []
  const deniedDomains = runtime.network?.deniedDomains ?? []

  // live 端口优先：settings 里那个值是配置面的占位（恒 0），真正在监听的端口只有
  // 运行时知道（`buildSandboxExecConfig` 把 `ensureSandboxFilteringProxy` 的返回
  // 值当 override 传进来）。**算一次给两个消费者用** —— 下面的 fidelity 判据与
  // wire 字段必须是同一个数：一边说「代理在 5555」另一边按「没有代理」算保真度，
  // 就是同一个事实的两套读法（SoT §4 E5 第二判定脑的微缩版）。
  const effectiveHttpProxyPort =
    options.httpProxyPortOverride && options.httpProxyPortOverride > 0
      ? options.httpProxyPortOverride
      : (runtime.network?.httpProxyPort ?? 0)

  const filesystem: SandboxExecFilesystemRules = {
    allowRead: dedupe(runtime.filesystem?.allowRead ?? []),
    allowWrite,
    denyRead: dedupe(runtime.filesystem?.denyRead ?? []),
    denyWrite: dedupe(runtime.filesystem?.denyWrite ?? []),
  }

  // 刻意不下传的两项，**记在这里**而不是无声丢弃：
  //   - `runtime.ripgrep` —— 旧 sandbox-runtime 用来决定在沙箱里怎么起 rg 的；
  //     新架构下 helper 只 exec 给定命令，不认识 rg，它不是隔离规则。
  //   - `runtime.ignoreViolations` —— 违规**上报**的过滤器（PR-4 反馈环），
  //     同样不是隔离规则。
  return {
    configVersion: SANDBOX_EXEC_CONFIG_VERSION,
    // 平台感知（PR-5 硬前提）：判据在 `./fidelity.ts`，与
    // `sandbox-adapter.ts::getSandboxFidelity()` 的会话级判据共享同一个纯函数
    // —— 两个消费者得出不同结论就是新的漂移面。
    fidelity: computeSandboxFidelity({
      platform: getPlatform(),
      filesystem,
      network: {
        allowedDomains,
        deniedDomains,
        allowManagedDomainsOnly: shouldAllowManagedSandboxDomainsOnly(),
        // per-command 派生**知道真端口**（override 就是这一条命令要用的那个），
        // 所以这份保真度是精确的，不是会话级的近似。
        httpProxyPort: effectiveHttpProxyPort,
      },
    }),
    securityLevel: 'allowlist',
    cwd: options.cwd,
    tmpDir,
    filesystem,
    network: {
      // L1Allowlist 的默认档。settings 未暴露档位选择，故此处没有可派生的输入。
      policy: 'restricted',
      allowedDomains: dedupe(allowedDomains),
      deniedDomains: dedupe(deniedDomains),
      allowUnixSockets: dedupe(runtime.network?.allowUnixSockets ?? []),
      allowAllUnixSockets: runtime.network?.allowAllUnixSockets ?? false,
      allowLocalBinding: runtime.network?.allowLocalBinding ?? false,
      // 非零 ⇒ Rust 侧把 TCP 出口锁死在这个端口上并跑 canary 实证
      // （`platform.rs::verify_egress_locked_to_proxy`）。上面那份 fidelity 读的
      // 是同一个值 —— 「已强制」这句话与「真锁了出口」这件事共用一个数。
      httpProxyPort: effectiveHttpProxyPort,
      socksProxyPort: runtime.network?.socksProxyPort ?? 0,
    },
    weaker: {
      nestedSandbox: runtime.enableWeakerNestedSandbox ?? false,
      networkIsolation: runtime.enableWeakerNetworkIsolation ?? false,
    },
  }
}

/**
 * 沙箱化 = 给同一次 spawn 的 argv 加一段前缀（W-SANDBOX-ENFORCED-DEADCODE PR-2）。
 *
 * 被执行的二进制与它的参数**一个字节都不变**，只是前面多了 helper 和它的两个
 * flag。这正是关联方矩阵（SoT §3）里 15 项能「按构造继承、零改动」的原因：
 * 进程仍由 `Shell.ts` 本地 spawn，`execvp` 之后 helper 把自己换成了那条命令。
 *
 * 抽成纯函数不是洁癖：上一版的路由决策就是因为长在 `Shell.ts` 的表达式里，
 * 测试只能抄一份真值表，结果测的是 JavaScript 的 `&&` 而不是产品行为
 * （FIX-3 复盘，`tests/unit/sandbox-exec-wiring.test.ts` 头注释）。argv 的形状
 * 是 TS↔Rust 之间的合同——`sandbox_exec.rs::parse_sandbox_exec_argv` 只认这**一种**
 * 形态，多一个空格少一个 `--` 都会变成 125 `invalid-argv`。
 */
export function buildSandboxExecArgv(input: {
  /** helper 二进制（`resolveSandboxHelperBin()` 的产物）。 */
  helperBin: string
  /** 未沙箱化时本来要 spawn 的二进制。 */
  program: string
  /** 未沙箱化时本来要传的参数。 */
  args: readonly string[]
}): { binary: string; args: string[] } {
  return {
    binary: input.helperBin,
    args: [
      'sandbox-exec',
      '--config-stdin',
      // `--` 之后的一切都是命令自己的。Rust 侧刻意不再解释它们。
      '--',
      input.program,
      ...input.args,
    ],
  }
}

/**
 * 派生配置 → 返回一次性 stdin payload 与保真度报告。
 *
 * 不落路径是安全边界：已经运行的同 UID 沙箱命令可以写共享 tmp 目录，任何
 * “先写文件、再按路径打开”的方案都有替换窗口。stdin pipe 只属于本次 helper，
 * helper 在 exec 用户命令前有界读完；用户命令看到的 stdin 已是 EOF。
 */
export async function buildSandboxExecConfig(
  options: BuildSandboxExecConfigOptions & { tmpDir: string },
): Promise<BuildSandboxExecConfigResult> {
  // 过滤代理是**进程级单例**（规则来自 settings，不随命令变），这里只是「确保它
  // 起着」并取回 live 端口 —— 常态下是一次同步命中的查表。起不来返回 null，端口
  // 留 0：命令照跑、不注入 proxy env、保真度照常 partial（见 sandboxNetworkProxy.ts
  // 头注释「失败一律降级」）。
  const rules = deriveDomainFilterRules()
  const proxyPort = await ensureSandboxFilteringProxy(rules)

  const config = deriveSandboxExecConfig({
    ...options,
    httpProxyPortOverride: proxyPort ?? undefined,
  })

  // The private leaf is security state, not ordinary scratch setup. Reusing an
  // existing name would let a surviving older sandbox retain write authority
  // over this command's TMPDIR and cwd probe.
  await createSandboxCommandTempDir(
    config.tmpDir,
    options.tmpDirAliases ?? [],
  )

  return {
    configJson: JSON.stringify(config),
    fidelity: config.fidelity,
    networkProxyPort: proxyPort ?? 0,
  }
}
