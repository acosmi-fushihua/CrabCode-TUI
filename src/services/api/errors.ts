import type {
  APIError,
  BetaMessage,
  BetaStopReason,
} from '../../types/api-types.js'
import {
  APIConnectionError,
  APIConnectionTimeoutError,
} from '../../errors/api-errors.js'
import { HTTPError } from '@acosmi/sdk-ts'
import {
  normalizeGatewayError,
  type NormalizedGatewayError,
} from './gatewayErrorNormalizer.js'

import type { SDKAssistantMessageError } from 'src/entrypoints/agentSdkTypes.js'
import type {
  AssistantMessage,
  Message,
  UserMessage,
} from 'src/types/message.js'
import {
  getAcosmiApiKeyWithSource,
  getOauthAccountInfo,
} from 'src/utils/auth.js'
import {
  createAssistantAPIErrorMessage,
  NO_RESPONSE_REQUESTED,
} from 'src/utils/messages.js'
import { getDefaultMainLoopModelSetting } from 'src/utils/model/model.js'
import { getCachedDefaultModelId } from 'src/utils/model/modelCapabilities.js'
import { getAPIProvider, isOfficialProvider } from 'src/utils/model/providers.js'
import { getIsNonInteractiveSession } from '../../bootstrap/state.js'
import {
  API_PDF_MAX_PAGES,
  PDF_TARGET_RAW_SIZE,
} from '../../constants/apiLimits.js'
import { isEnvTruthy } from '../../utils/envUtils.js'
import { formatFileSize } from '../../utils/format.js'
import { ImageResizeError } from '../../utils/imageResizer.js'
import { ImageSizeError } from '../../utils/imageValidation.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from '../analytics/index.js'
import { extractConnectionErrorDetails, formatAPIError } from './errorUtils.js'

export const API_ERROR_MESSAGE_PREFIX = 'API Error'

export function startsWithApiErrorPrefix(text: string): boolean {
  return (
    text.startsWith(API_ERROR_MESSAGE_PREFIX) ||
    text.startsWith(`Please run /login · ${API_ERROR_MESSAGE_PREFIX}`)
  )
}
export const PROMPT_TOO_LONG_ERROR_MESSAGE = 'Prompt is too long'

export function isPromptTooLongMessage(msg: AssistantMessage): boolean {
  if (!msg.isApiErrorMessage) {
    return false
  }
  const content = msg.message.content
  if (!Array.isArray(content)) {
    return false
  }
  return content.some(
    block =>
      block.type === 'text' &&
      block.text.startsWith(PROMPT_TOO_LONG_ERROR_MESSAGE),
  )
}

/**
 * Parse actual/limit token counts from a raw prompt-too-long API error
 * message like "prompt is too long: 137500 tokens > 135000 maximum".
 * The raw string may be wrapped in SDK prefixes or JSON envelopes, or
 * have different casing (Vertex), so this is intentionally lenient.
 */
export function parsePromptTooLongTokenCounts(rawMessage: string): {
  actualTokens: number | undefined
  limitTokens: number | undefined
} {
  const match = rawMessage.match(
    /prompt is too long[^0-9]*(\d+)\s*tokens?\s*>\s*(\d+)/i,
  )
  return {
    actualTokens: match ? parseInt(match[1]!, 10) : undefined,
    limitTokens: match ? parseInt(match[2]!, 10) : undefined,
  }
}

/**
 * Returns how many tokens over the limit a prompt-too-long error reports,
 * or undefined if the message isn't PTL or its errorDetails are unparseable.
 * Reactive compact uses this gap to jump past multiple groups in one retry
 * instead of peeling one-at-a-time.
 */
export function getPromptTooLongTokenGap(
  msg: AssistantMessage,
): number | undefined {
  if (!isPromptTooLongMessage(msg) || !msg.errorDetails) {
    return undefined
  }
  const { actualTokens, limitTokens } = parsePromptTooLongTokenCounts(
    msg.errorDetails,
  )
  if (actualTokens === undefined || limitTokens === undefined) {
    return undefined
  }
  const gap = actualTokens - limitTokens
  return gap > 0 ? gap : undefined
}

/**
 * Is this raw API error text a media-size rejection that stripImagesFromMessages
 * can fix? Reactive compact's summarize retry uses this to decide whether to
 * strip and retry (media error) or bail (anything else).
 *
 * Patterns MUST stay in sync with the getAssistantMessageFromError branches
 * that populate errorDetails (~L523 PDF, ~L560 image, ~L573 many-image) and
 * the classifyAPIError branches (~L929-946). The closed loop: errorDetails is
 * only set after those branches already matched these same substrings, so
 * isMediaSizeError(errorDetails) is tautologically true for that path. API
 * wording drift causes graceful degradation (errorDetails stays undefined,
 * caller short-circuits), not a false negative.
 */
export function isMediaSizeError(raw: string): boolean {
  return (
    (raw.includes('image exceeds') && raw.includes('maximum')) ||
    (raw.includes('image dimensions exceed') && raw.includes('many-image')) ||
    /maximum of \d+ PDF pages/.test(raw)
  )
}

