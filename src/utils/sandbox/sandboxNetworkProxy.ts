/**
 * 过滤代理的**生命周期与接线**（W-SANDBOX-ENFORCED-DEADCODE PR-8 Slice 2）。
 *
 * Slice 1 交付了判据（`networkFilter.ts`）与管道（`filteringProxy.ts`）两块砖，
 * 但没有人去起它、没有人告诉沙箱里的命令「代理在哪」。本模块补的就是这两件：
 *
 *   1. **进程级单例** —— 规则来自 settings，settings 不随命令变，所以代理也不该
 *      随命令起落。per-command 起一个代理意味着每条 `curl` 都要付一次 listen +
 *      close 的钱，还会在高并发命令下把 loopback 端口攒成一片。
 *   2. **child env 注入** —— `HTTPS_PROXY` / `HTTP_PROXY` 让守规矩的客户端
 *      （curl / git / node / python-requests / bun）自己走进代理。
 *
 * ## 这里只是**接线**；「强制」是 Slice 3 的内核出口锁给的
 *
 * env 注入只对「读 proxy 环境变量」的客户端有效，一个直接 `socket()` 出去的程序
 * 完全绕得过去。所以本模块自己**不主张任何强制**：Slice 2 交付时 `fidelity.ts`
 * 里域名规则照标 `unenforced`。Slice 3 之后 Seatbelt / Landlock 会把 TCP 出口锁死
 * 在这个端口上（helper 侧还有 canary 实证），此时「不经代理就出不去」，代理的判据
 * 才升格成最终判据 —— Slice 4 因此用 {@link currentSandboxFilteringProxyPort} 把
 * 「代理此刻在不在跑」喂进保真度判据。**判据看的是端口而不是规则**：没跑起来就
 * 什么都没强制，宣称已强制正是本立项要根治的那种谎（SoT §4 E5「假装强制」）。
 *
 * ## 失败一律降级成 null，绝不抛
 *
 * 起代理失败（端口耗尽、EMFILE、平台异常）返回 `null` ⇒ 调用方把
 * `httpProxyPort` 留在 0 ⇒ 不注入任何 proxy env ⇒ 命令照常跑、保真度照常 partial。
 * 反过来（抛出去）等于**让一个尽力而为的过滤面把用户的 shell 命令打死**，而它
 * 本来就没承诺过强制。fail-closed 用在能兑现的承诺上；这里没有那份承诺。
 */

import { registerCleanup } from '../cleanupRegistry.js'
import { logForDebugging } from '../debug.js'
import { getInitialSettings } from '../settings/settings.js'
import {
  startFilteringProxy,
  type FilteringProxyHandle,
} from './filteringProxy.js'
import type { DomainFilterRules } from './networkFilter.js'
import {
  convertToSandboxRuntimeConfig,
  shouldAllowManagedSandboxDomainsOnly,
} from './sandbox-adapter.js'

/**
 * 沙箱子进程的 `NO_PROXY` —— **只有 loopback**。
 *
 * 这里每加一个真实域名，就等于给它开一条不过滤的直通道：客户端看到
 * `NO_PROXY` 命中就根本不连代理，判据函数一次都不会被调用。所以哪怕是
 * 「我们自己的」域名（更新源、registry、网关）也不进这张表 —— 一条 allowlist
 * 里凭直觉挖出来的洞，正是 allowlist 存在的理由。
 *
 * loopback 是唯一例外，且它不是让步而是**必需**：代理自己就住在 127.0.0.1，
 * 不豁免 loopback 会让访问代理的请求再指回代理（自指环）；同机的本地服务
 * （dev server、local runtime sockets 的 TCP 兄弟面）也没有任何理由绕一圈。
 *
 * 文案即契约：测试 import 本常量断言，**不手抄近似值**。
 */
export const SANDBOX_PROXY_NO_PROXY = 'localhost,127.0.0.1,::1'

/** 已经起来的那一个。`key` 是它当初的规则指纹，用来判断要不要重起。 */
type RunningProxy = {
  key: string
  handle: FilteringProxyHandle
}

/** 正在起的那一次。`key` 让同规则的并发调用者认出「跟着等就行」。 */
type InFlightStart = {
  key: string
  promise: Promise<number | null>
}

let running: RunningProxy | null = null
let inFlight: InFlightStart | null = null
let cleanupRegistered = false

/**
 * 规则指纹。全部安全字段按**位置**进数组再 JSON —— 位置区分了字段归属，所以
 * `allowed:['a'], denied:[]` 与 `allowed:[], denied:['a']` 不会撞成同一个 key
 * （拼字符串的写法会：两者都是 "a"）。
 */
function rulesKey(rules: DomainFilterRules): string {
  return JSON.stringify([
    rules.allowedDomains,
    rules.deniedDomains,
    rules.allowManagedDomainsOnly,
    rules.policy,
    rules.allowLocalBinding,
  ])
}

