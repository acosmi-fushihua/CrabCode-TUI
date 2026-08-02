/**
 * Effective plugin installation resolver — 按「请求目标 cwd」回答安装态的
 * 唯一真源（Wave D，P0 事故根因 R2 根修）。
 *
 * 为什么存在：`plugin/list` / `skills/list` 是投影（projection）面 ——
 * Work 工作区把自己的 cwd 放进请求，但旧实现校验完 `cwds` 就丢弃，安装态
 * 一律用 control worker 自己的进程 cwd（`getOriginalCwd()`）判 project/local
 * 相关性。control worker 是 cwd-independent 的进程级单例，它的 cwd 和用户
 * 正在看的工作区毫无关系 → 插件页与 Work 各说各话。投影必须按请求目标 cwd
 * 作答，不能用进程 cwd 猜 —— 本模块把 `installedPluginsManager.ts` 里按
 * 进程 cwd 写死的三件事参数化：
 *
 *   1. 相关性判定（`isInstallationRelevantToCurrentProject` 的 cwd 参数化版）
 *   2. 生效安装记录选择（`marketplaceLoader.ts` 内联优先级链的提取：
 *      local > project > user > managed，前两者按精确 cwd）
 *   3. enabledPlugins 合并视图（user < 目标 cwd 项目层 < 目标 cwd local 层）
 *
 * 精确匹配铁律：project/local scope 的 `projectPath` 与目标 cwd 必须
 * `pathsEqual(resolve(a), resolve(b))` 级别的**精确路径相等**。绝不引入
 * 祖先目录匹配（projectPath=/a 对 targetCwd=/a/b 就是不相关）——scope 语义
 * 是「装在这个项目」，不是「装在这棵目录树」。
 *
 * 纯函数边界：除读 settings / registry 文件外无副作用；不写盘、不改缓存。
 * 目标 cwd 的项目层 settings 用裸读（`getFsImplementation().readFileSync`
 * + `jsonParse`），**不走** settings.ts 的进程缓存 —— 那套缓存按进程 cwd
 * 键（`getSettingsRootPathForSource('projectSettings')` = `getOriginalCwd()`），
 * 对任意目标 cwd 无效。user 层与 cwd 无关，走 `getSettingsForSource`
 * 的常规缓存即可。
 */

import { join, resolve } from 'path'
import { pathsEqual } from '../file.js'
import { getFsImplementation } from '../fsOperations.js'
import { getSettingsForSource } from '../settings/settings.js'
import { jsonParse } from '../slowOperations.js'
import { loadInstalledPluginsV2 } from './installedPluginsManager.js'
import type {
  InstalledPluginsFileV2,
  PluginInstallationEntry,
} from './schemas.js'

/**
 * Predicate: is this installation relevant to the given target cwd?
 *
 * 语义 = `installedPluginsManager.ts::isInstallationRelevantToCurrentProject`
 * 的 cwd 参数化版：user/managed 恒真（全局）；project/local 只有 projectPath
 * 与 targetCwd 精确相等才相关。两侧先 `resolve` 再 `pathsEqual`（后者只做
 * separator/大小写归一，不做相对→绝对）。
 */
export function isInstallationRelevantToCwd(
  inst: PluginInstallationEntry,
  targetCwd: string,
): boolean {
  return (
    inst.scope === 'user' ||
    inst.scope === 'managed' ||
    (inst.projectPath !== undefined &&
      pathsEqual(resolve(inst.projectPath), resolve(targetCwd)))
  )
}

/**
 * Pick the effective installation entry for a plugin at the given target cwd.
 *
 * 优先级链与 `marketplaceLoader.ts` 加载时的内联选择完全一致（该处现已改为
 * 调本函数）：local（精确 cwd）→ project（精确 cwd）→ user → managed。
 * 找不到任何相关记录 → null。
 */
export function resolveEffectivePluginInstallation(
  pluginId: string,
  targetCwd: string,
  data: InstalledPluginsFileV2,
): PluginInstallationEntry | null {
  const installations = data.plugins[pluginId] ?? []
  const target = resolve(targetCwd)
  return (
    installations.find(
      (installation) =>
        installation.scope === 'local' &&
        installation.projectPath !== undefined &&
        pathsEqual(resolve(installation.projectPath), target),
    ) ??
    installations.find(
      (installation) =>
        installation.scope === 'project' &&
        installation.projectPath !== undefined &&
        pathsEqual(resolve(installation.projectPath), target),
    ) ??
    installations.find((installation) => installation.scope === 'user') ??
    installations.find((installation) => installation.scope === 'managed') ??
    null
  )
}

/**
 * Merged `enabledPlugins` view for a target cwd (projection read only).
 *
 * 覆盖序（后者赢）：user settings → `<targetCwd>/.crabcode/settings.json`
 * → `<targetCwd>/.crabcode/settings.local.json`。项目两层裸读 + jsonParse，
 * 文件缺失 / JSON 损坏 / 形状不对 → 该层视为空（fail-soft —— 这里只是
 * 投影读，坏层不应放大成整个 list 失败；registry 侧的 fail-loud 由
 * Wave A 的 health 闸门负责）。
 */
export function readEnabledPluginsForCwd(
  targetCwd: string,
): Record<string, unknown> {
  const merged: Record<string, unknown> = {}
  try {
    const user = getSettingsForSource('userSettings')
    const userEnabled = user?.enabledPlugins
    if (userEnabled && typeof userEnabled === 'object') {
      Object.assign(merged, userEnabled)
    }
  } catch {
    // fail-soft：user 层不可读 → 视为空层，项目层仍可作答
  }
  for (const fileName of ['settings.json', 'settings.local.json']) {
    Object.assign(
      merged,
      readEnabledPluginsFileRaw(join(targetCwd, '.crabcode', fileName)),
    )
  }
  return merged
}

/**
 * 裸读单个 settings 文件的 enabledPlugins 层。不走 settings.ts 进程缓存
 * （模块头注释的缓存键理由）；任何失败都返回空层。
 */
function readEnabledPluginsFileRaw(
  filePath: string,
): Record<string, unknown> {
  try {
    const content = getFsImplementation().readFileSync(filePath, {
      encoding: 'utf-8',
    })
    const parsed: unknown = jsonParse(content)
    if (parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)) {
      const enabled = (parsed as Record<string, unknown>).enabledPlugins
      if (
        enabled !== null &&
        typeof enabled === 'object' &&
        !Array.isArray(enabled)
      ) {
        return enabled as Record<string, unknown>
      }
    }
  } catch {
    // 文件缺失 / 解析失败 → 空层
  }
  return {}
}

/**
 * Check if a plugin is installed in a way relevant to the target cwd.
 *
 * 语义对齐 `installedPluginsManager.ts::isPluginInstalled` 但 cwd 参数化：
 * registry（installed_plugins.json v2）里有任一 `isInstallationRelevantToCwd`
 * 的记录，**且**目标 cwd 的 enabledPlugins 合并视图里该 key 存在
 * （`!== undefined`；值为 false = 已装但禁用，仍算 installed —— 与
 * `isPluginInstalled` 的 settings 分歧卫兵同判据）。
 */
export function isPluginInstalledForCwd(
  pluginId: string,
  targetCwd: string,
): boolean {
  const v2Data = loadInstalledPluginsV2()
  const installations = v2Data.plugins[pluginId]
  if (!installations || installations.length === 0) {
    return false
  }
  if (
    !installations.some((inst) => isInstallationRelevantToCwd(inst, targetCwd))
  ) {
    return false
  }
  return readEnabledPluginsForCwd(targetCwd)[pluginId] !== undefined
}