/**
 * Message-level predicate: is this assistant message a media-size rejection?
 * Parallel to isPromptTooLongMessage. Checks errorDetails (the raw API error
 * string populated by the getAssistantMessageFromError branches at ~L523/560/573)
 * rather than content text, since media errors have per-variant content strings.
 */
export function isMediaSizeErrorMessage(msg: AssistantMessage): boolean {
  return (
    msg.isApiErrorMessage === true &&
    msg.errorDetails !== undefined &&
    isMediaSizeError(msg.errorDetails)
  )
}
export const CREDIT_BALANCE_TOO_LOW_ERROR_MESSAGE = 'Credit balance is too low'
export const INVALID_API_KEY_ERROR_MESSAGE = 'Not logged in · Please run /login'
export const INVALID_API_KEY_ERROR_MESSAGE_EXTERNAL =
  'Invalid API key · Fix external API key'
export const ORG_DISABLED_ERROR_MESSAGE_ENV_KEY_WITH_OAUTH =
  'Your ACOSMI_API_KEY belongs to a disabled organization · Unset the environment variable to use your subscription instead'
export const ORG_DISABLED_ERROR_MESSAGE_ENV_KEY =
  'Your ACOSMI_API_KEY belongs to a disabled organization · Update or unset the environment variable'
export const TOKEN_REVOKED_ERROR_MESSAGE =
  'OAuth token revoked · Please run /login'
export const CCR_AUTH_ERROR_MESSAGE =
  'Authentication error · This may be a temporary network issue, please try again'
export const REPEATED_529_ERROR_MESSAGE = 'Repeated 529 Overloaded errors'
export const CUSTOM_OFF_SWITCH_MESSAGE =
  'The selected model is experiencing high load, please use /model to switch to a different model'
// W-TUI-LOCKED-GATEWAY-MODELS D6 (2026-07-05): membership-entitlement block.
// Stable core marker — the queryModel locked-model pre-flight throws an Error
// whose message embeds it (plus the model name), and getAssistantMessageFromError
// matches on it (same throw-then-match pattern as CUSTOM_OFF_SWITCH_MESSAGE).
export const GATEWAY_ENTITLEMENT_CORE_MESSAGE =
  '需要会员权益（会员已到期或档位不足）'
export function getGatewayEntitlementErrorMessage(model: string): string {
  const switchCmd = getIsNonInteractiveSession() ? '--model' : '/model'
  return `模型 ${model} ${GATEWAY_ENTITLEMENT_CORE_MESSAGE}。运行 ${switchCmd} 切换到可用模型，或升级会员后重试。`
}

// ============================================================================
// V116.1 P1-3 (2026-07-24) 网关错误统一文案 —— 免费用户准入事故修复。
// 分类唯一依据 = 归一器抽出的 status/errorCode(errorCode 是跨服资产,计划 §五.3);
// 禁止再用 message 文本嗅探推断会员状态(事故根因:500「校验流量包权益失败」被
// 嗅探成「需要会员权益」,免费用户被误导为无权益)。
// ============================================================================

/** 402 / INSUFFICIENT_BALANCE:额度用尽,与「无权益/档位不足」是两回事。 */
export const QUOTA_EXHAUSTED_MESSAGE =
  '当前账号可用额度已用尽（免费每日额度或流量包余额不足）。免费额度次日自动恢复；也可前往 acosmi.com 充值流量包或升级会员后重试。(INSUFFICIENT_BALANCE)'

/** 429 / WINDOW_LIMIT_EXCEEDED:滚动窗口限额(文案自 queryModel 原位提出,逐字不变)。 */
export const WINDOW_LIMIT_EXCEEDED_MESSAGE =
  '本账号已达到滚动用量限额。已开始的任务可在共享完成空间内收尾；新会话或新任务需等待窗口逐步恢复后再发起。\n(WINDOW_LIMIT_EXCEEDED)'

/** 5xx 无机器码:通用上游故障,明确否定「权益问题」的旧误导方向。 */
export const GATEWAY_UPSTREAM_FAULT_MESSAGE =
  '网关或上游服务暂时不可用（服务端内部错误）。这不是账号权益问题，请稍后重试；若持续失败请联系支持。'

/** V116.1 P0-3:未绑定手机号的固定文案(计划 §三 P0-3 定稿,逐字)。 */
export const PHONE_BINDING_REQUIRED_MESSAGE =
  '当前账号尚未绑定手机号。请前往 acosmi.com 个人中心完成手机绑定后重新登录，即可领取注册赠送权益。'

/** 5xx 带机器码:按码呈现,附服务端原话,不做任何会员/权益推断。 */
export function getGatewayServerFaultMessage(
  errorCode: string,
  serverMessage: string,
): string {
  const detail = serverMessage ? `：${serverMessage}` : ''
  return `网关服务错误 (${errorCode})${detail}。这不是账号权益问题，请稍后重试；若持续失败请联系支持并提供此错误码。`
}

/**
 * 五路统一映射:归一结果 → 用户文案 + SDK error 分类。未命中返回 null,
 * 调用方继续走各自的既有分支。errors.ts 与 queryModel.ts 流式 catch、
 * SSE `case 'error'` 三处共用,保证同一错误在任何路径文案一致。
 */
