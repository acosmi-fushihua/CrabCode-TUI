/**
 * gatewayErrorNormalizer.ts — 网关错误归一器(V116.1 P1-3,2026-07-24)。
 *
 * 单一函数族:从 SDK `HTTPError`(statusCode/type/body/errorCode)、SSE 流内
 * error 帧、普通 `Error` 文本中抽取统一的 `{status, errorCode, errorType,
 * message}`。消费方(errors.ts 的文案映射、queryModel.ts 的 HTTPError 分流、
 * SSE `case 'error'`)一律读归一结果,不再各自嗅探 message 文本。
 *
 * 背景:免费用户准入事故中,服务端 500「校验流量包权益失败」被客户端的
 * message.includes 嗅探误映射为「需要会员权益」,把网关故障呈现成用户无权益。
 * 归一器让 status/errorCode 成为唯一分类依据:500 带机器码按码呈现,不再做
 * 会员推断;errorCode 是跨服资产(计划 §五.3),服务端兜底分支一律带机器码。
 *
 * 本文件只做分类,不产文案 —— 文案与映射在 errors.ts(依赖方向:
 * errors.ts → 本文件,不可反向 import,防循环)。
 */
import { HTTPError } from '@acosmi/sdk-ts'

export type NormalizedGatewayError = {
  /** HTTP 状态码;SSE 流内错误 / 无法从文本解出时为 null */
  status: number | null
  /** 后端业务机器码(响应体顶层 errorCode),无则 null */
  errorCode: string | null
  /** anthropic/openai error.type(如 invalid_request_error),无则 null */
  errorType: string | null
  /** 服务端 message(响应体内优先)或错误自身 message */
  message: string
}

/**
 * 网关/Java 侧已知机器码封闭清单(nexus-v4 handler + tk-dist V116.1 契约)。
 * 用封闭清单而非通配大写词扫描:避免把 "HTTP"/"JSON"/"POST" 之类误认为机器码。
 * 新增机器码时同步补此清单(仅影响纯文本兜底路径;HTTPError.errorCode 与 SSE
 * 帧顶层 errorCode 不经过清单,天然透传)。
 */
export const KNOWN_GATEWAY_ERROR_CODES: readonly string[] = [
  'WINDOW_LIMIT_EXCEEDED',
  'LIMIT_POLICY_DENIED',
  'INSUFFICIENT_BALANCE',
  'WINDOW_ADMISSION_UNAVAILABLE',
  'HOLD_INTERNAL_ERROR',
]

function scanKnownCode(text: string): string | null {
  for (const code of KNOWN_GATEWAY_ERROR_CODES) {
    if (text.includes(code)) return code
  }
  return null
}

/** 宽容解析 JSON 响应体;非 JSON / 解析失败返回 null。 */
function parseBodyObject(body: string): Record<string, unknown> | null {
  const trimmed = body.trim()
  if (!trimmed.startsWith('{')) return null
  try {
    const parsed = JSON.parse(trimmed) as unknown
    return typeof parsed === 'object' && parsed !== null
      ? (parsed as Record<string, unknown>)
      : null
  } catch {
    return null
  }
}

/** 从响应体对象里取服务端 message(nexus gin.H 与 anthropic 两种形态)。 */
function messageFromBody(obj: Record<string, unknown>): string | null {
  if (typeof obj.message === 'string' && obj.message.length > 0) {
    return obj.message
  }
  // gin.H 老形态 {code, msg}
  if (typeof obj.msg === 'string' && obj.msg.length > 0) {
    return obj.msg
  }
  const inner = obj.error
  if (typeof inner === 'object' && inner !== null) {
    const m = (inner as Record<string, unknown>).message
    if (typeof m === 'string' && m.length > 0) return m
  }
  return null
}

/** 从响应体对象里取顶层 errorCode(anthropic 形态下也是顶层,见 managed_model.go)。 */
function errorCodeFromBody(obj: Record<string, unknown>): string | null {
  const code = obj.errorCode
  return typeof code === 'string' && code.length > 0 ? code : null
}

function errorTypeFromBody(obj: Record<string, unknown>): string | null {
  const inner = obj.error
  if (typeof inner === 'object' && inner !== null) {
    const t = (inner as Record<string, unknown>).type
    if (typeof t === 'string' && t.length > 0) return t
  }
  if (typeof obj.type === 'string' && obj.type !== 'error' && obj.type.length > 0) {
    return obj.type
  }
  return null
}

/**
 * 归一任意错误对象。识别顺序:
 *   1. SDK HTTPError(含 CannotRetryError 等包装器请调用方先解包再传入)
 *   2. 普通 Error —— 从文本尽力抽取 "HTTP <status>" 与已知机器码
 *   3. 其他 unknown —— String() 兜底
 */
export function normalizeGatewayError(err: unknown): NormalizedGatewayError {
  if (err instanceof HTTPError) {
    const bodyObj = parseBodyObject(err.body ?? '')
    const errorCode =
      (typeof err.errorCode === 'string' && err.errorCode.length > 0
        ? err.errorCode
        : null) ??
      (bodyObj ? errorCodeFromBody(bodyObj) : null) ??
      scanKnownCode(`${err.body ?? ''} ${err.message ?? ''}`)
    const errorType =
      (err.type && err.type.length > 0 ? err.type : null) ??
      (bodyObj ? errorTypeFromBody(bodyObj) : null)
    const message =
      (bodyObj ? messageFromBody(bodyObj) : null) ?? err.message ?? ''
    return { status: err.statusCode, errorCode, errorType, message }
  }
  if (err instanceof Error) {
    const text = err.message ?? ''
    const statusMatch = text.match(/HTTP[\s:]+([1-5]\d{2})\b/i)
    return {
      status: statusMatch ? Number(statusMatch[1]) : null,
      errorCode: scanKnownCode(text),
      errorType: null,
      message: text,
    }
  }
  return {
    status: null,
    errorCode: null,
    errorType: null,
    message: String(err),
  }
}

/**
 * 归一 SSE 流内 error 帧(queryModel `case 'error'`)。
 *
 * 帧形态(nexus-v4 managed_model.go anthropicGatewayError + V116.1 errorCode):
 *   `{type:'error', error:{type,message}, errorCode?}`
 * 上游 anthropic 原生 error 帧同形但无顶层 errorCode。
 */
export function normalizeSseErrorEvent(part: unknown): NormalizedGatewayError {
  const obj =
    typeof part === 'object' && part !== null
      ? (part as Record<string, unknown>)
      : {}
  const inner =
    typeof obj.error === 'object' && obj.error !== null
      ? (obj.error as Record<string, unknown>)
      : {}
  const message =
    typeof inner.message === 'string' && inner.message.length > 0
      ? inner.message
      : '(stream error without message)'
  const errorType =
    typeof inner.type === 'string' && inner.type.length > 0 ? inner.type : null
  const errorCode =
    (typeof obj.errorCode === 'string' && obj.errorCode.length > 0
      ? obj.errorCode
      : null) ?? scanKnownCode(message)
  return { status: null, errorCode, errorType, message }
}
