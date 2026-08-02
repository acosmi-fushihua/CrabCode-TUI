// W-TELE-P2-FU-2 (2026-04-25 攻坚 3)：前端 W3C TraceContext 抽象。
//
// 与 backend/internal/observability/context.go 和
// CrabCode/services/observability/tracing.go 的 wire 形状对齐
// (contracts/schemas/observability-context.schema.json)：
//   - trace_id : 32-char lowercase hex (128-bit, 32 bytes)
//   - span_id  : 16-char lowercase hex (64-bit, 16 bytes)
//   - sampled  : boolean
//
// 使用模式：
//   1. 长生命周期消费方（firstPartyEventLogger）通过 `getOrCreateAmbientContext()`
//      获取或建立"会话默认 trace"，每个事件 emit 一份 child span_id。
//   2. 短生命周期一次性 trace（如 click → request 链）调用 `newContext(sessionId?)`
//      获得新 trace + span，调用方负责传递。
//   3. 后端一旦在 WS connect challenge 里下发 trace_id（W-TELE-P2-FU-1 已激活
//      Gateway WS observability），前端可调 `setAmbientContext(received)` 继承
//      上游 trace，实现"用户点击 → 后端 trace"端到端联通。

export interface ObservabilityContext {
  trace_id: string
  span_id: string
  session_id?: string
  sampled: boolean
}

const HEX = '0123456789abcdef'

function randomHex(bytes: number): string {
  // 优先 globalThis.crypto.getRandomValues（浏览器 / Node 18+）
  const cryptoLike = (globalThis as { crypto?: { getRandomValues?: (a: Uint8Array) => Uint8Array } })
    .crypto
  if (cryptoLike && typeof cryptoLike.getRandomValues === 'function') {
    const buf = new Uint8Array(bytes)
    cryptoLike.getRandomValues(buf)
    let out = ''
    for (let i = 0; i < buf.length; i++) {
      const b = buf[i]
      out += HEX[(b >>> 4) & 0x0f] + HEX[b & 0x0f]
    }
    return out
  }
  // Fallback：弱随机（Math.random），仅用于无 crypto 环境，trace 可能碰撞
  let out = ''
  for (let i = 0; i < bytes; i++) {
    const b = Math.floor(Math.random() * 256)
    out += HEX[(b >>> 4) & 0x0f] + HEX[b & 0x0f]
  }
  return out
}

/** 生成 W3C 128-bit trace_id（32 hex 小写）。 */
export function newTraceId(): string {
  return randomHex(16)
}

/** 生成 W3C 64-bit span_id（16 hex 小写）。 */
export function newSpanId(): string {
  return randomHex(8)
}

/**
 * 创建全新 ObservabilityContext，sampled 默认 true。
 */
export function newContext(sessionId?: string): ObservabilityContext {
  return {
    trace_id: newTraceId(),
    span_id: newSpanId(),
    session_id: sessionId,
    sampled: true,
  }
}

/**
 * 在同一 trace 下派生 child span。trace_id / sampled / session_id 继承。
 */
export function childSpan(parent: ObservabilityContext): ObservabilityContext {
  return {
    trace_id: parent.trace_id,
    span_id: newSpanId(),
    session_id: parent.session_id,
    sampled: parent.sampled,
  }
}

// Ambient context — module-level singleton。
//
// 并发模型说明（子代理 round 2 审计后明确）：
//   - JS 单线程 event loop，无 Web Worker 共享内存场景下，下面 read-modify-write
//     序列原子（同步代码块内不会被中断）
//   - 每个浏览器 tab / Node 进程加载独立 module 实例，模块级 _ambient 不跨边界
//   - 若使用 Web Worker / SharedArrayBuffer：worker 加载本模块时会得到独立 _ambient，
//     主线程与 worker 互不影响（结构性隔离 > 锁），符合 trace 应"按 isolate
//     边界划分"的语义
//   - 不采用 Atomics / SharedArrayBuffer 互斥：W3C trace 不要求跨 isolate 全局唯一，
//     只要求同 isolate 内一致；不存在合理的 cross-isolate 共享场景
let _ambient: ObservabilityContext | null = null

/**
 * 返回当前 isolate 内的"环境 trace"。首次调用懒建立。
 * 用于长生命周期消费方（如 firstPartyEventLogger）做事件关联。
 */
export function getOrCreateAmbientContext(sessionId?: string): ObservabilityContext {
  if (!_ambient) {
    _ambient = newContext(sessionId)
  }
  return _ambient
}

/**
 * 强制覆盖环境 trace（例如 WS connect 后从 server 收到的上游 trace）。
 * 传 null 清空。仅在调用方明确知道当前 isolate 应该重新锚点 trace 时调用
 * （如 WS 连接重建 / 用户切换会话）。
 */
export function setAmbientContext(ctx: ObservabilityContext | null): void {
  _ambient = ctx
}

/**
 * 仅用于测试：重置内部状态。
 */
export function _resetAmbientForTest(): void {
  _ambient = null
}
