/**
 * 本机域名过滤代理 —— `networkFilter.ts` 判据的**执行面**
 * （W-SANDBOX-ENFORCED-DEADCODE PR-8 Slice 1）。
 *
 * 一个只听 `127.0.0.1` 临时端口的 HTTP 正向代理：
 *
 *   - **CONNECT**（HTTPS 及一切 TLS 隧道）—— 先判目标主机名，再解析并校验 IP，
 *     放行后对已校验 IP 建连并**原样对拷字节**。
 *   - **absolute-form 明文 HTTP**（`GET http://host/path`）—— 同样做域名 + 解析后
 *     IP 两层判定，并把连接钉在该 IP 上。
 *
 * ## 三条不可动摇的边界
 *
 * 1. **绝不 MITM** —— 不终止 TLS、不签证书、不装 CA、不看隧道里的一个字节。
 *    过滤面是**主机名**，不是内容。一个会解密用户流量的「安全特性」是净负债：
 *    它把每一次 shell 命令的凭据都摊到一个本机进程的内存里，换来的过滤精度
 *    对「这台机器能不能连 evil.example」这个问题毫无增量。
 * 2. **只听 loopback + 临时端口** —— `listen(0, '127.0.0.1')`，永不 `0.0.0.0`。
 *    这个代理对同机任何进程都是无认证可用的；一旦它出现在局域网上，就是一个
 *    开放中继。端口取 0 由内核分配，避免固定端口被抢占 / 被预测。
 * 3. **拒绝必须发生在建连之前** —— 域名判拒连 DNS 都不做；域名放行后才解析，
 *    若解析到 loopback/private/link-local/metadata/LAN 则在 TCP 前拒绝。真正建连
 *    使用的是同一个已校验 IP，不把主机名交给系统做第二次 DNS（防 rebinding）。
 *
 * ## 本切片刻意不做的事
 *
 * 不注册 `cleanupRegistry`、不读 settings、不派生规则、不碰 `Shell.ts` —— 生命周期
 * 与接线归 Slice 2。本模块只暴露 `start → {port, stop}` 这一对，`rules` 由调用方
 * 现给。这样它在测试里可以被完整站起来打，而不必先造一个会话。
 *
 * 用 `node:http` / `node:net` 而不是 `Bun.listen`：与 `upstreamproxy/relay.ts` 同因 ——
 * 这条路要在 Bun 与 Node 两个运行时下都成立。
 */

import {
  createServer as createHttpServer,
  request as httpRequest,
  type IncomingHttpHeaders,
  type IncomingMessage,
  type Server as HttpServer,
  type ServerResponse,
} from 'node:http'
import { lookup as dnsLookup } from 'node:dns/promises'
import { connect as netConnect, isIP, type Socket } from 'node:net'

import {
  decideHost,
  decideResolvedAddress,
  type DomainFilterRules,
} from './networkFilter.js'

/**
 * 上游**建连**预算。到点即 destroy，避免一个吊着不响应的目标把 socket 攒到
 * 进程生命周期结束。隧道建立后这个计时器会被撤掉 —— 见 `armConnectTimeout`
 * 处的注释（长连接空闲是合法的，拿建连预算去杀已建立的隧道会误伤 SSE /
 * 长轮询 / `git fetch` 这类正常长静默）。
 */
export const UPSTREAM_CONNECT_TIMEOUT_MS = 30_000

/**
 * 拒绝响应体的稳定前缀。文案是契约（沿用 `STRICT_SANDBOX_REFUSAL_PREFIX` /
 * `WEB_FETCH_SSRF_BLOCKED_PREFIX` 同款约定）：消费者按前缀归类，测试 import
 * 真源断言，**不手抄近似文案**。
 */
export const NETWORK_FILTER_BLOCKED_PREFIX = 'crabcode sandbox network filter:'

/** 构造拒绝响应体。两要素：拦了谁（host）、凭什么（reason）。 */
export function buildBlockedBody(host: string, reason: string): string {
  return `${NETWORK_FILTER_BLOCKED_PREFIX} ${host} is blocked by the sandbox network policy (${reason})\n`
}

export type FilteringProxyHandle = {
  /** 内核分配的 loopback 端口。 */
  port: number
  /** 停止监听并拆掉在飞连接。幂等。 */
  stop: () => Promise<void>
}

