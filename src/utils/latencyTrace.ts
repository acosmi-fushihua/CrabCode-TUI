// TypeScript runtime latency tracing.
//
// 默认关闭。开启方式：设置 env `CRABCODE_LATENCY_TRACE=1`。
//
// 输出走 console.error（stderr）— Bun/Node 进程 stderr 不会污染协议 stdout，
// 也不被 logForDebugging 缓冲。tag 前缀 `[lat][ts]` 方便 grep。

let _enabledCache: boolean | null = null

export function latTraceEnabled(): boolean {
  if (_enabledCache !== null) return _enabledCache
  const raw = process.env.CRABCODE_LATENCY_TRACE
  _enabledCache = typeof raw === 'string' && raw !== '' && raw !== '0'
  return _enabledCache
}

export function __crabcodeLatencyTraceForTests__(enabled: boolean): void {
  _enabledCache = enabled
}

export function latTrace(tag: string, payload?: Record<string, unknown>): void {
  if (!latTraceEnabled()) return
  if (payload === undefined) {
    console.error(`[lat][ts] ${tag}`)
    return
  }
  try {
    console.error(`[lat][ts] ${tag}`, payload)
  } catch {
    console.error(`[lat][ts] ${tag} <unserializable payload>`)
  }
}

export function latNow(): number {
  const bun = (globalThis as { Bun?: { nanoseconds(): number } }).Bun
  if (bun && typeof bun.nanoseconds === 'function') {
    return bun.nanoseconds() / 1e6
  }
  try {
    return performance.now()
  } catch {
    return Date.now()
  }
}
