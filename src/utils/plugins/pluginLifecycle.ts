/**
 * 插件前台生命周期代次（lifecycle generation）sidecar。
 *
 * 问题背景（P0 事故根因 R1）：`inMemoryInstalledPlugins`
 * （installedPluginsManager.ts）是会话级冻结快照——前台安装/卸载/启停成功后
 * 它不失效，长寿命 worker 的 skills/list 与 SkillTool 一直看到安装前的状态。
 * 但「快照冻结」本身又被后台自动更新（pluginAutoupdate.ts / updatePluginOp）
 * 依赖：staged 写盘 + "更新需重启" 语义要求运行中的进程**不**中途换代。
 *
 * 设计不变量（前台 vs 后台 staged）：
 *   * 只有**前台生命周期事件**（安装 / 卸载 / 启停 / 用户显式点击的
 *     plugin/update handler）在 mutation 事务成功后调用
 *     `bumpPluginLifecycleGeneration()`，令 sidecar 代次单调 +1。
 *   * **后台 staged 写**（pluginAutoupdate.ts 周期自动更新与 updatePluginOp
 *     本体）**绝不 bump**——它们只改 registry 文件、不动 sidecar，
 *     `syncPluginRuntimeIfStale()` 因此视其为同代（快照保持冻结，
 *     维持"重启后生效"契约）。
 *   * 各进程只在**安全边界**（turn 开始、list handler 入口）调用
 *     `syncPluginRuntimeIfStale()` 比对代次；active turn 内绝不换代
 *     （`markTurnActive` 闩保护 turn 中途的工具语义一致性）。
 *
 * 锁序契约：`bumpPluginLifecycleGeneration` 自己获取 installed-plugins
 * registry 锁（与 installedPluginsManager.ts::withInstalledPluginsLock
 * 同资源同 targetPath），因此**绝不允许**在已持该锁的 mutation 闭包内调用
 * bump——必须等事务返回成功之后（同资源重入会被 assertNotReentrant 拒绝）。
 *
 * 模块加载零副作用：不读盘、不建目录；一切 IO 在被调用时发生。
 */

import { join } from 'path'
import { withCrossProcessResourceLockSync } from '../crossProcessResourceLock.js'
import { logForDebugging } from '../debug.js'
import { errorMessage, toError } from '../errors.js'
import { writeFileSyncAtomicNoFallback } from '../file.js'
import { getFsImplementation } from '../fsOperations.js'
import { logError } from '../log.js'
import { jsonParse, jsonStringify } from '../slowOperations.js'
import { clearAllCaches } from './cacheUtils.js'
import {
  clearInstalledPluginsCache,
  getInMemoryInstalledPlugins,
  getInstalledPluginsFilePath,
  loadInstalledPluginsV2,
} from './installedPluginsManager.js'
import { getPluginsDirectory } from './pluginDirectories.js'

const LIFECYCLE_GENERATION_FILENAME = 'lifecycle-generation.json'

/**
 * 本进程已加载（已收敛到）的代次。null = 进程启动以来从未同步过——
 * 此时启动快照本就新鲜，首次 sync 只记录基线、不触发刷新。
 */
let loadedGeneration: number | null = null

/**
 * 进程级 active-turn 计数。>0 时 `syncPluginRuntimeIfStale` 直接 no-op：
 * turn 进行中换代会让同一轮内的工具看到不一致的插件面。
 */
let turnActiveCount = 0

/** sidecar 文件路径（与 installed_plugins.json 同目录）。 */
export function getPluginLifecycleGenerationFilePath(): string {
  return join(getPluginsDirectory(), LIFECYCLE_GENERATION_FILENAME)
}

/**
 * 读当前盘上代次。纯读不写。
 *
 * 判定顺序：
 *   * 文件缺失（读+stat 均失败）→ 0（从未有过前台事件的合法初始态）；
 *   * JSON 或 generation 字段非法 → 用文件 mtimeMs 兜底（文件确实被写过，
 *     内容坏了也必须让比对方察觉"变了"；stat 失败 → 0）。
 */
export function readPluginLifecycleGeneration(): number {
  const fs = getFsImplementation()
  const filePath = getPluginLifecycleGenerationFilePath()

  try {
    const content = fs.readFileSync(filePath, { encoding: 'utf-8' })
    const parsed = jsonParse(content) as { generation?: unknown } | null
    const generation = parsed?.generation
    if (
      typeof generation === 'number' &&
      Number.isFinite(generation) &&
      generation >= 0
    ) {
      return generation
    }
  } catch {
    // 读失败（含 ENOENT）与解析失败共用下方 mtime 兜底；ENOENT 时 stat
    // 一样失败，最终落 0。
  }

  try {
    return fs.statSync(filePath).mtimeMs
  } catch {
    return 0
  }
}