/**
 * 从当前 settings 派生域名规则。
 *
 * **读法与 `sandboxExecConfig.ts::deriveSandboxExecConfig` 逐字一致**（同一个
 * `convertToSandboxRuntimeConfig` + 同一个 `shouldAllowManagedSandboxDomainsOnly`）。
 * 第二套读法就是第二个漂移面：代理按 A 组规则过滤、配置文件按 B 组规则声明，
 * 用户看到的保真度报告会与实际拦截行为对不上，而且没有任何测试会发现。
 */
export function deriveDomainFilterRules(): DomainFilterRules {
  const runtime = convertToSandboxRuntimeConfig(getInitialSettings())
  return {
    allowedDomains: runtime.network?.allowedDomains ?? [],
    deniedDomains: runtime.network?.deniedDomains ?? [],
    allowManagedDomainsOnly: shouldAllowManagedSandboxDomainsOnly(),
    // direct-TUI 的 L1 helper 配置当前固定为 restricted；代理必须携带同一档位，
    // 不能成为 `none`/restricted 之外的宿主代连旁路。
    policy: 'restricted',
    allowLocalBinding: runtime.network?.allowLocalBinding ?? false,
  }
}

/**
 * 有没有东西要过滤。
 *
 * 三个来源任一非空即算 —— 与 `decideHost` 的档位判据同源：`allowManagedDomainsOnly`
 * 单独成立（空 allowlist + 该开关 = 全拒，是最严的一档，绝不能因为「两张表都空」
 * 被当成「没配规则」跳过）。
 *
 * 全空 ⇒ `decideHost` 恒 `default-allow` ⇒ 代理起来也只是在转发字节，纯粹是给
 * 每条命令的网络路径多插一跳。此时**不起**，现网络行为一个字节都不变。
 */
export function sandboxFilteringActive(rules: DomainFilterRules): boolean {
  return (
    rules.allowedDomains.length > 0 ||
    rules.deniedDomains.length > 0 ||
    rules.allowManagedDomainsOnly
  )
}

/**
 * 首次成功启动时注册一次进程退出清理。
 *
 * 注册的闭包**不捕获 handle**，只调 `stopSandboxFilteringProxy()` 现读模块状态 ——
 * 所以它在重起（换规则）之后依然收得到正确的那一个，也不会把一个早就 stop 过的
 * 旧 handle 拽在内存里。正因为它与具体实例无关，注册一次就够，测试 reset 后也
 * 不需要（更不该）再注册第二遍。
 */
function ensureCleanupRegistered(): void {
  if (cleanupRegistered) return
  cleanupRegistered = true
  registerCleanup(async () => {
    await stopSandboxFilteringProxy()
  })
}

/**
 * 真正的启停动作。**串行执行**：`previous` 是上一次在飞的启动，必须等它落地
 * 才能动 `running`，否则两次启动会互相覆盖 `running`，把先起来的那个变成一个
 * 没人认识、也没人 stop 的监听面（端口泄漏 + 一个仍在按旧规则放行的代理）。
 */
async function startForKey(
  key: string,
  rules: DomainFilterRules,
  previous: Promise<unknown> | null,
): Promise<number | null> {
  if (previous !== null) {
    // 前一次失败与否都不影响本次该不该起；只借它的时序。
    await previous.catch(() => null)
  }

  if (running !== null) {
    if (running.key === key) {
      // 排在前面的那次已经把同规则的代理起好了 —— 本次退化成一次查询。
      return running.handle.port
    }
    // 规则会话中途变了（用户改 settings）：旧代理按旧规则放行，留着就是一条
    // 已经被撤销的授权仍在生效。先收再起。
    await running.handle.stop().catch(() => null)
    running = null
  }

  try {
    const handle = await startFilteringProxy(rules)
    running = { key, handle }
    ensureCleanupRegistered()
    return handle.port
  } catch (error) {
    logForDebugging(
      `sandbox filtering proxy failed to start: ${String(error)}`,
      { level: 'warn' },
    )
    return null
  }
}

/**
 * 拿到可用的过滤代理端口；没有可用的就返回 `null`。
 *
 * 三种 `null`：规则为空（无需过滤）、规则刚变空（顺手把旧代理收了）、启动失败。
 * 调用方对三者一视同仁 —— 都是「没有 live 端口」，都不注入 proxy env。
 *
 * ## 单飞为什么无竞态
 *
 * 从函数入口到 `inFlight = entry` 这一段**没有任何 await**，而 JS 是单线程：
 * 并发调用者要么看见 `inFlight === null`（自己去起、并在同一个同步块里把
 * promise 挂上），要么看见已经挂好的 promise（跟着 await）。「两个都看见 null
 * 于是各起一个」这个交错在构造上不存在。不同 key 的调用者不共享 promise，但
 * 会把自己的启动**链在**前一次之后（见 `startForKey` 的 `previous`），所以任何
 * 时刻最多只有一个代理在监听。
 */