export type FilteringProxyOptions = {
  /** 每次判拒回调一次（host + 归因）。用于遥测 / 用户可见反馈，不参与判定。 */
  onDenied?: (host: string, reason: string) => void
  /**
   * 测试/平台注入的 DNS 解析器。生产默认 `dns.lookup({all:true})`。返回地址仍会
   * 逐条过安全分区判据，注入 resolver 不能绕过校验。
   */
  resolveHost?: HostResolver
}

export type ResolvedHostAddress = {
  address: string
  family: 4 | 6
}

export type HostResolver = (
  host: string,
) => Promise<readonly ResolvedHostAddress[]>

/** CONNECT authority-form 解析结果。 */
type ConnectTarget = {
  host: string
  port: number
}

const CONNECT_DEFAULT_PORT = 443
const HTTP_DEFAULT_PORT = 80

/**
 * 解析 CONNECT 的 authority-form 目标（`host:port` / `[v6]:port`）。
 *
 * 解析不出来一律返回 null（调用方回 400）——**畸形目标绝不猜**。猜错的方向
 * 是替客户端发明一个它没要求的主机名，而那个主机名会被拿去做放行判定。
 */
function parseConnectTarget(target: string | undefined): ConnectTarget | null {
  const raw = (target ?? '').trim()
  if (raw.length === 0) return null

  if (raw.startsWith('[')) {
    // IPv6 字面量：`[::1]` 或 `[::1]:443`
    const end = raw.indexOf(']')
    if (end <= 1) return null
    const host = raw.slice(1, end)
    const rest = raw.slice(end + 1)
    if (rest.length === 0) return { host, port: CONNECT_DEFAULT_PORT }
    if (!rest.startsWith(':')) return null
    const port = parsePort(rest.slice(1))
    if (port === null) return null
    return { host, port }
  }

  const sep = raw.lastIndexOf(':')
  if (sep === -1) {
    // 端口缺省是常见的宽松客户端写法；主机名仍要过校验。
    return isPlausibleHost(raw) ? { host: raw, port: CONNECT_DEFAULT_PORT } : null
  }
  const host = raw.slice(0, sep)
  const port = parsePort(raw.slice(sep + 1))
  if (port === null || !isPlausibleHost(host)) return null
  return { host, port }
}

function parsePort(text: string): number | null {
  if (!/^\d{1,5}$/.test(text)) return null
  const port = Number(text)
  if (!Number.isInteger(port) || port < 1 || port > 65_535) return null
  return port
}

/**
 * 主机名的最低形态校验。**不做 DNS 语法的完整校验** —— 只挡掉那些会让下游
 * 把一个串读成别的东西的字符（未加括号的 IPv6 冒号、路径分隔符、空白、
 * 用户信息 `@`）。宁可回 400，也不要把一个歧义串送进判据函数。
 */
function isPlausibleHost(host: string): boolean {
  if (host.length === 0 || host.length > 253) return false
  return !/[\s:/\\@[\]]/.test(host)
}

/** `new URL().hostname` 对 IPv6 会带方括号；判据与建连都要不带括号的裸名。 */
function stripIpv6Brackets(host: string): string {
  return host.startsWith('[') && host.endsWith(']') ? host.slice(1, -1) : host
}

async function defaultHostResolver(host: string): Promise<ResolvedHostAddress[]> {
  const addresses = await dnsLookup(host, { all: true, verbatim: true })
  return addresses.map(item => ({
    address: item.address,
    family: item.family === 6 ? 6 : 4,
  }))
}

type ApprovedTarget = {
  address: string
  family: 4 | 6
}

type TargetResolution =
  | { allowed: true; target: ApprovedTarget }
  | { allowed: false; reason: string }

/**
 * 解析一次、校验一次，并把**同一个已校验 IP**交给建连层。调用方绝不能再把
 * `host` 传给 `net.connect` / `http.request`，否则系统 resolver 会做第二次查询，
 * DNS rebinding 就会把第一次校验变成装饰。
 */
