// Content for the crabcode-api bundled skill.
// Each .md file is inlined as a string at build time via Bun's text loader.

import { getCachedDefaultModelId, getCachedModelDisplayName } from '../../utils/model/modelCapabilities.js'
import { findModelByCapability } from '../../utils/model/findModelByCapability.js'
import { getDefaultModel, getMaxEffortModel } from '../../utils/model/model.js'
import csharpCrabCodeApi from './crabcode-api/csharp/crabcode-api.md'
import curlExamples from './crabcode-api/curl/examples.md'
import goCrabCodeApi from './crabcode-api/go/crabcode-api.md'
import javaCrabCodeApi from './crabcode-api/java/crabcode-api.md'
import phpCrabCodeApi from './crabcode-api/php/crabcode-api.md'
import pythonAgentSdkPatterns from './crabcode-api/python/agent-sdk/patterns.md'
import pythonAgentSdkReadme from './crabcode-api/python/agent-sdk/README.md'
import pythonCrabCodeApiBatches from './crabcode-api/python/crabcode-api/batches.md'
import pythonCrabCodeApiFilesApi from './crabcode-api/python/crabcode-api/files-api.md'
import pythonCrabCodeApiReadme from './crabcode-api/python/crabcode-api/README.md'
import pythonCrabCodeApiStreaming from './crabcode-api/python/crabcode-api/streaming.md'
import pythonCrabCodeApiToolUse from './crabcode-api/python/crabcode-api/tool-use.md'
import rubyCrabCodeApi from './crabcode-api/ruby/crabcode-api.md'
import skillPrompt from './crabcode-api/SKILL.md'
import sharedErrorCodes from './crabcode-api/shared/error-codes.md'
import sharedLiveSources from './crabcode-api/shared/live-sources.md'
import sharedModels from './crabcode-api/shared/models.md'
import sharedPromptCaching from './crabcode-api/shared/prompt-caching.md'
import sharedToolUseConcepts from './crabcode-api/shared/tool-use-concepts.md'
import typescriptAgentSdkPatterns from './crabcode-api/typescript/agent-sdk/patterns.md'
import typescriptAgentSdkReadme from './crabcode-api/typescript/agent-sdk/README.md'
import typescriptCrabCodeApiBatches from './crabcode-api/typescript/crabcode-api/batches.md'
import typescriptCrabCodeApiFilesApi from './crabcode-api/typescript/crabcode-api/files-api.md'
import typescriptCrabCodeApiReadme from './crabcode-api/typescript/crabcode-api/README.md'
import typescriptCrabCodeApiStreaming from './crabcode-api/typescript/crabcode-api/streaming.md'
import typescriptCrabCodeApiToolUse from './crabcode-api/typescript/crabcode-api/tool-use.md'

// Substituted into {{VAR}} placeholders in the .md files at runtime before the
// skill prompt is sent.
export function getSkillModelVars(): Record<string, string> {
  const maxEffortId = getMaxEffortModel()
  const defaultId = getCachedDefaultModelId() ?? getDefaultModel()
  const fastId = findModelByCapability('supports_fast_mode')?.id ?? defaultId
  return {
    MAX_EFFORT_ID: maxEffortId,
    MAX_EFFORT_NAME: getCachedModelDisplayName(maxEffortId),
    DEFAULT_ID: defaultId,
    DEFAULT_NAME: getCachedModelDisplayName(defaultId),
    FAST_MODE_ID: fastId,
    FAST_MODE_NAME: getCachedModelDisplayName(fastId),
  }
}

export const SKILL_PROMPT: string = skillPrompt

export const SKILL_FILES: Record<string, string> = {
  'csharp/crabcode-api.md': csharpCrabCodeApi,
  'curl/examples.md': curlExamples,
  'go/crabcode-api.md': goCrabCodeApi,
  'java/crabcode-api.md': javaCrabCodeApi,
  'php/crabcode-api.md': phpCrabCodeApi,
  'python/agent-sdk/README.md': pythonAgentSdkReadme,
  'python/agent-sdk/patterns.md': pythonAgentSdkPatterns,
  'python/crabcode-api/README.md': pythonCrabCodeApiReadme,
  'python/crabcode-api/batches.md': pythonCrabCodeApiBatches,
  'python/crabcode-api/files-api.md': pythonCrabCodeApiFilesApi,
  'python/crabcode-api/streaming.md': pythonCrabCodeApiStreaming,
  'python/crabcode-api/tool-use.md': pythonCrabCodeApiToolUse,
  'ruby/crabcode-api.md': rubyCrabCodeApi,
  'shared/error-codes.md': sharedErrorCodes,
  'shared/live-sources.md': sharedLiveSources,
  'shared/models.md': sharedModels,
  'shared/prompt-caching.md': sharedPromptCaching,
  'shared/tool-use-concepts.md': sharedToolUseConcepts,
  'typescript/agent-sdk/README.md': typescriptAgentSdkReadme,
  'typescript/agent-sdk/patterns.md': typescriptAgentSdkPatterns,
  'typescript/crabcode-api/README.md': typescriptCrabCodeApiReadme,
  'typescript/crabcode-api/batches.md': typescriptCrabCodeApiBatches,
  'typescript/crabcode-api/files-api.md': typescriptCrabCodeApiFilesApi,
  'typescript/crabcode-api/streaming.md': typescriptCrabCodeApiStreaming,
  'typescript/crabcode-api/tool-use.md': typescriptCrabCodeApiToolUse,
}