export function getGatewayErrorDisplay(
  n: NormalizedGatewayError,
  model: string,
): { content: string; error: SDKAssistantMessageError } | null {
  if (n.status === 402 || n.errorCode === 'INSUFFICIENT_BALANCE') {
    return { content: QUOTA_EXHAUSTED_MESSAGE, error: 'billing_error' }
  }
  if (n.errorCode === 'LIMIT_POLICY_DENIED') {
    return {
      content: getGatewayEntitlementErrorMessage(model),
      error: 'invalid_request',
    }
  }
  if (n.errorCode === 'WINDOW_LIMIT_EXCEEDED') {
    return { content: WINDOW_LIMIT_EXCEEDED_MESSAGE, error: 'rate_limit' }
  }
  if (n.status !== null && n.status >= 500) {
    return n.errorCode
      ? {
          content: getGatewayServerFaultMessage(n.errorCode, n.message),
          error: 'server_error',
        }
      : { content: GATEWAY_UPSTREAM_FAULT_MESSAGE, error: 'server_error' }
  }
  return null
}

export const API_TIMEOUT_ERROR_MESSAGE = 'Request timed out'
export function getPdfTooLargeErrorMessage(): string {
  const limits = `max ${API_PDF_MAX_PAGES} pages, ${formatFileSize(PDF_TARGET_RAW_SIZE)}`
  return getIsNonInteractiveSession()
    ? `PDF too large (${limits}). Try reading the file a different way (e.g., extract text with pdftotext).`
    : `PDF too large (${limits}). Double press esc to go back and try again, or use pdftotext to convert to text first.`
}
export function getPdfPasswordProtectedErrorMessage(): string {
  return getIsNonInteractiveSession()
    ? 'PDF is password protected. Try using a CLI tool to extract or convert the PDF.'
    : 'PDF is password protected. Please double press esc to edit your message and try again.'
}
export function getPdfInvalidErrorMessage(): string {
  return getIsNonInteractiveSession()
    ? 'The PDF file was not valid. Try converting it to text first (e.g., pdftotext).'
    : 'The PDF file was not valid. Double press esc to go back and try again with a different file.'
}
export function getImageTooLargeErrorMessage(): string {
  return getIsNonInteractiveSession()
    ? 'Image was too large. Try resizing the image or using a different approach.'
    : 'Image was too large. Double press esc to go back and try again with a smaller image.'
}
export function getRequestTooLargeErrorMessage(): string {
  const limits = `max ${formatFileSize(PDF_TARGET_RAW_SIZE)}`
  return getIsNonInteractiveSession()
    ? `Request too large (${limits}). Try with a smaller file.`
    : `Request too large (${limits}). Double press esc to go back and try with a smaller file.`
}
export const OAUTH_ORG_NOT_ALLOWED_ERROR_MESSAGE =
  'Your account does not have access to CrabCode. Please run /login.'

export function getTokenRevokedErrorMessage(): string {
  return getIsNonInteractiveSession()
    ? 'Your account does not have access to CrabCode. Please login again or contact your administrator.'
    : TOKEN_REVOKED_ERROR_MESSAGE
}

export function getOauthOrgNotAllowedErrorMessage(): string {
  return getIsNonInteractiveSession()
    ? 'Your organization does not have access to CrabCode. Please login again or contact your administrator.'
    : OAUTH_ORG_NOT_ALLOWED_ERROR_MESSAGE
}

/**
 * Check if we're in CCR (CrabCode Remote) mode.
 * In CCR mode, auth is handled via JWTs provided by the infrastructure,
 * not via /login. Transient auth errors should suggest retrying, not logging in.
 */
function isCCRMode(): boolean {
  return isEnvTruthy(process.env.CRABCODE_REMOTE)
}

