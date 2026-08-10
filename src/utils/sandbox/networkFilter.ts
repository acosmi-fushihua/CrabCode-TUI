/**
 * 域名过滤判据 —— **纯函数、零 I/O、零 socket**
 * （W-SANDBOX-ENFORCED-DEADCODE PR-8 Slice 1）。
 *
 * ## 它补的是哪个洞
 *
 * `fidelity.ts` 里那三行不是装饰：`network.allowedDomains` /
 * `network.deniedDomains` / `network.allowManagedDomainsOnly` 在**所有平台**都被
 * 无条件记进 `unenforced` —— 内核 seccomp 按 socket domain 过滤、Seatbelt 按
 * host/port 过滤，两个都表达不了「域名 allowlist」。用户在 settings 里写下的
 * 域名规则一直是**被携带、被校验、然后不施加**。本模块是补齐那条强制面的第一
 * 块砖：把「这个主机名放不放行」收敛成一个可穷举测试的判据函数。
 *
 * ## 为什么判据必须与代理面分家
 *
 * 判据是安全边界，代理面是传输管道。两者住在一起时，测试要么得站起一个真
 * server 才能测一条字符串匹配（慢、平台相关、易 flake），要么就只测得到管道
 * 而测不到边界。拆开之后本模块可以被**穷举**打：每一种模式形态 × 每一种
 * label 边界负例，全部是微秒级纯函数断言。
 *
 * ## label 边界是本模块唯一真正危险的地方
 *
 * 朴素写法 `host.endsWith(pattern)` 会让 `notexample.com` 命中 `example.com`，
 * `evilexample.com` 命中 `example.com` —— 攻击者只要注册一个以放行域**结尾**
 * 的域名就能穿过 allowlist。所以本模块的每一条分支都只用两种判据：
 *
 *   - `host === base`（apex 精确相等）
 *   - `host.endsWith('.' + base)`（那个点强制了一次 label 边界）
 *
 * **绝不出现**裸 `includes()` / 不带前导点的 `endsWith()`。IP 字面量走 plain
 * 形态天然只能精确相等命中（`'.'+ip` 永远不会误配另一个 IP）。
 */

/**
 * 域名规则 + 代理传输安全语义。字段名与 `SandboxExecNetworkRules` 对齐，便于
 * 生命周期层从同一份 runtime 派生，不另造一套会漂移的策略输入。
 */
export type DomainFilterRules = {
  /** 非空 ⇒ 进 allowlist 档：只有命中其一（且未被 deny）的主机才放行。 */
  allowedDomains: string[]
  /** 恒拦。**deny 赢 allow**（同一主机同时命中两表时判拒）。 */
  deniedDomains: string[]
  /**
   * true ⇒ 即便 `allowedDomains` 为空也进 allowlist 档
   * （⇒ 空 allowlist ⇒ 一切外部主机全拒）。
   *
   * 这是策略级承诺（`policySettings.sandbox.network.allowManagedDomainsOnly`）：
   * 「只有托管域名可达」在托管侧一条域名都没配时的正确解释是**全拒**，不是
   * 「没配就随便走」—— 后者会把一条最严的策略读成最松的默认值。
   */
  allowManagedDomainsOnly: boolean
  /**
   * 内核三档网络策略。过滤代理必须拿到同一档位，不能在 `none` 档替命令
   * 打开一条宿主代连通道。所有调用方必须显式携带，避免策略字段在某条接线
   * 上悄悄掉回默认值。
   */
  policy: 'none' | 'restricted' | 'host'
  /**
   * 唯一允许的本机地址例外。true 只放行 loopback；RFC1918、link-local、
   * metadata 等范围仍禁止由宿主代理代连。
   */
  allowLocalBinding: boolean
}

/**
 * 判据结论。`reason` 是给日志 / 用户可见拒绝文案用的**归因**，不是给程序做
 * 二次判断用的 —— 程序只看 `allowed`。
 */
export type FilterDecision = {
  allowed: boolean
  reason: string
}

/** DNS 解析后地址的安全分区。`public` 是代理默认唯一可代连的分区。 */
export type NetworkAddressScope =
  | 'public'
  | 'loopback'
  | 'private'
  | 'link-local'
  | 'metadata'
  | 'non-public'

const METADATA_IPV4 = new Set([
  // AWS / GCP / OpenStack and container credential endpoints.
  '169.254.169.254',
  '169.254.170.2',
  // Alibaba Cloud.
  '100.100.100.200',
  // Oracle Cloud.
  '192.0.0.192',
  // Azure host virtual address (wire server / platform services).
  '168.63.129.16',
])

function parseIpv4Address(address: string): number | null {
  const parts = address.split('.')
  if (parts.length !== 4) return null
  let value = 0
  for (const part of parts) {
    // Resolved addresses should already be canonical. Reject ambiguous octal /
    // shorthand spellings here instead of trying to interpret them twice.
    if (!/^(?:0|[1-9]\d{0,2})$/.test(part)) return null
    const octet = Number(part)
    if (octet > 255) return null
    value = (value * 256 + octet) >>> 0
  }
  return value
}

