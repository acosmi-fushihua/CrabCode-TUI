// ANT-ONLY import markers must not be reordered
import { MODEL_ALIASES } from './aliases.js'
import { isModelAllowed } from './modelAllowlist.js'
import { isOfficialProvider } from './providers.js'
import { sideQuery } from '../sideQuery.js'
import { HTTPError } from '@acosmi/sdk-ts'
import { normalizeGatewayError } from '../../services/api/gatewayErrorNormalizer.js'
import { APIConnectionError } from '../../errors/api-errors.js'
import { getCachedDefaultModelId } from './modelCapabilities.js'

// Cache valid models to avoid repeated API calls
const validModelCache = new Map<string, boolean>()

/**
 * P2-4: clear the positive validation cache. The cache is keyed only on the
 * model string and only stores `true` results, so after an account / provider /
 * baseURL / entitlement switch a model that validated under the OLD principal
 * would be reused as still-valid. Must be invoked on the same logout / login
 * events that refresh the model catalog (see commands/logout + commands/login)
 * so a stale positive does not survive a principal change.
 */
export function clearValidModelCache(): void {
  validModelCache.clear()
}

/**
 * Validates a model by attempting an actual API call.
 */
export async function validateModel(
  model: string,
): Promise<{ valid: boolean; error?: string }> {
  const normalizedModel = model.trim()

  // Empty model is invalid
  if (!normalizedModel) {
    return { valid: false, error: 'Model name cannot be empty' }
  }

  // Check against availableModels allowlist before any API call
  if (!isModelAllowed(normalizedModel)) {
    return {
      valid: false,
      error: `Model '${normalizedModel}' is not in the list of available models`,
    }
  }

  // Check if it's a known alias (these are always valid)
  const lowerModel = normalizedModel.toLowerCase()
  if ((MODEL_ALIASES as readonly string[]).includes(lowerModel)) {
    return { valid: true }
  }

  // Check if it matches ACOSMI_CUSTOM_MODEL_OPTION (pre-validated by the user)
  if (normalizedModel === process.env.ACOSMI_CUSTOM_MODEL_OPTION) {
    return { valid: true }
  }

  // Check cache first
  if (validModelCache.has(normalizedModel)) {
    return { valid: true }
  }


  // Try to make an actual API call with minimal parameters
  try {
    await sideQuery({
      model: normalizedModel,
      max_tokens: 1,
      maxRetries: 0,
      querySource: 'model_validation',
      messages: [
        {
          role: 'user',
          content: [
            {
              type: 'text',
              text: 'Hi',
              cache_control: { type: 'ephemeral' },
            },
          ],
        },
      ],
    })

    // If we got here, the model is valid
    validModelCache.set(normalizedModel, true)
    return { valid: true }
  } catch (error) {
    return handleValidationError(error, normalizedModel)
  }
}

function handleValidationError(
  error: unknown,
  modelName: string,
): { valid: boolean; error: string } {
  // V116.1 P1-3 (2026-07-24):原梯子基于本地 APIError 分类族(零构造点死代码,
  // 自 Phase 3 起所有分支落空、一律走"Unable to validate"兜底),按真实错误
  // 形态(SDK HTTPError / NetworkError)复活各分类文案。
  const n = normalizeGatewayError(error)

  // 404 / not_found_error means the model doesn't exist
  if (
    n.status === 404 ||
    n.errorType === 'not_found_error' ||
    (n.errorType === null && n.message.includes('not_found_error'))
  ) {
    const fallback = get3PFallbackSuggestion(modelName)
    const suggestion = fallback ? `. Try '${fallback}' instead` : ''
    return {
      valid: false,
      error: `Model '${modelName}' not found${suggestion}`,
    }
  }

  if (
    n.status === 401 ||
    (n.status === 403 && n.errorType === 'authentication_error')
  ) {
    return {
      valid: false,
      error: 'Authentication failed. Please check your API credentials.',
    }
  }

  // 传输层失败:SDK NetworkError(name 判别)或本地哨兵 APIConnectionError
  if (
    error instanceof APIConnectionError ||
    (error instanceof Error && error.name === 'NetworkError')
  ) {
    return {
      valid: false,
      error: 'Network error. Please check your internet connection.',
    }
  }

  if (error instanceof HTTPError) {
    return { valid: false, error: `API error: ${n.message}` }
  }

  // For unknown errors, be safe and reject
  const errorMessage = error instanceof Error ? error.message : String(error)
  return {
    valid: false,
    error: `Unable to validate model: ${errorMessage}`,
  }
}

/**
 * Suggest a fallback model for 3P users when the selected model is unavailable.
 * Uses the SDK default (resolved via ManagedModel cache) instead of legacy
 * hardcoded version-specific aliases — keeps suggestion catalog-driven.
 */
function get3PFallbackSuggestion(model: string): string | undefined {
  if (isOfficialProvider()) return undefined
  const defaultId = getCachedDefaultModelId()
  if (!defaultId || defaultId === model) return undefined
  return defaultId
}