// Temp helper to log tool_use/tool_result mismatch errors
function logToolUseToolResultMismatch(
  toolUseId: string,
  messages: Message[],
  messagesForAPI: (UserMessage | AssistantMessage)[],
): void {
  try {
    // Find tool_use in normalized messages
    let normalizedIndex = -1
    for (let i = 0; i < messagesForAPI.length; i++) {
      const msg = messagesForAPI[i]
      if (!msg) continue
      const content = msg.message.content
      if (Array.isArray(content)) {
        for (const block of content) {
          if (
            block.type === 'tool_use' &&
            'id' in block &&
            block.id === toolUseId
          ) {
            normalizedIndex = i
            break
          }
        }
      }
      if (normalizedIndex !== -1) break
    }

    // Find tool_use in original messages
    let originalIndex = -1
    for (let i = 0; i < messages.length; i++) {
      const msg = messages[i]
      if (!msg) continue
      if (msg.type === 'assistant' && 'message' in msg) {
        const content = msg.message.content
        if (Array.isArray(content)) {
          for (const block of content) {
            if (
              block.type === 'tool_use' &&
              'id' in block &&
              block.id === toolUseId
            ) {
              originalIndex = i
              break
            }
          }
        }
      }
      if (originalIndex !== -1) break
    }

    // Build normalized sequence
    const normalizedSeq: string[] = []
    for (let i = normalizedIndex + 1; i < messagesForAPI.length; i++) {
      const msg = messagesForAPI[i]
      if (!msg) continue
      const content = msg.message.content
      if (Array.isArray(content)) {
        for (const block of content) {
          const role = msg.message.role
          if (block.type === 'tool_use' && 'id' in block) {
            normalizedSeq.push(`${role}:tool_use:${block.id}`)
          } else if (block.type === 'tool_result' && 'tool_use_id' in block) {
            normalizedSeq.push(`${role}:tool_result:${block.tool_use_id}`)
          } else if (block.type === 'text') {
            normalizedSeq.push(`${role}:text`)
          } else if (block.type === 'thinking') {
            normalizedSeq.push(`${role}:thinking`)
          } else if (block.type === 'image') {
            normalizedSeq.push(`${role}:image`)
          } else {
            normalizedSeq.push(`${role}:${block.type}`)
          }
        }
      } else if (typeof content === 'string') {
        normalizedSeq.push(`${msg.message.role}:string_content`)
      }
    }

    // Build pre-normalized sequence
    const preNormalizedSeq: string[] = []
    for (let i = originalIndex + 1; i < messages.length; i++) {
      const msg = messages[i]
      if (!msg) continue

      switch (msg.type) {
        case 'user':
        case 'assistant': {
          if ('message' in msg) {
            const content = msg.message.content
            if (Array.isArray(content)) {
              for (const block of content) {
                const role = msg.message.role
                if (block.type === 'tool_use' && 'id' in block) {
                  preNormalizedSeq.push(`${role}:tool_use:${block.id}`)
                } else if (
                  block.type === 'tool_result' &&
                  'tool_use_id' in block
                ) {
                  preNormalizedSeq.push(
                    `${role}:tool_result:${block.tool_use_id}`,
                  )
                } else if (block.type === 'text') {
                  preNormalizedSeq.push(`${role}:text`)
                } else if (block.type === 'thinking') {
                  preNormalizedSeq.push(`${role}:thinking`)
                } else if (block.type === 'image') {
                  preNormalizedSeq.push(`${role}:image`)
                } else {
                  preNormalizedSeq.push(`${role}:${block.type}`)
                }
              }
            } else if (typeof content === 'string') {
              preNormalizedSeq.push(`${msg.message.role}:string_content`)
            }
          }
          break
        }
        case 'attachment':
          if ('attachment' in msg) {
            preNormalizedSeq.push(`attachment:${msg.attachment.type}`)
          }
          break
        case 'system':
          if ('subtype' in msg) {
            preNormalizedSeq.push(`system:${msg.subtype}`)
          }
          break
        case 'progress':
          if (
            'progress' in msg &&
            msg.progress &&
            typeof msg.progress === 'object' &&
            'type' in msg.progress
          ) {
            preNormalizedSeq.push(`progress:${msg.progress.type ?? 'unknown'}`)
          } else {
            preNormalizedSeq.push('progress:unknown')
          }
          break
      }
    }

    // Log to Statsig
    logEvent('tengu_tool_use_tool_result_mismatch_error', {
      toolUseId:
        toolUseId as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      normalizedSequence: normalizedSeq.join(
        ', ',
      ) as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      preNormalizedSequence: preNormalizedSeq.join(
        ', ',
      ) as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      normalizedMessageCount: messagesForAPI.length,
      originalMessageCount: messages.length,
      normalizedToolUseIndex: normalizedIndex,
      originalToolUseIndex: originalIndex,
    })
  } catch (_) {
    // Ignore errors in debug logging
  }
}

/**
 * Type guard to check if a value is a valid Message response from the API
 */
export function isValidAPIMessage(value: unknown): value is BetaMessage {
  return (
    typeof value === 'object' &&
    value !== null &&
    'content' in value &&
    'model' in value &&
    'usage' in value &&
    Array.isArray((value as BetaMessage).content) &&
    typeof (value as BetaMessage).model === 'string' &&
    typeof (value as BetaMessage).usage === 'object'
  )
}

