/**
 * /proxy — 网络代理诊断与声明。W-SYSPROXY-DISCOVERY（2026-07-24 立项审计）。
 *
 * CrabCode 只从环境变量发现代理（`src/utils/proxy.ts::getProxyUrl`）。把代理
 * 配在系统设置面板里的机器因此在应用内直连，用户却以为代理生效，抓取失败只
 * 表现为无解释的网络错误（2026-07-24 根因审计 §7 F3）。
 *
 * P0 把这条暗缝显式化（报告生效值 + 只读探测系统设置 + 打印可粘贴配方）。
 * P1（2026-07-25）补上执行面：`/proxy use-system` 把探测到的系统代理写进
 * **用户级 `settings.env`**，`/proxy off` 撤销。
 *
 * 为什么是"写 settings.env"而不是"自动采纳成第二个代理来源"（P1 前置审计裁定）：
 *   - **子进程一致**：settings.env 经 `applyConfigEnvironmentVariables()` 投影进
 *     `process.env`，Bash 里的 curl / MCP stdio server 与应用走同一条出口；
 *     进程内的"discovered 代理"则永远到不了子进程，造成"应用能抓、curl 不行"。
 *   - **不制造回归**：`ProxyEnable=1` 指向一个已死端口是常见残留态。自动采纳会
 *     让今天直连正常的机器变成全网失败；写入是用户显式动作，不会自己发生。
 *   - **守卫语义不分叉**：写完它就是一个用户自觉声明的 env 代理，
 *     `WebFetchTool/utils.ts` 的 `envProxyActive` 判定无需增加第二种模式。
 *   - **状态可见**：配置落在 `~/.crabcode/settings.json`，可读可改可撤销；
 *     进程内内存态不可见也不可审计。
 *
 * 两条不变的边界：
 *   - **不执行 PAC**：检测到自动配置脚本一律如实告知并要求手工填写；客户端执行
 *     远程 JS 是 RCE 级新面，永久否决。`use-system` 对 PAC 一律拒绝写入。
 *   - **不参与代理发现**：探针只做输入（见 `src/utils/systemProxy.ts` 的硬不变量），
 *     `src/utils/proxy.ts` 永不引用它。
 */
import type { LocalCommandCall, LocalCommandResult } from '../../types/command.js'
import { applyConfigEnvironmentVariables } from '../../utils/managedEnv.js'
import { getNoProxy, getProxyUrl } from '../../utils/proxy.js'
import {
  getSettingsForSource,
  updateSettingsForSource,
} from '../../utils/settings/settings.js'
import {
  probeSystemProxy,
  translateSystemBypass,
  type SystemProxySnapshot,
} from '../../utils/systemProxy.js'

/**
 * The three keys `/proxy use-system` owns. `/proxy off` removes exactly these
 * and nothing else, so the command can never eat an unrelated settings.env
 * entry the user put there by hand.
 */
const MANAGED_ENV_KEYS = ['HTTPS_PROXY', 'HTTP_PROXY', 'NO_PROXY'] as const
type ManagedEnvKey = (typeof MANAGED_ENV_KEYS)[number]

export type ProxyCommandDeps = {
  getProxy: typeof getProxyUrl
  getNoProxyList: typeof getNoProxy
  probe: typeof probeSystemProxy
  readUserEnv: () => Record<string, string> | undefined
  writeSettings: typeof updateSettingsForSource
  reapplyEnv: typeof applyConfigEnvironmentVariables
  processEnv: Record<string, string | undefined>
}

const DEFAULT_DEPS: ProxyCommandDeps = {
  getProxy: getProxyUrl,
  getNoProxyList: getNoProxy,
  probe: probeSystemProxy,
  readUserEnv: () => getSettingsForSource('userSettings')?.env,
  writeSettings: updateSettingsForSource,
  reapplyEnv: applyConfigEnvironmentVariables,
  processEnv: process.env,
}

const USAGE =
  '用法：/proxy（查看）· /proxy use-system（把系统代理写进用户级设置）· /proxy off（撤销写入）'

/**
 * Re-project settings.env into this process (clears the proxy agent cache and
 * reinstalls the global agents — the same path onChangeAppState takes).
 *
 * Returns a warning line instead of throwing: the settings write has already
 * succeeded by this point, and letting a reconfigure failure surface as a
 * command crash would hide that from the user.
 */
function reapply(deps: ProxyCommandDeps): string | undefined {
  try {
    deps.reapplyEnv()
    return undefined
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error)
    return `注意：设置已落盘，但本进程重新应用配置失败（${detail}）——重启 CrabCode 后生效。`
  }
}

/** Managed keys currently present in user-level settings.env. */
function managedKeysInSettings(deps: ProxyCommandDeps): ManagedEnvKey[] {
  const env = deps.readUserEnv()
  if (!env) return []
  return MANAGED_ENV_KEYS.filter(key => env[key] !== undefined)
}