/**
 * 前台生命周期事件成功后调用：sidecar 代次单调 +1，随后（锁外）刷新本进程
 * 运行时状态。
 *
 * fail-soft：任何失败只 logError 不抛——bump 丢失的代价是其它进程晚一拍
 * 收敛（下一个前台事件补上），而抛错会炸掉已经成功的安装/卸载主流程。
 * 本进程自身的收敛不依赖 sidecar 写成功：refresh 无条件执行。
 */
export function bumpPluginLifecycleGeneration(): void {
  try {
    // 与 installedPluginsManager.ts::withInstalledPluginsLock 同名同参：
    // bump 与 registry 写共享同一把跨进程锁，串行化并发 bump 的
    // read-increment-write（绝不在已持该锁时调用本函数）。
    withCrossProcessResourceLockSync(
      'installed-plugins-registry',
      () => {
        const current = readPluginLifecycleGeneration()
        // 非法/缺失读值按 max(0, 当前读值) 起步 +1，保持单调。
        const next = Math.max(0, current) + 1
        const fs = getFsImplementation()
        fs.mkdirSync(getPluginsDirectory())
        writeFileSyncAtomicNoFallback(
          getPluginLifecycleGenerationFilePath(),
          `${jsonStringify(
            { generation: next, updatedAt: new Date().toISOString() },
            null,
            2,
          )}\n`,
          { encoding: 'utf-8', mode: 0o600 },
        )
        logForDebugging(`Bumped plugin lifecycle generation to ${next}`)
      },
      { targetPath: getInstalledPluginsFilePath(), syncAttempts: 128 },
    )
  } catch (error) {
    logError(toError(error))
    logForDebugging(
      `Failed to bump plugin lifecycle generation (other processes converge on the next bump): ${errorMessage(error)}`,
      { level: 'warn' },
    )
  }

  // 锁外：本进程立即收敛（内含 clearAllCaches + 快照重建 + 代次登记）。
  refreshPluginRuntimeState()
}

/**
 * 全量刷新本进程的插件运行时状态：清 installed 两层缓存（含 health）→
 * 从盘重建 registry 缓存与会话快照 → 清全部派生缓存 → 登记已加载代次。
 *
 * 代次在**刷新开始前**读取：若另一进程恰在我们重读盘期间 bump，登记的是
 * 旧代次，下一次 sync 仍能察觉 stale 并再刷一遍（保守方向，绝不漏收敛）。
 */
export function refreshPluginRuntimeState(): void {
  const generationBeforeRefresh = readPluginLifecycleGeneration()
  clearInstalledPluginsCache()
  loadInstalledPluginsV2()
  getInMemoryInstalledPlugins()
  clearAllCaches()
  loadedGeneration = generationBeforeRefresh
  logForDebugging(
    `Refreshed plugin runtime state at lifecycle generation ${generationBeforeRefresh}`,
  )
}

/**
 * 安全边界收敛入口（turn 开始、list handler 入口调用）：
 *   * active turn 挂起（计数 >0）→ 直接 return（turn 内绝不换代）；
 *   * 进程从未同步过（loadedGeneration === null）→ **无条件全量收敛**。
 *     只记基线不刷新会留洞：[启动快照读取 → 首个安全边界] 窗口内他进程
 *     bump 过的话，新代次被直接记成基线、刷新被跳过，本进程要等下一个
 *     前台事件才收敛（毁灭性复核抓获）。首边界时派生缓存本就基本是冷的，
 *     全量收敛成本≈读一个 KB 级 registry 文件；语义上等价于"进程晚一点
 *     启动"，不破坏后台 staged 冻结契约（冻结指 mid-session 不因后台写
 *     换代，不指首边界不得对齐盘上态）；
 *   * 盘上代次 ≠ 已加载代次 → 全量刷新。
 */
export function syncPluginRuntimeIfStale(): void {
  if (turnActiveCount > 0) return
  if (
    loadedGeneration === null ||
    readPluginLifecycleGeneration() !== loadedGeneration
  ) {
    refreshPluginRuntimeState()
  }
}

/** turn 进入：挂起换代闩（可重入，计数）。 */
export function markTurnActive(): void {
  turnActiveCount += 1
}

/** turn 退出：释放换代闩（下限 0，防未配对 dec 把计数拉负）。 */
export function markTurnIdle(): void {
  turnActiveCount = Math.max(0, turnActiveCount - 1)
}

/** 仅测试用：复位进程级状态（已加载代次 + turn 计数）。 */
export function __resetPluginLifecycleForTests(): void {
  loadedGeneration = null
  turnActiveCount = 0
}