export function getAssistantMessageFromError(
  error: unknown,
  model: string,
  options?: {
    messages?: Message[]
    messagesForAPI?: (UserMessage | AssistantMessage)[]
  },
): AssistantMessage {
  // Check for SDK timeout errors
  if (
    error instanceof APIConnectionTimeoutError ||
    (error instanceof APIConnectionError &&
      error.message.toLowerCase().includes('timeout'))
  ) {
    return createAssistantAPIErrorMessage({
      content: API_TIMEOUT_ERROR_MESSAGE,
      error: 'unknown',
    })
  }

  // Check for image size/resize errors (thrown before API call during validation)
  // Use getImageTooLargeErrorMessage() to show "esc esc" hint for CLI users
  // but a generic message for SDK users (non-interactive mode)
  if (error instanceof ImageSizeError || error instanceof ImageResizeError) {
    return createAssistantAPIErrorMessage({
      content: getImageTooLargeErrorMessage(),
    })
  }

  // Check for emergency capacity off switch (max-effort tier PAYG users)
  if (
    error instanceof Error &&
    error.message.includes(CUSTOM_OFF_SWITCH_MESSAGE)
  ) {
    return createAssistantAPIErrorMessage({
      content: CUSTOM_OFF_SWITCH_MESSAGE,
      error: 'rate_limit',
    })
  }

  // V116.1 P1-3 (2026-07-24): 网关权益/计费/限额/服务端故障错误统一走归一器,
  // 替换旧「四形态一文案」块。旧块的两处文本嗅探(500+「校验流量包权益」推断为
  // 会员问题、裸 LIMIT_POLICY_DENIED 子串)正是免费用户准入事故的客户端根因 ——
  // 网关故障被呈现成「需要会员权益」。现分类唯一依据 = status/errorCode:
  //   402 / INSUFFICIENT_BALANCE      → 额度用尽引导(不再映射为档位文案)
  //   LIMIT_POLICY_DENIED(403/文本)  → 档位文案(既有)
  //   WINDOW_LIMIT_EXCEEDED(429)     → 窗口文案(既有)
  //   5xx 带机器码                    → 按码呈现,明示非权益问题
  //   5xx 无码                        → 通用上游故障
  // 位置保持在通用 401/403「Please run /login」分支之前(登录态不是这些错误的病因)。
  // 锁定预检 marker 是本地构造的 Error,形状可控,保留字符串匹配:
  if (
    error instanceof Error &&
    error.message.includes(GATEWAY_ENTITLEMENT_CORE_MESSAGE)
  ) {
    return createAssistantAPIErrorMessage({
      content: getGatewayEntitlementErrorMessage(model),
      error: 'invalid_request',
    })
  }
  // V116.1 P0-3:手机绑定预检 marker(queryModel 本地构造,throw-then-match
  // 与上方 locked 预检同模式)。
  if (
    error instanceof Error &&
    error.message.includes(PHONE_BINDING_REQUIRED_MESSAGE)
  ) {
    return createAssistantAPIErrorMessage({
      content: PHONE_BINDING_REQUIRED_MESSAGE,
      error: 'invalid_request',
    })
  }
  {
    const gatewayDisplay = getGatewayErrorDisplay(
      normalizeGatewayError(error),
      model,
    )
    if (gatewayDisplay) {
      return createAssistantAPIErrorMessage(gatewayDisplay)
    }
  }

  // Handle prompt too long errors (Vertex returns 413, direct API returns 400)
  // Use case-insensitive check since Vertex returns "Prompt is too long" (capitalized)
  if (
    error instanceof Error &&
    error.message.toLowerCase().includes('prompt is too long')
  ) {
    // Content stays generic (UI matches on exact string). The raw error with
    // token counts goes into errorDetails — reactive compact's retry loop
    // parses the gap from there via getPromptTooLongTokenGap.
    return createAssistantAPIErrorMessage({
      content: PROMPT_TOO_LONG_ERROR_MESSAGE,
      error: 'invalid_request',
      errorDetails: error.message,
    })
  }

  // Check for PDF page limit errors
  if (
    error instanceof Error &&
    /maximum of \d+ PDF pages/.test(error.message)
  ) {
    return createAssistantAPIErrorMessage({
      content: getPdfTooLargeErrorMessage(),
      error: 'invalid_request',
      errorDetails: error.message,
    })
  }

  // Check for password-protected PDF errors
  if (
    error instanceof Error &&
    error.message.includes('The PDF specified is password protected')
  ) {
    return createAssistantAPIErrorMessage({
      content: getPdfPasswordProtectedErrorMessage(),
      error: 'invalid_request',
    })
  }

  // Check for invalid PDF errors (e.g., HTML file renamed to .pdf)
  // Without this handler, invalid PDF document blocks persist in conversation
  // context and cause every subsequent API call to fail with 400.
  if (
    error instanceof Error &&
    error.message.includes('The PDF specified was not valid')
  ) {
    return createAssistantAPIErrorMessage({
      content: getPdfInvalidErrorMessage(),
      error: 'invalid_request',
    })
  }

  // V116.1 P1-3 (2026-07-24) 死分支处置:本地 APIError 分类族全仓零构造点,
  // 原 `instanceof APIError` 分支自 Phase 3 SDK 迁移起从未命中(真实错误是
  // SDK HTTPError)。处置三分法:
  //   - 删除:image-exceeds 400 / many-image 400 / AFK beta 400 / 413(嗅探
  //     Anthropic 一方基础设施报文,经本网关的真实形态未经验证,盲激活风险
  //     大于收益;删除 = 与线上现状零行为差);
  //   - 复活为 HTTPError:tool_use 配对三连(会话腐坏 + /rewind 恢复引导,
  //     报文经网关 anthropic 直通线原样透传,埋点丢失多月需找回);
  //   - 权益/计费/限额/5xx:上方归一器五路(status/errorCode 分类)。
  // Check for tool_use/tool_result concurrency error
  if (
    error instanceof HTTPError &&
    error.statusCode === 400 &&
    error.message.includes(
      '`tool_use` ids were found without `tool_result` blocks immediately after',
    )
  ) {
    // Log to Statsig if we have the message context
    if (options?.messages && options?.messagesForAPI) {
      const toolUseIdMatch = error.message.match(/toolu_[a-zA-Z0-9]+/)
      const toolUseId = toolUseIdMatch ? toolUseIdMatch[0] : null
      if (toolUseId) {
        logToolUseToolResultMismatch(
          toolUseId,
          options.messages,
          options.messagesForAPI,
        )
      }
    }

    if (process.env.USER_TYPE === 'ant') {
      const baseMessage = `API Error: 400 ${error.message}\n\nRun /share and post the JSON file to ${MACRO.FEEDBACK_CHANNEL}.`
      const rewindInstruction = getIsNonInteractiveSession()
        ? ''
        : ' Then, use /rewind to recover the conversation.'
      return createAssistantAPIErrorMessage({
        content: baseMessage + rewindInstruction,
        error: 'invalid_request',
      })
    } else {
      const baseMessage = 'API Error: 400 due to tool use concurrency issues.'
      const rewindInstruction = getIsNonInteractiveSession()
        ? ''
        : ' Run /rewind to recover the conversation.'
      return createAssistantAPIErrorMessage({
        content: baseMessage + rewindInstruction,
        error: 'invalid_request',
      })
    }
  }

  if (
    error instanceof HTTPError &&
    error.statusCode === 400 &&
    error.message.includes('unexpected `tool_use_id` found in `tool_result`')
  ) {
    logEvent('tengu_unexpected_tool_result', {})
  }

  // Duplicate tool_use IDs (CC-1212). ensureToolResultPairing strips these
  // before send, so hitting this means a new corruption path slipped through.
  // Log for root-causing, and give users a recovery path instead of deadlock.
  if (
    error instanceof HTTPError &&
    error.statusCode === 400 &&
    error.message.includes('`tool_use` ids must be unique')
  ) {
    logEvent('tengu_duplicate_tool_use_id', {})
    const rewindInstruction = getIsNonInteractiveSession()
      ? ''
      : ' Run /rewind to recover the conversation.'
    return createAssistantAPIErrorMessage({
      content: `API Error: 400 duplicate tool_use ID in conversation history.${rewindInstruction}`,
      error: 'invalid_request',
      errorDetails: error.message,
    })
  }

  // Check for invalid model name error for Ant users. CrabCode may be
  // defaulting to a custom internal-only model for Ants, and there might be
  // Ants using new or unknown org IDs that haven't been gated in.
  if (
    process.env.USER_TYPE === 'ant' &&
    !process.env.ACOSMI_MODEL &&
    error instanceof Error &&
    error.message.toLowerCase().includes('invalid model name')
  ) {
    // Get organization ID from config - only use OAuth account data when actively using OAuth
    const orgId = getOauthAccountInfo()?.organizationUuid
    const baseMsg = `[ANT-ONLY] Your org isn't gated into the \`${model}\` model. Either run \`crabcode\` with \`ACOSMI_MODEL=${getDefaultMainLoopModelSetting()}\``
    const msg = orgId
      ? `${baseMsg} or share your orgId (${orgId}) in ${MACRO.FEEDBACK_CHANNEL} for help getting access.`
      : `${baseMsg} or reach out in ${MACRO.FEEDBACK_CHANNEL} for help getting access.`

    return createAssistantAPIErrorMessage({
      content: msg,
      error: 'invalid_request',
    })
  }

  if (
    error instanceof Error &&
    error.message.includes('Your credit balance is too low')
  ) {
    return createAssistantAPIErrorMessage({
      content: CREDIT_BALANCE_TOO_LOW_ERROR_MESSAGE,
      error: 'billing_error',
    })
  }
  // V116.1 P1-3:原「Organization has been disabled」400 分支为 `instanceof
  // APIError` 死分支(Anthropic 一方报文,经本网关形态未验证),按三分法删除。

  if (
    error instanceof Error &&
    error.message.toLowerCase().includes('x-api-key')
  ) {
    // In CCR mode, auth is via JWTs - this is likely a transient network issue
    if (isCCRMode()) {
      return createAssistantAPIErrorMessage({
        error: 'authentication_failed',
        content: CCR_AUTH_ERROR_MESSAGE,
      })
    }

    // Check if the API key is from an external source
    const { source } = getAcosmiApiKeyWithSource()
    const isExternalSource =
      source === 'ACOSMI_API_KEY' || source === 'apiKeyHelper'

    return createAssistantAPIErrorMessage({
      error: 'authentication_failed',
      content: isExternalSource
        ? INVALID_API_KEY_ERROR_MESSAGE_EXTERNAL
        : INVALID_API_KEY_ERROR_MESSAGE,
    })
  }

  // V116.1 P1-3:以下 401/403/404 分支按真实错误形态(SDK HTTPError)复活 ——
  // 原 `instanceof APIError` 写法自 Phase 3 起从未命中,登录引导/模型引导 UX
  // 静默失效多月(与化石表同类的「悄悄失效」,计划 §五.4)。
  // Check for OAuth token revocation error
  if (
    error instanceof HTTPError &&
    error.statusCode === 403 &&
    error.message.includes('OAuth token has been revoked')
  ) {
    return createAssistantAPIErrorMessage({
      error: 'authentication_failed',
      content: getTokenRevokedErrorMessage(),
    })
  }

  // Check for OAuth organization not allowed error
  if (
    error instanceof HTTPError &&
    (error.statusCode === 401 || error.statusCode === 403) &&
    error.message.includes(
      'OAuth authentication is currently not allowed for this organization',
    )
  ) {
    return createAssistantAPIErrorMessage({
      error: 'authentication_failed',
      content: getOauthOrgNotAllowedErrorMessage(),
    })
  }

  // Generic handler for other 401/403 authentication errors
  if (
    error instanceof HTTPError &&
    (error.statusCode === 401 || error.statusCode === 403)
  ) {
    // In CCR mode, auth is via JWTs - this is likely a transient network issue
    if (isCCRMode()) {
      return createAssistantAPIErrorMessage({
        error: 'authentication_failed',
        content: CCR_AUTH_ERROR_MESSAGE,
      })
    }

    return createAssistantAPIErrorMessage({
      error: 'authentication_failed',
      content: getIsNonInteractiveSession()
        ? `Failed to authenticate. ${API_ERROR_MESSAGE_PREFIX}: ${error.message}`
        : `Please run /login · ${API_ERROR_MESSAGE_PREFIX}: ${error.message}`,
    })
  }

  // 404 Not Found — usually means the selected model doesn't exist or isn't
  // available. Guide the user to /model so they can pick a valid one.
  // For 3P users, suggest a specific fallback model they can try.
  if (error instanceof HTTPError && error.statusCode === 404) {
    const switchCmd = getIsNonInteractiveSession() ? '--model' : '/model'
    const fallbackSuggestion = get3PModelFallbackSuggestion(model)
    return createAssistantAPIErrorMessage({
      content: fallbackSuggestion
        ? `The model ${model} is not available on your ${getAPIProvider()} deployment. Try ${switchCmd} to switch to ${fallbackSuggestion}, or ask your admin to enable this model.`
        : `There's an issue with the selected model (${model}). It may not exist or you may not have access to it. Run ${switchCmd} to pick a different model.`,
      error: 'invalid_request',
    })
  }

  // Connection errors (non-timeout) — use formatAPIError for detailed messages
  if (error instanceof APIConnectionError) {
    return createAssistantAPIErrorMessage({
      content: `${API_ERROR_MESSAGE_PREFIX}: ${formatAPIError(error)}`,
      error: 'unknown',
    })
  }

  if (error instanceof Error) {
    return createAssistantAPIErrorMessage({
      content: `${API_ERROR_MESSAGE_PREFIX}: ${error.message}`,
      error: 'unknown',
    })
  }
  return createAssistantAPIErrorMessage({
    content: API_ERROR_MESSAGE_PREFIX,
    error: 'unknown',
  })
}

