import {
  type CapabilityKey,
  findModelByCapability,
  pickAnyEnabledModel,
} from './findModelByCapability.js'
import { getCachedDefaultModelId } from './modelCapabilities.js'

export type RuntimeModelContext = {
  source: string
  querySource?: string
  agentType?: string
  purpose?: string
}

export type RuntimeModelResolution =
  | { ok: true; model: string; source: string }
  | { ok: false; reason: string; source: string }

export type RuntimeModelFallbackPolicy =
  | 'unavailable'
  | 'default'
  | 'any-enabled'
  | 'literal'

export type AgentModelPreference = {
  capability?: CapabilityKey
  tier?: 'default'
  fallback: RuntimeModelFallbackPolicy | 'inherit'
  literalFallback?: string
}

export class EmptyRuntimeModelError extends Error {
  readonly code = 'EMPTY_RUNTIME_MODEL'
  readonly context: RuntimeModelContext
  readonly rawModel: unknown

  constructor(rawModel: unknown, context: RuntimeModelContext) {
    const details = [
      `source=${context.source}`,
      context.querySource ? `querySource=${context.querySource}` : undefined,
      context.agentType ? `agentType=${context.agentType}` : undefined,
      context.purpose ? `purpose=${context.purpose}` : undefined,
    ].filter(Boolean).join(' ')
    super(`Runtime model must be non-empty (${details})`)
    this.name = 'EmptyRuntimeModelError'
    this.context = context
    this.rawModel = rawModel
    Object.setPrototypeOf(this, EmptyRuntimeModelError.prototype)
  }
}

export function normalizeRuntimeModelInput(value: unknown): string | undefined {
  if (typeof value !== 'string') return undefined
  const trimmed = value.trim()
  if (!trimmed) return undefined
  const apiModel = trimmed.replace(/\[(1|2)m\]/gi, '').trim()
  if (!apiModel) return undefined
  return trimmed
}

export function assertRuntimeModel(
  value: unknown,
  context: RuntimeModelContext,
): string {
  const normalized = normalizeRuntimeModelInput(value)
  if (!normalized) {
    throw new EmptyRuntimeModelError(value, context)
  }
  return normalized
}

export function resolveAnyEnabledOrDefault(params: {
  source: string
  literalFallback?: string
}): RuntimeModelResolution {
  const defaultModel = normalizeRuntimeModelInput(getCachedDefaultModelId())
  if (defaultModel) {
    return { ok: true, model: defaultModel, source: `${params.source}:default` }
  }

  const enabledModel = normalizeRuntimeModelInput(pickAnyEnabledModel()?.id)
  if (enabledModel) {
    return { ok: true, model: enabledModel, source: `${params.source}:enabled` }
  }

  const literal = normalizeRuntimeModelInput(params.literalFallback)
  if (literal) {
    return { ok: true, model: literal, source: `${params.source}:literal` }
  }

  return {
    ok: false,
    reason: 'no default, enabled, or literal fallback model available',
    source: params.source,
  }
}

export function resolveCapabilityModel(
  capability: CapabilityKey,
  params: {
    source: string
    fallback: RuntimeModelFallbackPolicy
    literalFallback?: string
  },
): RuntimeModelResolution {
  const capabilityModel = normalizeRuntimeModelInput(
    findModelByCapability(capability)?.id,
  )
  if (capabilityModel) {
    return {
      ok: true,
      model: capabilityModel,
      source: `${params.source}:${capability}`,
    }
  }

  switch (params.fallback) {
    case 'default': {
      const defaultModel = normalizeRuntimeModelInput(getCachedDefaultModelId())
      if (defaultModel) {
        return {
          ok: true,
          model: defaultModel,
          source: `${params.source}:default-fallback`,
        }
      }
      return resolveAnyEnabledOrDefault(params)
    }
    case 'any-enabled':
      return resolveAnyEnabledOrDefault(params)
    case 'literal': {
      const literal = normalizeRuntimeModelInput(params.literalFallback)
      if (literal) {
        return {
          ok: true,
          model: literal,
          source: `${params.source}:literal-fallback`,
        }
      }
      return {
        ok: false,
        reason: `no ${capability} model and literal fallback is unavailable`,
        source: params.source,
      }
    }
    case 'unavailable':
      return {
        ok: false,
        reason: `no model with capability ${capability}`,
        source: params.source,
      }
  }
}