export async function ensureSandboxFilteringProxy(
  rules: DomainFilterRules,
): Promise<number | null> {
  if (!sandboxFilteringActive(rules)) {
    // 规则变空 = 用户把过滤关了。留着一个还在监听的代理既不再被任何人使用
    // （下面返回 null ⇒ 不注入 env），又是一个多余的本机开放中继。
    if (running !== null || inFlight !== null) {
      await stopSandboxFilteringProxy()
    }
    return null
  }

  const key = rulesKey(rules)

  // 有在飞的启动时不走 `running` 快路：那个 running 可能正要被换规则的启动收掉。
  if (inFlight === null && running !== null && running.key === key) {
    return running.handle.port
  }
  if (inFlight !== null && inFlight.key === key) {
    return await inFlight.promise
  }

  const entry: InFlightStart = {
    key,
    promise: startForKey(key, rules, inFlight?.promise ?? null),
  }
  inFlight = entry
  try {
    return await entry.promise
  } finally {
    // 只有「最后一个排队者」才清空：中途来的新 key 已经把 inFlight 指向它自己，
    // 此处无条件清空会让那次启动对后来者隐形。
    if (inFlight === entry) inFlight = null
  }
}

/**
 * 这批规则**此刻**有没有一个在跑的过滤代理；有则返回它的端口，否则 0。
 *
 * **同步**（直接读单例）是硬要求：消费者是会话级保真度判据
 * （`sandbox-adapter.ts::getSandboxFidelity`），它长在权限层的同步路径上，没有
 * 地方 await 一次启动。这也正是本函数只**查**不**起**的原因 —— 起代理是
 * {@link ensureSandboxFilteringProxy} 的事（`buildSandboxExecConfig` 在派生每条
 * 命令的配置前调它）。
 *
 * **按规则匹配**：一个按旧规则跑着的代理不能给新规则背书。用户改了 settings 之后
 * 到下一条命令重起代理之前有一个窗口，那段时间里在跑的东西过滤的是**已经被撤销
 * 的那套规则**；此时返回 0（⇒ 保真度 partial ⇒ 不发 auto-allow）是唯一诚实的
 * 答案。
 */
export function currentSandboxFilteringProxyPort(rules: DomainFilterRules): number {
  return running !== null && running.key === rulesKey(rules)
    ? running.handle.port
    : 0
}

/**
 * 停掉单例并清空状态。幂等。
 *
 * 先等在飞的启动落地再收 —— 不等就 stop 会漏掉「正在 listen 但还没写进
 * `running`」的那一个，留下一个谁也够不着的监听面。
 */
export async function stopSandboxFilteringProxy(): Promise<void> {
  const pending = inFlight
  if (pending !== null) {
    await pending.promise.catch(() => null)
  }
  const current = running
  running = null
  inFlight = null
  if (current !== null) {
    await current.handle.stop()
  }
}

/**
 * 沙箱子进程的 proxy 环境变量。**纯函数**，不碰 `process.env`。
 *
 * `port <= 0` ⇒ 空对象。这让调用方可以无条件展开它：没有代理时它就是个 no-op，
 * 不需要在 spawn 处再写一遍「有没有端口」的分支（第二处判据 = 第二个漂移面）。
 *
 * 大小写两套都写：工具生态对读哪一套从来没达成一致（curl 读小写、
 * git/Node/Bun 两套都读、部分 Python 栈只读大写），只写一套等于随机放过一半客户端。
 */
export function sandboxProxyChildEnv(port: number): Record<string, string> {
  if (!Number.isInteger(port) || port <= 0) return {}
  const url = `http://127.0.0.1:${port}`
  return {
    HTTP_PROXY: url,
    http_proxy: url,
    HTTPS_PROXY: url,
    https_proxy: url,
    NO_PROXY: SANDBOX_PROXY_NO_PROXY,
    no_proxy: SANDBOX_PROXY_NO_PROXY,
  }
}

/**
 * 测试用：**同步**丢掉单例状态，不等 stop 落地。
 *
 * 同步是刻意的 —— 测试的 teardown 要能在不 await 的路径上把状态清干净。真正的
 * 关闭 fire-and-forget（handle.stop 幂等），需要确定性关闭的用例请先 await
 * {@link stopSandboxFilteringProxy}。
 *
 * `cleanupRegistered` **不重置**：注册的闭包现读模块状态、与实例无关，重置只会
 * 让每个用例往全局 registry 里再塞一个等价回调。
 */
export function resetSandboxNetworkProxyForTests(): void {
  const current = running
  running = null
  inFlight = null
  void current?.handle.stop().catch(() => null)
}
