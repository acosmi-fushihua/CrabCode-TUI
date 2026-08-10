import { afterEach, describe, expect, test } from 'bun:test'
import { createServer as createHttpServer } from 'node:http'
import {
  connect as netConnect,
  createServer as createNetServer,
  type Server,
  type Socket,
} from 'node:net'

import {
  buildBlockedBody,
  startFilteringProxy,
  type FilteringProxyHandle,
  type HostResolver,
} from '../../src/utils/sandbox/filteringProxy.js'
import type { DomainFilterRules } from '../../src/utils/sandbox/networkFilter.js'

const TIMEOUT_MS = 5_000
const cleanups: Array<() => Promise<void> | void> = []

afterEach(async () => {
  while (cleanups.length > 0) await cleanups.pop()?.()
})

function rules(overrides: Partial<DomainFilterRules>): DomainFilterRules {
  return {
    allowedDomains: [],
    deniedDomains: [],
    allowManagedDomainsOnly: false,
    policy: 'restricted',
    allowLocalBinding: false,
    ...overrides,
  }
}

async function listen(server: Server): Promise<number> {
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => {
      server.removeListener('error', reject)
      resolve()
    })
  })
  const address = server.address()
  if (address === null || typeof address === 'string') {
    throw new Error('test server did not bind')
  }
  return address.port
}

async function startEcho(): Promise<{ port: number; connections: () => number }> {
  let connections = 0
  const sockets = new Set<Socket>()
  const server = createNetServer(socket => {
    connections += 1
    sockets.add(socket)
    socket.once('close', () => sockets.delete(socket))
    socket.on('error', () => socket.destroy())
    socket.pipe(socket)
  })
  const port = await listen(server)
  cleanups.push(async () => {
    for (const socket of sockets) socket.destroy()
    await new Promise<void>(resolve => server.close(() => resolve()))
  })
  return { port, connections: () => connections }
}

async function startProxy(
  proxyRules: DomainFilterRules,
  denied?: Array<[string, string]>,
  resolveHost?: HostResolver,
): Promise<FilteringProxyHandle> {
  const proxy = await startFilteringProxy(proxyRules, {
    onDenied: (host, reason) => denied?.push([host, reason]),
    resolveHost,
  })
  cleanups.push(() => proxy.stop())
  return proxy
}

async function startHttpEcho(): Promise<{
  port: number
  requests: () => number
  lastHost: () => string
}> {
  let requests = 0
  let lastHost = ''
  const server = createHttpServer((req, res) => {
    requests += 1
    lastHost = req.headers.host ?? ''
    res.writeHead(200, { 'Content-Type': 'text/plain', Connection: 'close' })
    res.end('pinned-http-upstream')
  })
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => {
      server.removeListener('error', reject)
      resolve()
    })
  })
  const address = server.address()
  if (address === null || typeof address === 'string') {
    throw new Error('HTTP test server did not bind')
  }
  cleanups.push(async () => {
    ;(
      server as typeof server & { closeAllConnections?: () => void }
    ).closeAllConnections?.()
    await new Promise<void>(resolve => server.close(() => resolve()))
  })
  return {
    port: address.port,
    requests: () => requests,
    lastHost: () => lastHost,
  }
}

async function client(port: number): Promise<{
  socket: Socket
  text: () => string
  closed: () => boolean
}> {
  const socket = netConnect(port, '127.0.0.1')
  let text = ''
  let closed = false
  socket.on('data', chunk => {
    text += chunk.toString()
  })
  socket.on('close', () => {
    closed = true
  })
  socket.on('error', () => {
    closed = true
  })
  cleanups.push(() => socket.destroy())
  await new Promise<void>((resolve, reject) => {
    socket.once('connect', resolve)
    socket.once('error', reject)
  })
  return { socket, text: () => text, closed: () => closed }
}

async function waitFor(check: () => boolean): Promise<void> {
  const deadline = Date.now() + TIMEOUT_MS
  while (!check()) {
    if (Date.now() > deadline) throw new Error('timed out waiting for socket state')
    await Bun.sleep(10)
  }
}

