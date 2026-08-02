// BetaOutputConfig is not yet in the SDK typings; use a local placeholder.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type BetaOutputConfig = any
import type { BetaMessageStreamParams } from '../../types/api-types.js'
import { feature } from '../../utils/featurePolyfill.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from 'src/services/analytics/growthbook.js'
import {
  shouldIncludeFirstPartyOnlyBetas,
} from 'src/utils/betas.js'
import { logForDebugging } from 'src/utils/debug.js'
import type { EffortValue } from 'src/utils/effort.js'
import { modelSupportsEffort } from 'src/utils/effort.js'
import {
  CAPPED_DEFAULT_MAX_TOKENS,
  getModelMaxOutputTokens,
} from '../../utils/context.js'
import { isEnvTruthy } from '../../utils/envUtils.js'
import { errorMessage } from '../../utils/errors.js'
import { getOauthAccountInfo } from '../../utils/auth.js'
import { getOrCreateUserID } from '../../utils/config.js'
import {
  getMaxEffortModel,
  getDefaultModel,
  getSmallFastModel,
} from '../../utils/model/model.js'
import { jsonStringify } from '../../utils/slowOperations.js'
import { safeParseJSON } from '../../utils/json.js'
import { validateBoundedIntEnvVar } from '../../utils/envValidation.js'
import { getSessionId } from 'src/bootstrap/state.js'
import { EFFORT_BETA_HEADER, TASK_BUDGETS_BETA_HEADER } from 'src/constants/betas.js'
import { getWindowWorkId } from 'src/utils/workloadContext.js'

import type { JsonObject, TaskBudgetParam, Options } from './types.js'

/**
 * Assemble the extra body parameters for the API request, based on the
 * CRABCODE_EXTRA_BODY environment variable if present and on any beta
 * headers (primarily for Bedrock requests).
 *
 * @param betaHeaders - An array of beta headers to include in the request.
 * @returns A JSON object representing the extra body parameters.
 */
export function getExtraBodyParams(betaHeaders?: string[]): JsonObject {
  // Parse user's extra body parameters first
  const extraBodyStr = process.env.CRABCODE_EXTRA_BODY
  let result: JsonObject = {}

  if (extraBodyStr) {
    try {
      // Parse as JSON, which can be null, boolean, number, string, array or object
      const parsed = safeParseJSON(extraBodyStr)
      // We expect an object with key-value pairs to spread into API parameters
      if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
        // Shallow clone — safeParseJSON is LRU-cached and returns the same
        // object reference for the same string. Mutating `result` below
        // would poison the cache, causing stale values to persist.
        result = { ...(parsed as JsonObject) }
      } else {
        logForDebugging(
          `CRABCODE_EXTRA_BODY env var must be a JSON object, but was given ${extraBodyStr}`,
          { level: 'error' },
        )
      }
    } catch (error) {
      logForDebugging(
        `Error parsing CRABCODE_EXTRA_BODY: ${errorMessage(error)}`,
        { level: 'error' },
      )
    }
  }

  // Anti-distillation: send fake_tools opt-in for 1P CLI only
  if (
    feature('ANTI_DISTILLATION_CC')
      ? process.env.CRABCODE_ENTRYPOINT === 'cli' &&
        shouldIncludeFirstPartyOnlyBetas() &&
        getFeatureValue_CACHED_MAY_BE_STALE(
          'tengu_anti_distill_fake_tool_injection',
          false,
        )
      : false
  ) {
    result.anti_distillation = ['fake_tools']
  }

  // Handle beta headers if provided
  if (betaHeaders && betaHeaders.length > 0) {
    if (result.acosmi_beta && Array.isArray(result.acosmi_beta)) {
      // Add to existing array, avoiding duplicates
      const existingHeaders = result.acosmi_beta as string[]
      const newHeaders = betaHeaders.filter(
        header => !existingHeaders.includes(header),
      )
      result.acosmi_beta = [...existingHeaders, ...newHeaders]
    } else {
      // Create new array with the beta headers
      result.acosmi_beta = betaHeaders
    }
  }

  return result
}

export function getAPIMetadata() {
  // https://docs.google.com/document/d/1dURO9ycXXQCBS0V4Vhl4poDBRgkelFc5t2BNPoEgH5Q/edit?tab=t.0#heading=h.5g7nec5b09w5
  let extra: JsonObject = {}
  const extraStr = process.env.CRABCODE_EXTRA_METADATA
  if (extraStr) {
    const parsed = safeParseJSON(extraStr, false)
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      extra = parsed as JsonObject
    } else {
      logForDebugging(
        `CRABCODE_EXTRA_METADATA env var must be a JSON object, but was given ${extraStr}`,
        { level: 'error' },
      )
    }
  }

  return {
    user_id: jsonStringify({
      ...extra,
      device_id: getOrCreateUserID(),
      // Only include OAuth account UUID when actively using OAuth authentication
      account_uuid: getOauthAccountInfo()?.accountUuid ?? '',
      session_id: getSessionId(),
      // Server-authoritative weekly-window admission: stable for every tool/model round in this turn.
      // Undefined outside a turn is intentionally omitted; the gateway then treats the call as fresh work.
      ...(getWindowWorkId() ? { work_id: getWindowWorkId() } : {}),
    }),
  }
}

/**
 * Configure effort parameters for API request.
 *
 */