async function resolveApprovedTarget(
  host: string,
  rules: DomainFilterRules,
  resolver: HostResolver,
): Promise<TargetResolution> {
  if (rules.policy === 'none') {
    return { allowed: false, reason: 'network-policy:none' }
  }

  const literalFamily = isIP(host)
  const candidates: readonly ResolvedHostAddress[] =
    literalFamily === 4 || literalFamily === 6
      ? [{ address: host, family: literalFamily }]
      : await resolver(host)

  let firstBlockedReason = 'dns-no-usable-address'
  for (const candidate of candidates) {
    // 不信 resolver 自报的 family；地址文本本身必须是一个合法、完整的 IP。
    const actualFamily = isIP(candidate.address)
    if (actualFamily !== 4 && actualFamily !== 6) {
      firstBlockedReason = 'dns-invalid-address'
      continue
    }
    const decision = decideResolvedAddress(candidate.address, rules)
    if (decision.allowed) {
      return {
        allowed: true,
        target: { address: candidate.address, family: actualFamily },
      }
    }
    if (firstBlockedReason === 'dns-no-usable-address') {
      firstBlockedReason = decision.reason
    }
  }
  return { allowed: false, reason: firstBlockedReason }
}

/**
 * 转发前摘掉 hop-by-hop 的代理头。`proxy-authorization` 尤其不能带给目标 ——
 * 那是客户端给**代理**的凭据，转出去就是一次凭据外泄。
 */
function sanitizeForwardHeaders(
  headers: IncomingHttpHeaders,
): IncomingHttpHeaders {
  const forwarded: IncomingHttpHeaders = { ...headers }
  delete forwarded['proxy-connection']
  delete forwarded['proxy-authorization']
  return forwarded
}

/**
 * 写完响应再拆连接。**刻意用 write 回调而不是紧跟着 destroy()**：`destroy()`
 * 会丢掉还留在流缓冲里的字节，而拒绝响应恰恰是最不能被截断的那一个 ——
 * 客户端读到半截会把「被策略拦了」看成「网络坏了」。
 */
function respondAndClose(socket: Socket, payload: string): void {
  if (socket.destroyed) return
  socket.write(payload, () => {
    socket.destroy()
  })
}

/**
 * 起一个过滤代理。返回它绑到的 loopback 端口与 stop 句柄。
 *
 * `rules` 按值捕获：本模块不监听配置变化，规则变了由调用方重起一个
 * （per-command 派生的配置本来就没有「会话中途变更」这个概念）。
 */