/**
 * For 3P users (non-acosmi/non-firstParty), suggest the SDK default model as
 * a fallback when the selected model is unavailable. The SDK default
 * (resolved via the SDK ManagedModel cache) replaces the legacy hardcoded
 * version-specific family aliases.
 */
function get3PModelFallbackSuggestion(model: string): string | undefined {
  if (isOfficialProvider()) return undefined
  const defaultId = getCachedDefaultModelId()
  if (!defaultId || defaultId === model) return undefined
  return defaultId
}

/**
 * Classifies an API error into a specific error type for analytics tracking.
 * Returns a standardized error type string suitable for Datadog tagging.
 */
export function classifyAPIError(error: unknown): string {
  // Aborted requests
  if (error instanceof Error && error.message === 'Request was aborted.') {
    return 'aborted'
  }

  // Timeout errors
  if (
    error instanceof APIConnectionTimeoutError ||
    (error instanceof APIConnectionError &&
      error.message.toLowerCase().includes('timeout'))
  ) {
    return 'api_timeout'
  }

  // Check for repeated 529 errors
  if (
    error instanceof Error &&
    error.message.includes(REPEATED_529_ERROR_MESSAGE)
  ) {
    return 'repeated_529'
  }

  // Check for emergency capacity off switch
  if (
    error instanceof Error &&
    error.message.includes(CUSTOM_OFF_SWITCH_MESSAGE)
  ) {
    return 'capacity_off_switch'
  }

  // Rate limiting
  // V116.1 P1-3:本函数(analytics 分类)的 `instanceof APIError` 分支全部改读
  // 归一结果 —— 原写法自 Phase 3 起真实 HTTPError 一律落 'unknown',遥测盲区。
  const n = normalizeGatewayError(error)
  if (n.status === 429) {
    return 'rate_limit'
  }

  // Server overload (529)
  if (
    n.status === 529 ||
    (error instanceof Error &&
      error.message?.includes('"type":"overloaded_error"'))
  ) {
    return 'server_overload'
  }

  // Prompt/content size errors
  if (
    error instanceof Error &&
    error.message
      .toLowerCase()
      .includes(PROMPT_TOO_LONG_ERROR_MESSAGE.toLowerCase())
  ) {
    return 'prompt_too_long'
  }

  // PDF errors
  if (
    error instanceof Error &&
    /maximum of \d+ PDF pages/.test(error.message)
  ) {
    return 'pdf_too_large'
  }

  if (
    error instanceof Error &&
    error.message.includes('The PDF specified is password protected')
  ) {
    return 'pdf_password_protected'
  }

  // Image size errors
  if (
    error instanceof Error &&
    n.status === 400 &&
    error.message.includes('image exceeds') &&
    error.message.includes('maximum')
  ) {
    return 'image_too_large'
  }

  // Many-image dimension errors
  if (
    error instanceof Error &&
    n.status === 400 &&
    error.message.includes('image dimensions exceed') &&
    error.message.includes('many-image')
  ) {
    return 'image_too_large'
  }

  // Tool use errors (400)
  if (
    error instanceof Error &&
    n.status === 400 &&
    error.message.includes(
      '`tool_use` ids were found without `tool_result` blocks immediately after',
    )
  ) {
    return 'tool_use_mismatch'
  }

  if (
    error instanceof Error &&
    n.status === 400 &&
    error.message.includes('unexpected `tool_use_id` found in `tool_result`')
  ) {
    return 'unexpected_tool_result'
  }

  if (
    error instanceof Error &&
    n.status === 400 &&
    error.message.includes('`tool_use` ids must be unique')
  ) {
    return 'duplicate_tool_use_id'
  }

  // Invalid model errors (400)
  if (
    error instanceof Error &&
    n.status === 400 &&
    error.message.toLowerCase().includes('invalid model name')
  ) {
    return 'invalid_model'
  }

  // Credit/billing errors
  if (
    error instanceof Error &&
    error.message
      .toLowerCase()
      .includes(CREDIT_BALANCE_TOO_LOW_ERROR_MESSAGE.toLowerCase())
  ) {
    return 'credit_balance_low'
  }

  // Authentication errors
  if (
    error instanceof Error &&
    error.message.toLowerCase().includes('x-api-key')
  ) {
    return 'invalid_api_key'
  }

  if (
    error instanceof Error &&
    n.status === 403 &&
    error.message.includes('OAuth token has been revoked')
  ) {
    return 'token_revoked'
  }

  if (
    error instanceof Error &&
    (n.status === 401 || n.status === 403) &&
    error.message.includes(
      'OAuth authentication is currently not allowed for this organization',
    )
  ) {
    return 'oauth_org_not_allowed'
  }

  // Generic auth errors
  if (n.status === 401 || n.status === 403) {
    return 'auth_error'
  }

  // Status code based fallbacks
  if (n.status !== null && n.status >= 500) return 'server_error'
  if (n.status !== null && n.status >= 400) return 'client_error'

  // Connection errors - check for SSL/TLS issues first
  if (error instanceof APIConnectionError) {
    const connectionDetails = extractConnectionErrorDetails(error)
    if (connectionDetails?.isSSLError) {
      return 'ssl_cert_error'
    }
    return 'connection_error'
  }

  return 'unknown'
}