export function configureEffortParams(
  effortValue: EffortValue | undefined,
  outputConfig: BetaOutputConfig,
  extraBodyParams: Record<string, unknown>,
  betas: string[],
  model: string,
): void {
  if (!modelSupportsEffort(model) || 'effort' in outputConfig) {
    return
  }

  if (effortValue === undefined) {
    betas.push(EFFORT_BETA_HEADER)
  } else if (typeof effortValue === 'string') {
    // Send string effort level as is
    outputConfig.effort = effortValue
    betas.push(EFFORT_BETA_HEADER)
  } else if (process.env.USER_TYPE === 'ant') {
    // Numeric effort override - ant-only (uses acosmi_internal)
    const existingInternal =
      (extraBodyParams.acosmi_internal as Record<string, unknown>) || {}
    extraBodyParams.acosmi_internal = {
      ...existingInternal,
      effort_override: effortValue,
    }
  }
}

export function configureTaskBudgetParams(
  taskBudget: Options['taskBudget'],
  outputConfig: BetaOutputConfig & { task_budget?: TaskBudgetParam },
  betas: string[],
): void {
  if (
    !taskBudget ||
    'task_budget' in outputConfig ||
    !shouldIncludeFirstPartyOnlyBetas()
  ) {
    return
  }
  outputConfig.task_budget = {
    type: 'tokens',
    total: taskBudget.total,
    ...(taskBudget.remaining !== undefined && {
      remaining: taskBudget.remaining,
    }),
  }
  if (!betas.includes(TASK_BUDGETS_BETA_HEADER)) {
    betas.push(TASK_BUDGETS_BETA_HEADER)
  }
}

function isMaxTokensCapEnabled(): boolean {
  // 3P default: false (not validated on Bedrock/Vertex)
  return getFeatureValue_CACHED_MAY_BE_STALE('tengu_otk_slot_v1', false)
}

export function getMaxOutputTokensForModel(model: string): number {
  const maxOutputTokens = getModelMaxOutputTokens(model)

  // Slot-reservation cap: drop default to 8k for all models. BQ p99 output
  // = 4,911 tokens; 32k/64k defaults over-reserve 8-16× slot capacity.
  // Requests hitting the cap get one clean retry at 64k (query.ts
  // max_output_tokens_escalate). Math.min keeps models with lower native
  // defaults at their native value. Applied before the env-var override so
  // CRABCODE_MAX_OUTPUT_TOKENS still wins.
  const defaultTokens = isMaxTokensCapEnabled()
    ? Math.min(maxOutputTokens.default, CAPPED_DEFAULT_MAX_TOKENS)
    : maxOutputTokens.default

  const result = validateBoundedIntEnvVar(
    'CRABCODE_MAX_OUTPUT_TOKENS',
    process.env.CRABCODE_MAX_OUTPUT_TOKENS,
    defaultTokens,
    maxOutputTokens.upperLimit,
  )
  return result.effective
}

// Non-streaming requests have a 10min max.
// The SDK's 21333-token cap is derived from 10min × 128k tokens/hour, but we
// bypass it by setting a client-level timeout, so we can cap higher.
export const MAX_NON_STREAMING_TOKENS = 64_000

/**
 * Adjusts thinking budget when max_tokens is capped for non-streaming fallback.
 * Ensures the API constraint: max_tokens > thinking.budget_tokens
 *
 * @param params - The parameters that will be sent to the API
 * @param maxTokensCap - The maximum allowed tokens (MAX_NON_STREAMING_TOKENS)
 * @returns Adjusted parameters with thinking budget capped if needed
 */
export function adjustParamsForNonStreaming<
  T extends {
    max_tokens: number
    thinking?: BetaMessageStreamParams['thinking']
  },
>(params: T, maxTokensCap: number): T {
  const cappedMaxTokens = Math.min(params.max_tokens, maxTokensCap)

  // Adjust thinking budget if it would exceed capped max_tokens
  // to maintain the constraint: max_tokens > thinking.budget_tokens
  const adjustedParams = { ...params }
  if (
    adjustedParams.thinking?.type === 'enabled' &&
    adjustedParams.thinking.budget_tokens
  ) {
    adjustedParams.thinking = {
      ...adjustedParams.thinking,
      budget_tokens: Math.min(
        adjustedParams.thinking.budget_tokens,
        cappedMaxTokens - 1, // Must be at least 1 less than max_tokens
      ),
    }
  }

  return {
    ...adjustedParams,
    max_tokens: cappedMaxTokens,
  }
}

export function getPromptCachingEnabled(model: string): boolean {
  // Global disable takes precedence
  if (isEnvTruthy(process.env.DISABLE_PROMPT_CACHING)) return false

  // Check if we should disable for small/fast model
  if (isEnvTruthy(process.env.DISABLE_PROMPT_CACHING_FAST_MODE)) {
    const smallFastModel = getSmallFastModel()
    if (model === smallFastModel) return false
  }

  // Check if we should disable for default-tier model
  if (isEnvTruthy(process.env.DISABLE_PROMPT_CACHING_DEFAULT)) {
    const defaultId = getDefaultModel()
    if (model === defaultId) return false
  }

  // Check if we should disable for max-effort model
  if (isEnvTruthy(process.env.DISABLE_PROMPT_CACHING_MAX_EFFORT)) {
    const maxEffortId = getMaxEffortModel()
    if (model === maxEffortId) return false
  }

  return true
}