export async function startFilteringProxy(
  rules: DomainFilterRules,
  opts?: FilteringProxyOptions,
): Promise<FilteringProxyHandle> {
  const onDenied = opts?.onDenied
  const resolveHost = opts?.resolveHost ?? defaultHostResolver
  let stopped = false

  // 在飞 socket 台账，供 stop() 强拆。`once('close')` 自摘，故不会泄漏。
  const openSockets = new Set<Socket>()
  function track(socket: Socket): void {
    openSockets.add(socket)
    socket.once('close', () => {
      openSockets.delete(socket)
    })
  }

  function deny(host: string, reason: string): void {
    try {
      onDenied?.(host, reason)
    } catch {
      // 回调是观测面，不是控制面：它抛错不该影响拒绝本身已经生效这件事。
    }
  }

  const server: HttpServer = createHttpServer(
    (req: IncomingMessage, res: ServerResponse) => {
      void handlePlainRequest(
        req,
        res,
        rules,
        resolveHost,
        deny,
        track,
        () => stopped,
      ).catch(() => {
        if (res.destroyed) return
        if (res.headersSent) {
          res.destroy()
          return
        }
        res.writeHead(502, { 'Content-Type': 'text/plain; charset=utf-8' })
        res.end('upstream request failed\n')
      })
    },
  )

  // ── CONNECT：TLS 隧道 ────────────────────────────────────────────────────
  server.on('connect', (req: IncomingMessage, clientSocket: Socket, head: Buffer) => {
    track(clientSocket)
    // 先装错误处理器再做任何事：一个说完 CONNECT 就跑掉的客户端不该把
    // 宿主进程带走（未处理的 socket error 是 uncaught exception）。
    clientSocket.on('error', () => {
      clientSocket.destroy()
    })

    const target = parseConnectTarget(req.url)
    if (target === null) {
      respondAndClose(
        clientSocket,
        'HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n',
      )
      return
    }

    const host = stripIpv6Brackets(target.host)
    const decision = decideHost(host, rules)
    if (!decision.allowed) {
      deny(host, decision.reason)
      // 判拒分支到此为止 —— 上游 socket 一个都没开过（见文件头边界 3）。
      respondAndClose(
        clientSocket,
        'HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n' +
          buildBlockedBody(host, decision.reason),
      )
      return
    }

    void resolveApprovedTarget(host, rules, resolveHost)
      .then(resolution => {
        if (stopped || clientSocket.destroyed) return
        if (!resolution.allowed) {
          deny(host, resolution.reason)
          respondAndClose(
            clientSocket,
            'HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n' +
              buildBlockedBody(host, resolution.reason),
          )
          return
        }
        openTunnel(clientSocket, head, resolution.target, target.port, track)
      })
      .catch(() => {
        // DNS failure is an upstream availability error, not a policy denial.
        // Crucially, no upstream socket was opened with the unvalidated name.
        respondAndClose(
          clientSocket,
          'HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n',
        )
      })
  })

  // 客户端发来的字节根本不成 HTTP（半截请求行、未知方法、超长头…）。
  // 显式接管只为一件事：**任何形态都不许把进程带走**。
  //
  // 实测（Bun 1.3.11 / Windows）：Bun 的 node:http **会**触发本事件、且 socket
  // 自称 writable，但此刻写进去的字节客户端一个也收不到（`end(payload)` 与
  // `write(payload, cb)` 两种写法都是空响应）—— 底层连接已经拆了。所以这条
  // 400 是 **Node 路径**的礼貌，不是跨运行时承诺；两个运行时共同兑现的只有
  // 「不成 HTTP 的字节流永远拿不到隧道，且宿主继续服务下一条连接」。
  server.on('clientError', (_err: Error, socket: Socket) => {
    if (socket.destroyed || !socket.writable) {
      socket.destroy()
      return
    }
    socket.end('HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n')
  })

  const stop = async (): Promise<void> => {
    if (stopped) return
    stopped = true
    for (const socket of openSockets) {
      socket.destroy()
    }
    openSockets.clear()
    // Node ≥18.2 / Bun：keep-alive 的空闲连接不会自己走，close() 会一直等它们。
    ;(
      server as HttpServer & { closeAllConnections?: () => void }
    ).closeAllConnections?.()
    await new Promise<void>(resolve => {
      // close() 的 error 参数只在「本来就没在监听」时出现，对幂等 stop 无意义。
      server.close(() => resolve())
    })
  }

  return await new Promise<FilteringProxyHandle>((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => {
      server.removeListener('error', reject)
      // listen 之后再来的 server error（EMFILE 之类）没有 promise 可以拒了；
      // 不接住就是 uncaught。
      server.on('error', () => {})
      const address = server.address()
      if (address === null || typeof address === 'string') {
        server.close()
        reject(new Error('filtering proxy: server has no TCP address'))
        return
      }
      resolve({ port: address.port, stop })
    })
  })
}

/**
 * 放行后的 CONNECT：连上游 → 回 200 → 原样对拷。
 *
 * `head` 是 CONNECT 头之后**已经到达**的客户端字节（TCP 常把 CONNECT 与
 * ClientHello 合成一个包）。不 flush 它就等于静默吞掉客户端的第一段 TLS
 * 握手，表现为「连上了但一直没反应」。
 */
function openTunnel(
  clientSocket: Socket,
  head: Buffer,
  target: ApprovedTarget,
  port: number,
  track: (socket: Socket) => void,
): void {
  // Connect to the exact address returned by resolveApprovedTarget. Passing
  // the original hostname here would trigger a second DNS lookup (rebinding).
  const upstream = netConnect({
    host: target.address,
    port,
    family: target.family,
  })
  track(upstream)
  let established = false

  upstream.setTimeout(UPSTREAM_CONNECT_TIMEOUT_MS)
  upstream.on('timeout', () => {
    // 建连阶段：吊死的目标就地拆掉（'close' 会把客户端一并收拾）。
    if (!established) upstream.destroy()
  })

  upstream.on('connect', () => {
    established = true
    // 撤掉建连预算：已建立的隧道空闲是合法的（SSE / 长轮询 / git 大包）。
    upstream.setTimeout(0)
    clientSocket.write('HTTP/1.1 200 Connection Established\r\n\r\n')
    if (head.length > 0) upstream.write(head)
    clientSocket.pipe(upstream)
    upstream.pipe(clientSocket)
  })

  upstream.on('error', () => {
    if (!established) {
      // 还没回过 200，此时客户端仍在读 HTTP 响应，说得起人话。
      respondAndClose(clientSocket, 'HTTP/1.1 502 Bad Gateway\r\n\r\n')
      return
    }
    // 已经在跑 TLS 了：再写明文会污染客户端的流，只能拆。
    clientSocket.destroy()
  })

  upstream.on('close', hadError => {
    if (hadError) clientSocket.destroy()
    else endGracefully(clientSocket)
  })
  clientSocket.on('close', hadError => {
    if (hadError) upstream.destroy()
    else endGracefully(upstream)
  })
}