function classifyIpv4Value(value: number, canonical?: string): NetworkAddressScope {
  const a = (value >>> 24) & 0xff
  const b = (value >>> 16) & 0xff
  const c = (value >>> 8) & 0xff
  const d = value & 0xff
  const text = canonical ?? `${a}.${b}.${c}.${d}`

  if (METADATA_IPV4.has(text)) return 'metadata'
  if (a === 127) return 'loopback'
  if (a === 169 && b === 254) return 'link-local'
  if (
    a === 10 ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && b === 168) ||
    (a === 100 && b >= 64 && b <= 127)
  ) {
    return 'private'
  }

  // Unspecified, protocol-assignment, documentation, benchmarking,
  // multicast, and reserved ranges are not safe public destinations for a
  // host-side confused-deputy proxy.
  if (
    a === 0 ||
    (a === 192 && b === 0 && c === 0) ||
    (a === 192 && b === 0 && c === 2) ||
    (a === 192 && b === 88 && c === 99) ||
    (a === 198 && (b === 18 || b === 19)) ||
    (a === 198 && b === 51 && c === 100) ||
    (a === 203 && b === 0 && c === 113) ||
    a >= 224
  ) {
    return 'non-public'
  }
  return 'public'
}

function parseIpv6Address(address: string): bigint | null {
  const zone = address.indexOf('%')
  let raw = (zone === -1 ? address : address.slice(0, zone)).toLowerCase()
  if (raw.length === 0) return null

  // Turn an IPv4 tail into two hextets before expanding `::`.
  if (raw.includes('.')) {
    const separator = raw.lastIndexOf(':')
    if (separator === -1) return null
    const ipv4 = parseIpv4Address(raw.slice(separator + 1))
    if (ipv4 === null) return null
    raw =
      raw.slice(0, separator + 1) +
      ((ipv4 >>> 16) & 0xffff).toString(16) +
      ':' +
      (ipv4 & 0xffff).toString(16)
  }

  if (raw.indexOf('::') !== raw.lastIndexOf('::')) return null
  const hasCompression = raw.includes('::')
  const [leftText, rightText = ''] = hasCompression ? raw.split('::') : [raw, '']
  const left = leftText === '' ? [] : leftText.split(':')
  const right = rightText === '' ? [] : rightText.split(':')
  if (left.some(part => !/^[0-9a-f]{1,4}$/.test(part))) return null
  if (right.some(part => !/^[0-9a-f]{1,4}$/.test(part))) return null

  const missing = 8 - left.length - right.length
  if ((hasCompression && missing < 1) || (!hasCompression && missing !== 0)) {
    return null
  }
  const parts = [
    ...left,
    ...Array.from({ length: missing }, () => '0'),
    ...right,
  ]
  if (parts.length !== 8) return null

  let value = 0n
  for (const part of parts) value = (value << 16n) | BigInt(`0x${part}`)
  return value
}

/**
 * 把**解析后的** IP 分进安全区。输入不是合法 IP 时按 `non-public` 失败关闭。
 * IPv4-mapped / compatible IPv6 会递归按其内嵌 IPv4 判断，不能用
 * `::ffff:127.0.0.1` 绕过 loopback 检查。
 */
export function classifyNetworkAddress(address: string): NetworkAddressScope {
  const ipv4 = parseIpv4Address(address)
  if (ipv4 !== null) return classifyIpv4Value(ipv4, address)

  const ipv6 = parseIpv6Address(address)
  if (ipv6 === null) return 'non-public'
  if (ipv6 === 0n) return 'non-public'
  if (ipv6 === 1n) return 'loopback'

  const high96 = ipv6 >> 32n
  if (high96 === 0xffffn || high96 === 0n) {
    return classifyIpv4Value(Number(ipv6 & 0xffff_ffffn) >>> 0)
  }

  // fc00::/7 (ULA), fe80::/10 (link-local), ff00::/8 (multicast).
  if (ipv6 >> 121n === 0x7en) return 'private'
  if (ipv6 >> 118n === 0x3fan) return 'link-local'
  if (ipv6 >> 120n === 0xffn) return 'non-public'

  // Documentation and transition mechanisms can embed a second destination;
  // the proxy does not attempt to validate the remote translator's routing.
  if (
    ipv6 >> 96n === 0x2001_0db8n ||
    ipv6 >> 112n === 0x2002n ||
    ipv6 >> 96n === 0x2001_0000n
  ) {
    return 'non-public'
  }

  // Only global-unicast 2000::/3 is treated as public. Everything else is a
  // special/local allocation and fails closed.
  return ipv6 >> 125n === 1n ? 'public' : 'non-public'
}