// V116.1 P1-3:参数从本地 APIError 类改为 api-types 的结构鸭子形态(该类族已
// 删除)。唯一调用方 QueryEngine.ts:1200(headless api_retry 遥测帧分类)传入
// SystemAPIErrorMessage.error。注:首轮清查曾误判"零调用方"—— git pathspec
// `src/**/*.ts` 不匹配 src/ 直下文件的假阴性,tsc 兜底抓回;负向 grep 结论
// 必须二次验证(与 NewNotificationCron 假阴性同教训)。
export function categorizeRetryableAPIError(
  error: APIError,
): SDKAssistantMessageError {
  if (
    error.status === 529 ||
    error.message?.includes('"type":"overloaded_error"')
  ) {
    return 'rate_limit'
  }
  if (error.status === 429) {
    return 'rate_limit'
  }
  if (error.status === 401 || error.status === 403) {
    return 'authentication_failed'
  }
  if (error.status !== undefined && error.status >= 408) {
    return 'server_error'
  }
  return 'unknown'
}

export function getErrorMessageIfRefusal(
  stopReason: BetaStopReason | null,
  model: string,
): AssistantMessage | undefined {
  if (stopReason !== 'refusal') {
    return
  }

  logEvent('tengu_refusal_api_response', {})

  const baseMessage = getIsNonInteractiveSession()
    ? `${API_ERROR_MESSAGE_PREFIX}: CrabCode is unable to respond to this request, which appears to violate our Usage Policy (https://acosmi.com/zh/legal/aup). Try rephrasing the request or attempting a different approach.`
    : `${API_ERROR_MESSAGE_PREFIX}: CrabCode is unable to respond to this request, which appears to violate our Usage Policy (https://acosmi.com/zh/legal/aup). Please double press esc to edit your last message or start a new session for CrabCode to assist with a different task.`

  const defaultId = getCachedDefaultModelId()
  const modelSuggestion =
    defaultId && model !== defaultId
      ? ' If you are seeing this refusal repeatedly, try running /model to switch to a different model.'
      : ''

  return createAssistantAPIErrorMessage({
    content: baseMessage + modelSuggestion,
    error: 'invalid_request',
  })
}
