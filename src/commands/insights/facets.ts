import { mkdir, readFile, unlink, writeFile } from 'fs/promises'
import { join } from 'path'
import { queryWithModel } from '../../services/api/crabcode.js'
import type { LogOption } from '../../types/logs.js'
import { toError } from '../../utils/errors.js'
import { logError } from '../../utils/log.js'
import { extractTextContent } from '../../utils/messages.js'
import { jsonParse, jsonStringify } from '../../utils/slowOperations.js'
import { asSystemPrompt } from '../../utils/systemPromptType.js'
import { formatTranscriptWithSummarization } from './sessionProcessing.js'
import type { SessionFacets, SessionMeta } from './types.js'
import {
  FACET_EXTRACTION_PROMPT,
  getAnalysisModel,
  getFacetsDir,
  getSessionMetaDir,
  isValidSessionFacets,
} from './types.js'

// ============================================================================
// Cache Operations
// ============================================================================

export async function loadCachedFacets(
  sessionId: string,
): Promise<SessionFacets | null> {
  const facetPath = join(getFacetsDir(), `${sessionId}.json`)
  try {
    const content = await readFile(facetPath, { encoding: 'utf-8' })
    const parsed: unknown = jsonParse(content)
    if (!isValidSessionFacets(parsed)) {
      // Delete corrupted cache file so it gets re-extracted next run
      try {
        await unlink(facetPath)
      } catch {
        // Ignore deletion errors
      }
      return null
    }
    return parsed
  } catch {
    return null
  }
}

export async function saveFacets(facets: SessionFacets): Promise<void> {
  try {
    await mkdir(getFacetsDir(), { recursive: true })
  } catch {
    // Directory may already exist
  }
  const facetPath = join(getFacetsDir(), `${facets.session_id}.json`)
  await writeFile(facetPath, jsonStringify(facets, null, 2), {
    encoding: 'utf-8',
    mode: 0o600,
  })
}

export async function loadCachedSessionMeta(
  sessionId: string,
): Promise<SessionMeta | null> {
  const metaPath = join(getSessionMetaDir(), `${sessionId}.json`)
  try {
    const content = await readFile(metaPath, { encoding: 'utf-8' })
    return jsonParse(content)
  } catch {
    return null
  }
}

export async function saveSessionMeta(meta: SessionMeta): Promise<void> {
  try {
    await mkdir(getSessionMetaDir(), { recursive: true })
  } catch {
    // Directory may already exist
  }
  const metaPath = join(getSessionMetaDir(), `${meta.session_id}.json`)
  await writeFile(metaPath, jsonStringify(meta, null, 2), {
    encoding: 'utf-8',
    mode: 0o600,
  })
}

// ============================================================================
// API Extraction
// ============================================================================

export async function extractFacetsFromAPI(
  log: LogOption,
  sessionId: string,
): Promise<SessionFacets | null> {
  try {
    // Use summarization for long transcripts
    const transcript = await formatTranscriptWithSummarization(log)

    // Build prompt asking for JSON directly (no tool use)
    const jsonPrompt = `${FACET_EXTRACTION_PROMPT}${transcript}

RESPOND WITH ONLY A VALID JSON OBJECT matching this schema:
{
  "underlying_goal": "What the user fundamentally wanted to achieve",
  "goal_categories": {"category_name": count, ...},
  "outcome": "fully_achieved|mostly_achieved|partially_achieved|not_achieved|unclear_from_transcript",
  "user_satisfaction_counts": {"level": count, ...},
  "crabcode_helpfulness": "unhelpful|slightly_helpful|moderately_helpful|very_helpful|essential",
  "session_type": "single_task|multi_task|iterative_refinement|exploration|quick_question",
  "friction_counts": {"friction_type": count, ...},
  "friction_detail": "One sentence describing friction or empty",
  "primary_success": "none|fast_accurate_search|correct_code_edits|good_explanations|proactive_help|multi_file_changes|good_debugging",
  "brief_summary": "One sentence: what user wanted and whether they got it"
}`

    const result = await queryWithModel({
      systemPrompt: asSystemPrompt([]),
      userPrompt: jsonPrompt,
      signal: new AbortController().signal,
      options: {
        model: getAnalysisModel(),
        querySource: 'insights',
        agents: [],
        isNonInteractiveSession: true,
        hasAppendSystemPrompt: false,
        mcpTools: [],
        maxOutputTokensOverride: 4096,
      },
    })

    const text = extractTextContent(result.message.content)

    // Parse JSON from response
    const jsonMatch = text.match(/\{[\s\S]*\}/)
    if (!jsonMatch) return null

    const parsed: unknown = jsonParse(jsonMatch[0])
    if (!isValidSessionFacets(parsed)) return null
    const facets: SessionFacets = { ...parsed, session_id: sessionId }
    return facets
  } catch (err) {
    logError(new Error(`Facet extraction failed: ${toError(err).message}`))
    return null
  }
}