function effectiveSection(
  proxy: string | undefined,
  deps: ProxyCommandDeps,
): string[] {
  const managed = managedKeysInSettings(deps)
  if (!proxy) {
    const lines = ['当前生效代理：未配置（应用直连）']
    if (managed.length > 0) {
      lines.push(
        `注意：用户级 settings.env 里有 ${managed.join(' / ')}，但当前进程未生效——通常是写入后尚未重启，或被更高优先级的设置层覆盖。`,
      )
    }
    return lines
  }
  const lines = [`当前生效代理：${proxy}`]
  const noProxy = deps.getNoProxyList()
  lines.push(`NO_PROXY：${noProxy ? noProxy : '未设置'}`)
  // 生效值一律取自 process.env。用户级 settings.env 是可辨的那一半来源（本命令
  // 写的就是它）；其余来自外部 shell，运行时不可区分，故不谎称来源。
  lines.push(
    managed.length > 0
      ? `来源：用户级 settings.env（含 ${managed.join(' / ')}，由 /proxy use-system 写入或手工添加）；撤销：/proxy off`
      : '来源：进程环境变量（外部 shell 导出，本命令未参与）',
  )
  return lines
}

function systemSection(snapshot: SystemProxySnapshot): string[] {
  switch (snapshot.kind) {
    case 'static': {
      const lines = [`系统代理探测（只读）：已配置 → ${snapshot.url}`]
      if (snapshot.bypass) {
        lines.push(`系统绕过列表：${snapshot.bypass}`)
      }
      return lines
    }
    case 'pac':
      return [
        `系统代理探测（只读）：检测到自动配置脚本（PAC）→ ${snapshot.pacUrl}`,
        'CrabCode 不执行 PAC 脚本（客户端执行远程脚本的风险不可接受）。请在系统设置里查看该脚本实际指向的代理地址后手工填写。',
      ]
    case 'none':
      return [
        '系统代理探测（只读）：未配置',
        '若你的代理客户端工作在 TUN / 虚拟网卡模式，流量在网络层被接管，应用无需额外配置即可联网。',
      ]
    case 'unsupported-platform':
      return [
        `系统代理探测（只读）：当前平台（${snapshot.platform}）没有统一的系统代理真源，已跳过探测。`,
      ]
    case 'probe-failed':
      return [
        '系统代理探测（只读）：读取失败（超时、无权限或输出无法解析）。系统繁忙时探测子进程可能超时，可重试一次；仍失败请手工确认系统设置。',
      ]
  }
}

function manualRecipeSection(url: string, noProxy: string): string[] {
  return [
    '若你更想手工管理，把下面这段并入 `~/.crabcode/settings.json` 的 `env` 字段，效果与 /proxy use-system 完全一致：',
    '',
    '{',
    '  "env": {',
    `    "HTTPS_PROXY": "${url}",`,
    `    "HTTP_PROXY": "${url}",`,
    `    "NO_PROXY": "${noProxy}"`,
    '  }',
    '}',
  ]
}

function statusHintSection(
  effective: string | undefined,
  snapshot: SystemProxySnapshot,
): string[] {
  if (effective || snapshot.kind !== 'static') return []
  const { noProxy } = translateSystemBypass(snapshot.bypass)
  return [
    '建议：系统已配代理，但应用当前直连。执行 `/proxy use-system` 即把它写进用户级设置并立即生效（应用与子进程一致）。',
    '',
    ...manualRecipeSection(snapshot.url, noProxy),
  ]
}

/** Every snapshot that cannot yield an address to adopt. */
type UnusableSnapshot = Exclude<SystemProxySnapshot, { kind: 'static' }>

function refuseUseSystem(snapshot: UnusableSnapshot): string {
  switch (snapshot.kind) {
    case 'pac':
      return (
        `未写入：系统用的是自动配置脚本（PAC → ${snapshot.pacUrl}）。` +
        'CrabCode 不执行 PAC 脚本，无法从中推断出一个确定的代理地址；' +
        '请在系统设置或该脚本里确认实际代理地址后，手工写进 settings.env。'
      )
    case 'none':
      return (
        '未写入：系统代理未配置，没有可采纳的地址。' +
        '若你的代理客户端工作在 TUN / 虚拟网卡模式，本来就不需要配置。'
      )
    case 'unsupported-platform':
      return `未写入：当前平台（${snapshot.platform}）没有统一的系统代理真源，无法探测；请手工写进 settings.env。`
    case 'probe-failed':
      return (
        '未写入：系统代理读取失败（超时、无权限或输出无法解析），不拿不确定的值写你的设置。' +
        '系统繁忙时探测子进程可能超时——请重试一次；仍失败请手工确认后填写。'
      )
  }
}

