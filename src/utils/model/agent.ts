import type { PermissionMode } from '../permissions/PermissionMode.js'
import { capitalize } from '../stringUtils.js'
import { MODEL_ALIASES, type ModelAlias } from './aliases.js'
import {
  getCanonicalName,
  getDefaultModel,
  getMaxEffortModel,
  getRuntimeMainLoopModel,
  parseUserSpecifiedModel,
} from './model.js'
import { getCachedDefaultModelId, getCachedEnabledModels, getCachedModelDisplayName } from './modelCapabilities.js'
import { findModelByCapability } from './findModelByCapability.js'
import {
  assertRuntimeModel,
  normalizeRuntimeModelInput,
  resolveAnyEnabledOrDefault,
  resolveCapabilityModel,
  type AgentModelPreference,
} from './runtimeModelResolution.js'

export const AGENT_MODEL_OPTIONS = [...MODEL_ALIASES, 'inherit'] as const
export type AgentModelAlias = (typeof AGENT_MODEL_OPTIONS)[number]

export type AgentModelOption = {
  value: string
  label: string
  description: string
}

/**
 * Get the default subagent model. Returns 'inherit' so subagents inherit
 * the model from the parent thread.
 */
export function getDefaultSubagentModel(): string {
  return 'inherit'
}

/**
 * Get the effective model string for an agent.
 */
export function getAgentModel(
  agentModel: string | undefined,
  parentModel: string,
  toolSpecifiedModel?: string,
  permissionMode?: PermissionMode,
  modelPreference?: AgentModelPreference,
  agentType?: string,
): string {
  const envModel = normalizeRuntimeModelInput(process.env.CRABCODE_SUBAGENT_MODEL)
  if (envModel) {
    return assertRuntimeModel(parseUserSpecifiedModel(envModel), {
      source: 'getAgentModel.env',
      agentType,
    })
  }

  // Prioritize tool-specified model if provided
  const normalizedToolModel = normalizeRuntimeModelInput(toolSpecifiedModel)
  if (normalizedToolModel) {
    if (modelMatchesParentTier(normalizedToolModel, parentModel)) {
      return parentModel
    }
    return assertRuntimeModel(parseUserSpecifiedModel(normalizedToolModel), {
      source: 'getAgentModel.toolSpecifiedModel',
      agentType,
    })
  }

  const normalizedAgentModel = normalizeRuntimeModelInput(agentModel)
  if (!normalizedAgentModel && modelPreference) {
    const preferredModel = resolveAgentModelPreference(
      modelPreference,
      parentModel,
      permissionMode,
      agentType,
    )
    if (preferredModel) return preferredModel
  }

  const agentModelWithExp = normalizedAgentModel ?? getDefaultSubagentModel()

  if (agentModelWithExp === 'inherit') {
    // Apply runtime model resolution for inherit to get the effective model.
    // This ensures agents using 'inherit' get planmode → max-effort resolution
    // in plan mode.
    return assertRuntimeModel(getRuntimeMainLoopModel({
      permissionMode: permissionMode ?? 'default',
      mainLoopModel: parentModel,
      exceeds200kTokens: false,
    }), {
      source: 'getAgentModel.inherit',
      agentType,
    })
  }

  if (modelMatchesParentTier(agentModelWithExp, parentModel)) {
    return parentModel
  }
  return assertRuntimeModel(parseUserSpecifiedModel(agentModelWithExp), {
    source: 'getAgentModel.agentModel',
    agentType,
  })
}