/**
 * 地址层判据。域名 allow/deny 先由 {@link decideHost} 裁决；即使域名显式在
 * allowlist，这一层仍不允许它解析到宿主/LAN/metadata。`allowLocalBinding`
 * 只对 loopback 开一个窄例外，不放宽任何其它非公网范围。
 */
export function decideResolvedAddress(
  address: string,
  rules: DomainFilterRules,
): FilterDecision {
  const policy = rules.policy
  if (policy === 'none') {
    return { allowed: false, reason: 'network-policy:none' }
  }
  if (policy !== 'restricted' && policy !== 'host') {
    return { allowed: false, reason: 'network-policy:invalid' }
  }

  const scope = classifyNetworkAddress(address)
  if (scope === 'public') return { allowed: true, reason: 'public-address' }
  if (scope === 'loopback' && rules.allowLocalBinding === true) {
    return { allowed: true, reason: 'allow-local-binding' }
  }
  return { allowed: false, reason: `blocked-address:${scope}` }
}

/**
 * 主机名 × 单条模式。**大小写无关**，且**恒守 label 边界**（永不子串命中）。
 *
 * 三种模式形态：
 *
 * | 模式 | apex（`example.com`） | 子域（`a.example.com`） | 说明 |
 * |---|---|---|---|
 * | `*.example.com` | ❌ | ✅ | 只要子域，刻意不含 apex |
 * | `.example.com`  | ✅ | ✅ | 前导点的传统「本域及其下」写法 |
 * | `example.com`   | ✅ | ✅ | 最常见的「这个域」意思 |
 *
 * `notexample.com` / `evilexample.com` 对上面任一形态都**不命中**。
 *
 * 主机名尾部的**单个**点（FQDN 写法 `example.com.`）在匹配前剥掉 —— DNS 上
 * `example.com.` 与 `example.com` 是同一个名字，不剥就等于给攻击者一个零成本
 * 的 allowlist 绕过（以及一个零成本的 denylist 绕过）。
 *
 * 空模式（trim 后为空）**永不命中**：配置里的一行空白不该变成「放行一切」，
 * 也不该变成「拦下一切」。退化模式（`*.` / `.` 这种没有 base 的）同理。
 */
export function matchesDomainPattern(
  hostname: string,
  pattern: string,
): boolean {
  const pat = pattern.trim().toLowerCase()
  if (pat.length === 0) return false

  let host = hostname.trim().toLowerCase()
  // FQDN 尾点：剥一个（只剥一个 —— `a..` 这种畸形名不该被规整成合法名）。
  if (host.endsWith('.')) host = host.slice(0, -1)
  if (host.length === 0) return false

  if (pat.startsWith('*.')) {
    const base = pat.slice(2)
    if (base.length === 0) return false
    // 子域 only：`'.' + base` 保证左边至少还有一个 label，apex 自然落空。
    return host.endsWith('.' + base)
  }

  if (pat.startsWith('.')) {
    const base = pat.slice(1)
    if (base.length === 0) return false
    return host === base || host.endsWith('.' + base)
  }

  return host === pat || host.endsWith('.' + pat)
}

/**
 * 一个主机名在一组规则下的裁决。**顺序即语义**，四步：
 *
 *   1. 命中任一 `deniedDomains` → 拒（**deny 赢 allow**，无条件先判）
 *   2. allowlist 档 = `allowedDomains` 非空 **或** `allowManagedDomainsOnly`
 *   3. allowlist 档内：命中 `allowedDomains` → 放行；否则拒
 *   4. 非 allowlist 档（两表都没说话）→ 放行（`default-allow`）
 *
 * 第 4 步是**有意的 default-allow**：没配任何域名规则的用户没有表达过「我要
 * 网络隔离」，此时全拦等于把一个没人打开的开关变成一场事故。真正的
 * fail-closed 责任在第 3 步 —— 一旦用户表达了 allowlist 意图，**没在名单上
 * 就是拒**，包括 allowlist 为空的极端情形。
 */
export function decideHost(
  hostname: string,
  rules: DomainFilterRules,
): FilterDecision {
  for (const pattern of rules.deniedDomains) {
    if (matchesDomainPattern(hostname, pattern)) {
      return { allowed: false, reason: `denied:${pattern}` }
    }
  }

  const allowlistMode =
    rules.allowedDomains.length > 0 || rules.allowManagedDomainsOnly

  if (allowlistMode) {
    for (const pattern of rules.allowedDomains) {
      if (matchesDomainPattern(hostname, pattern)) {
        return { allowed: true, reason: `allowed:${pattern}` }
      }
    }
    return {
      allowed: false,
      reason: rules.allowManagedDomainsOnly
        ? 'not-in-managed-allowlist'
        : 'not-in-allowlist',
    }
  }

  return { allowed: true, reason: 'default-allow' }
}