async function useSystemText(deps: ProxyCommandDeps): Promise<string> {
  const snapshot = await deps.probe()
  if (snapshot.kind !== 'static') return refuseUseSystem(snapshot)

  const { noProxy, dropped } = translateSystemBypass(snapshot.bypass)
  const next: Record<ManagedEnvKey, string> = {
    HTTPS_PROXY: snapshot.url,
    HTTP_PROXY: snapshot.url,
    NO_PROXY: noProxy,
  }
  const previous = deps.readUserEnv() ?? {}
  // 只报"值真的变了"的键：重复执行 use-system 不该谎称覆盖了什么。
  const overwritten = MANAGED_ENV_KEYS.filter(
    key => previous[key] !== undefined && previous[key] !== next[key],
  )

  const result = deps.writeSettings('userSettings', { env: next })
  if (result.error) {
    return `写入失败：${result.error.message}（设置未变更）`
  }
  // 立即生效：把 settings.env 投影进 process.env，清代理 agent 缓存并重装
  // 全局 agent。与设置变更时 onChangeAppState 走的是同一条再配置路径。
  const reapplyWarning = reapply(deps)

  const lines = [
    reapplyWarning
      ? '已写入用户级设置 `~/.crabcode/settings.json` 的 env 字段：'
      : '已写入用户级设置 `~/.crabcode/settings.json` 的 env 字段并立即生效：',
    `  HTTPS_PROXY / HTTP_PROXY = ${snapshot.url}`,
    `  NO_PROXY = ${noProxy}`,
  ]
  if (overwritten.length > 0) {
    const before = overwritten
      .map(key => `${key}=${previous[key]}`)
      .join('，')
    lines.push(`（覆盖了原有的 ${before}）`)
  }
  if (dropped.length > 0) {
    lines.push(
      `系统绕过列表里有 ${dropped.length} 项无法折算成 NO_PROXY 语法，已跳过：${dropped.join('、')}`,
      '（NO_PROXY 只支持精确主机名、IP、host:port 和 .后缀；通配段、网段与 <local> 无对应写法。这些目标此后会走代理，如需保留请手工细化 NO_PROXY。）',
    )
  }
  if (reapplyWarning) lines.push(reapplyWarning)
  lines.push(
    '此后应用与子进程（Bash 里的 curl、MCP 服务）走同一条出口；WebFetch 的连接期地址守卫会交由代理判定，与手工声明代理时一致。',
    '撤销：/proxy off',
  )
  return lines.join('\n')
}

function offText(deps: ProxyCommandDeps): string {
  const present = managedKeysInSettings(deps)
  // NO_PROXY on its own is not a proxy declaration — a user may keep a bypass
  // list for reasons of their own. "Turn the proxy off" must not eat it, so
  // revocation only triggers when an actual proxy key is present.
  const hasProxyKey = present.some(
    key => key === 'HTTPS_PROXY' || key === 'HTTP_PROXY',
  )
  if (!hasProxyKey) {
    const lines = ['用户级 settings.env 里没有 HTTPS_PROXY / HTTP_PROXY，无需撤销。']
    if (present.includes('NO_PROXY')) {
      lines.push('（其中的 NO_PROXY 是独立的绕过列表，不属代理声明，已原样保留。）')
    }
    if (deps.getProxy()) {
      lines.push(
        '注意：当前仍有生效代理，它来自外部环境变量（本命令不管理），请在 shell 侧取消。',
      )
    }
    return lines.join('\n')
  }
  const result = deps.writeSettings('userSettings', {
    // undefined = 删除该键（settings.ts 的 mergeWith customizer 契约）。
    env: Object.fromEntries(
      present.map(key => [key, undefined]),
    ) as unknown as Record<string, string>,
  })
  if (result.error) {
    return `撤销写入失败：${result.error.message}（设置未变更）`
  }
  // 设置层删除不会自己回收 process.env——投影是 Object.assign，只写不删。
  // 不在本进程同步清除，代理会一直活到重启，那就是个半状态。
  for (const key of present) {
    delete deps.processEnv[key]
  }
  const reapplyWarning = reapply(deps)

  const lines = [
    `已从用户级设置移除 ${present.join(' / ')}，并在本进程清除同名环境变量、重新应用其余配置层。`,
  ]
  if (reapplyWarning) lines.push(reapplyWarning)
  const remaining = deps.getProxy()
  lines.push(
    remaining
      ? `注意：仍有生效代理 ${remaining}——它来自其它配置层或外部 shell，本命令不管理。`
      : '应用已回到直连。',
  )
  return lines.join('\n')
}

async function statusText(deps: ProxyCommandDeps): Promise<string> {
  const effective = deps.getProxy()
  const snapshot = await deps.probe()
  const sections: string[][] = [
    effectiveSection(effective, deps),
    systemSection(snapshot),
  ]
  const hint = statusHintSection(effective, snapshot)
  if (hint.length > 0) sections.push(hint)
  sections.push([USAGE])
  return sections.map(section => section.join('\n')).join('\n\n')
}

export async function proxyCommand(
  args: string,
  deps: ProxyCommandDeps = DEFAULT_DEPS,
): Promise<LocalCommandResult> {
  const action = args.trim().toLowerCase()
  if (action === '') return { type: 'text', value: await statusText(deps) }
  if (action === 'use-system') {
    return { type: 'text', value: await useSystemText(deps) }
  }
  if (action === 'off') return { type: 'text', value: offText(deps) }
  return {
    type: 'text',
    value: `无法识别的参数「${args.trim()}」。${USAGE}`,
  }
}

export const call: LocalCommandCall = async args => proxyCommand(args)