function resolveAgentModelPreference(
  preference: AgentModelPreference,
  parentModel: string,
  permissionMode?: PermissionMode,
  agentType?: string,
): string | undefined {
  if (preference.fallback === 'inherit') {
    const inheritedModel = () => assertRuntimeModel(getRuntimeMainLoopModel({
      permissionMode: permissionMode ?? 'default',
      mainLoopModel: parentModel,
      exceeds200kTokens: false,
    }), {
      source: 'getAgentModel.preference.inherit',
      agentType,
    })

    if (preference.capability) {
      const resolved = resolveCapabilityModel(preference.capability, {
        source: `getAgentModel.${agentType ?? 'agent'}`,
        fallback: 'unavailable',
      })
      return resolved.ok ? resolved.model : inheritedModel()
    }
    if (preference.tier === 'default') {
      const resolved = resolveAnyEnabledOrDefault({
        source: `getAgentModel.${agentType ?? 'agent'}`,
        literalFallback: parentModel,
      })
      return resolved.ok ? resolved.model : inheritedModel()
    }
    return inheritedModel()
  }

  if (preference.capability) {
    const resolved = resolveCapabilityModel(preference.capability, {
      source: `getAgentModel.${agentType ?? 'agent'}`,
      fallback: preference.fallback,
      literalFallback: preference.literalFallback,
    })
    if (resolved.ok) return resolved.model
  } else if (preference.tier === 'default') {
    const resolved = resolveAnyEnabledOrDefault({
      source: `getAgentModel.${agentType ?? 'agent'}`,
      literalFallback: preference.literalFallback,
    })
    if (resolved.ok) return resolved.model
  }

  if (preference.fallback === 'literal') {
    const literal = normalizeRuntimeModelInput(preference.literalFallback)
    if (literal) return literal
  }

  return undefined
}

/**
 * Check whether a tool-specified model resolves to the same SDK model as the
 * parent. When it does, the subagent inherits the parent's exact model string
 * instead of resolving the alias to a provider default.
 *
 * Prevents surprising downgrades: a Vertex user on a max-effort SDK ID (via
 * /model) who spawns a subagent specifying the same SDK ID should get that
 * exact model, not whatever findModelByCapability returns for 3P.
 */
function modelMatchesParentTier(modelInput: string, parentModel: string): boolean {
  const resolvedInput = parseUserSpecifiedModel(modelInput).toLowerCase()
  const resolvedParent = parentModel.toLowerCase()
  if (resolvedInput === resolvedParent) return true
  return getCanonicalName(resolvedInput) === getCanonicalName(resolvedParent)
}

export function getAgentModelDisplay(model: string | undefined): string {
  // When model is omitted, getDefaultSubagentModel() returns 'inherit' at runtime
  if (!model) return 'Inherit from parent (default)'
  if (model === 'inherit') return 'Inherit from parent'
  return capitalize(model)
}

/**
 * Get available model options for agents — built dynamically from the SDK
 * capability cache. Returns SDK-resolved IDs for default / max-effort /
 * fast-mode tiers plus the "inherit" sentinel.
 */
export function getAgentModelOptions(): AgentModelOption[] {
  const defaultId = getCachedDefaultModelId() ?? getDefaultModel()
  const maxEffortId = getMaxEffortModel()
  const fastModeId = findModelByCapability('supports_fast_mode')?.id
  const defaultName = getCachedModelDisplayName(defaultId) || 'Default'
  const maxEffortName = getCachedModelDisplayName(maxEffortId) || 'Max effort'
  const fastModeName = fastModeId
    ? getCachedModelDisplayName(fastModeId)
    : 'Fast mode'
  const seen = new Set<string>()
  const options: AgentModelOption[] = []
  const push = (value: string | undefined, label: string, description: string) => {
    if (!value || seen.has(value)) return
    seen.add(value)
    options.push({ value, label, description })
  }
  push(defaultId, defaultName, 'Balanced performance — best for most agents')
  push(maxEffortId, maxEffortName, 'Most capable for complex reasoning tasks')
  push(fastModeId, fastModeName, 'Fast and efficient for simple tasks')
  if (options.length === 0) {
    // Fallback when SDK cache is empty: surface enabled models directly.
    for (const m of getCachedEnabledModels().slice(0, 3)) {
      push(m.id, m.name ?? m.id, m.id)
    }
  }
  options.push({
    value: 'inherit',
    label: 'Inherit from parent',
    description: 'Use the same model as the main conversation',
  })
  return options
}