/**
 * 对端正常收尾时用 `end()` 而不是 `destroy()` —— pipe 已经把 EOF 转过去了，
 * 此刻 destroy 会截掉还在写缓冲里的最后一段（隧道里那就是半个 TLS record）。
 */
function endGracefully(socket: Socket): void {
  if (socket.destroyed || socket.writableEnded) return
  socket.end()
}

/**
 * absolute-form 明文 HTTP。https:// **不从这里走**（那些是 CONNECT），
 * origin-form（`GET /path`）也不是给代理的请求 —— 两者一律 400。
 */
async function handlePlainRequest(
  req: IncomingMessage,
  res: ServerResponse,
  rules: DomainFilterRules,
  resolveHost: HostResolver,
  deny: (host: string, reason: string) => void,
  track: (socket: Socket) => void,
  isStopped: () => boolean,
): Promise<void> {
  let url: URL
  try {
    url = new URL(req.url ?? '')
  } catch {
    res.writeHead(400, { 'Content-Type': 'text/plain; charset=utf-8' })
    res.end('proxy requests must use absolute-form URLs\n')
    return
  }
  if (url.protocol !== 'http:') {
    res.writeHead(400, { 'Content-Type': 'text/plain; charset=utf-8' })
    res.end('only absolute-form http:// requests are proxied here\n')
    return
  }

  const host = stripIpv6Brackets(url.hostname)
  const decision = decideHost(host, rules)
  if (!decision.allowed) {
    deny(host, decision.reason)
    res.writeHead(403, { 'Content-Type': 'text/plain; charset=utf-8' })
    res.end(buildBlockedBody(host, decision.reason))
    return
  }

  let resolution: TargetResolution
  try {
    resolution = await resolveApprovedTarget(host, rules, resolveHost)
  } catch {
    if (!res.destroyed && !res.headersSent) {
      res.writeHead(502, { 'Content-Type': 'text/plain; charset=utf-8' })
      res.end('upstream name resolution failed\n')
    }
    return
  }
  if (isStopped() || req.destroyed || res.destroyed) return
  if (!resolution.allowed) {
    deny(host, resolution.reason)
    res.writeHead(403, { 'Content-Type': 'text/plain; charset=utf-8' })
    res.end(buildBlockedBody(host, resolution.reason))
    return
  }

  const headers = sanitizeForwardHeaders(req.headers)
  // absolute-form URL 是代理请求的权威目标；不允许一个不一致的 Host 头把已验证
  // IP 上的请求改投另一个虚拟主机。
  headers.host = url.host

  let upstreamReq: ReturnType<typeof httpRequest>
  try {
    upstreamReq = httpRequest(
      {
        // DNS pinning：只连接刚才验证过的 IP，绝不把原始 host 再交给 resolver。
        hostname: resolution.target.address,
        family: resolution.target.family,
        port: url.port === '' ? HTTP_DEFAULT_PORT : Number(url.port),
        method: req.method,
        path: url.pathname + url.search,
        headers,
      },
      upstreamRes => {
        res.writeHead(upstreamRes.statusCode ?? 502, upstreamRes.headers)
        upstreamRes.pipe(res)
      },
    )
  } catch {
    res.writeHead(502, { 'Content-Type': 'text/plain; charset=utf-8' })
    res.end('upstream request failed\n')
    return
  }
  upstreamReq.on('socket', socket => track(socket as Socket))
  upstreamReq.setTimeout(UPSTREAM_CONNECT_TIMEOUT_MS, () => {
    upstreamReq.destroy()
  })
  upstreamReq.on('error', () => {
    if (res.headersSent) {
      res.destroy()
      return
    }
    res.writeHead(502, { 'Content-Type': 'text/plain; charset=utf-8' })
    res.end('upstream request failed\n')
  })
  req.pipe(upstreamReq)
}