describe('loopback filtering proxy', () => {
  test('CONNECT denial happens before any upstream connection', async () => {
    const upstream = await startEcho()
    const denied: Array<[string, string]> = []
    const proxy = await startProxy(
      rules({ deniedDomains: ['127.0.0.1'] }),
      denied,
    )
    const connection = await client(proxy.port)

    connection.socket.write(
      `CONNECT 127.0.0.1:${upstream.port} HTTP/1.1\r\n` +
        `Host: 127.0.0.1:${upstream.port}\r\n\r\n`,
    )
    await waitFor(connection.closed)

    expect(connection.text()).toContain('HTTP/1.1 403 Forbidden')
    expect(connection.text()).toContain(
      buildBlockedBody('127.0.0.1', 'denied:127.0.0.1'),
    )
    expect(upstream.connections()).toBe(0)
    expect(denied).toEqual([['127.0.0.1', 'denied:127.0.0.1']])
  })

  test('an allowed CONNECT tunnel copies bytes unchanged', async () => {
    const upstream = await startEcho()
    const proxy = await startProxy(
      rules({
        allowedDomains: ['127.0.0.1'],
        allowLocalBinding: true,
      }),
    )
    const connection = await client(proxy.port)

    connection.socket.write(
      `CONNECT 127.0.0.1:${upstream.port} HTTP/1.1\r\n` +
        `Host: 127.0.0.1:${upstream.port}\r\n\r\n`,
    )
    await waitFor(() => connection.text().includes('\r\n\r\n'))
    expect(connection.text()).toContain('200 Connection Established')

    connection.socket.write('direct-tui-sandbox')
    await waitFor(() => connection.text().endsWith('direct-tui-sandbox'))
    expect(upstream.connections()).toBe(1)
  })

  test('explicit allow, deny-only and default-allow cannot relay a DNS name to loopback', async () => {
    const scenarios: Array<[string, DomainFilterRules]> = [
      [
        'explicit-allow',
        rules({ allowedDomains: ['internal.test'] }),
      ],
      [
        'deny-only',
        rules({ deniedDomains: ['unrelated.test'] }),
      ],
      ['default-allow', rules({})],
    ]

    for (const [name, proxyRules] of scenarios) {
      const upstream = await startEcho()
      let resolutions = 0
      const denied: Array<[string, string]> = []
      const proxy = await startProxy(proxyRules, denied, async host => {
        resolutions += 1
        expect(host).toBe('internal.test')
        return [{ address: '127.0.0.1', family: 4 }]
      })
      const connection = await client(proxy.port)
      connection.socket.write(
        `CONNECT internal.test:${upstream.port} HTTP/1.1\r\n` +
          `Host: internal.test:${upstream.port}\r\n\r\n`,
      )
      await waitFor(connection.closed)

      expect(connection.text(), name).toContain('HTTP/1.1 403 Forbidden')
      expect(connection.text(), name).toContain('blocked-address:loopback')
      expect(upstream.connections(), name).toBe(0)
      expect(resolutions, name).toBe(1)
      expect(denied, name).toEqual([
        ['internal.test', 'blocked-address:loopback'],
      ])
      await proxy.stop()
    }
  })

  test('none policy refuses before DNS and cannot turn the host proxy into an egress path', async () => {
    let resolutions = 0
    const proxy = await startProxy(
      rules({ policy: 'none', allowedDomains: ['public.test'] }),
      undefined,
      async () => {
        resolutions += 1
        return [{ address: '8.8.8.8', family: 4 }]
      },
    )
    const connection = await client(proxy.port)
    connection.socket.write(
      'CONNECT public.test:443 HTTP/1.1\r\nHost: public.test:443\r\n\r\n',
    )
    await waitFor(connection.closed)

    expect(connection.text()).toContain('network-policy:none')
    expect(resolutions).toBe(0)
  })

  test('CONNECT pins the validated DNS answer and never resolves the hostname again', async () => {
    const upstream = await startEcho()
    let resolutions = 0
    const proxy = await startProxy(
      rules({
        allowedDomains: ['rebind.invalid'],
        allowLocalBinding: true,
      }),
      undefined,
      async host => {
        resolutions += 1
        expect(host).toBe('rebind.invalid')
        return [{ address: '127.0.0.1', family: 4 }]
      },
    )
    const connection = await client(proxy.port)
    connection.socket.write(
      `CONNECT rebind.invalid:${upstream.port} HTTP/1.1\r\n` +
        `Host: rebind.invalid:${upstream.port}\r\n\r\n`,
    )
    await waitFor(() => connection.text().includes('200 Connection Established'))
    connection.socket.write('dns-pinned-connect')
    await waitFor(() => connection.text().endsWith('dns-pinned-connect'))

    // `.invalid` cannot resolve via the system. Reaching the echo server proves
    // net.connect used the one injected+validated IP rather than the hostname.
    expect(resolutions).toBe(1)
    expect(upstream.connections()).toBe(1)
  })

  test('absolute-form HTTP also pins DNS and replaces a conflicting Host header', async () => {
    const upstream = await startHttpEcho()
    let resolutions = 0
    const proxy = await startProxy(
      rules({
        deniedDomains: ['unrelated.test'],
        allowLocalBinding: true,
      }),
      undefined,
      async host => {
        resolutions += 1
        expect(host).toBe('rebind.invalid')
        return [{ address: '127.0.0.1', family: 4 }]
      },
    )
    const connection = await client(proxy.port)
    connection.socket.write(
      `GET http://rebind.invalid:${upstream.port}/pinned?q=1 HTTP/1.1\r\n` +
        'Host: metadata.internal\r\nConnection: close\r\n\r\n',
    )
    await waitFor(connection.closed)

    expect(connection.text()).toContain('HTTP/1.1 200 OK')
    expect(connection.text()).toContain('pinned-http-upstream')
    expect(resolutions).toBe(1)
    expect(upstream.requests()).toBe(1)
    expect(upstream.lastHost()).toBe(`rebind.invalid:${upstream.port}`)
  })

  test('stop is idempotent and removes the listener', async () => {
    const proxy = await startProxy(rules({}))
    await proxy.stop()
    await proxy.stop()

    const connected = await new Promise<boolean>(resolve => {
      const socket = netConnect(proxy.port, '127.0.0.1')
      socket.once('connect', () => {
        socket.destroy()
        resolve(true)
      })
      socket.once('error', () => {
        socket.destroy()
        resolve(false)
      })
    })
    expect(connected).toBe(false)
  })
})
