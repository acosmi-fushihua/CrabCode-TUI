/**
 * Process-owned session-title generation for the native TUI runtime.
 *
 * This is the transport-neutral backend operation. It deliberately calls the
 * existing query helpers in this
 * process; no daemon, socket, desktop client, or renderer participates.
 */

import { queryFastMode, queryWithModel } from '../services/api/queryModel.js'
import { logEvent } from '../services/analytics/index.js'
import { getIsNonInteractiveSession } from '../bootstrap/state.js'
import { sanitizeLanguagePreference } from './languagePreference.js'
import { extractTextContent } from './messages.js'
import { safeParseJSON } from './json.js'
import { getSettingsWithSources } from './settings/settings.js'
import { asSystemPrompt } from './systemPromptType.js'
import { logForDebugging } from './debug.js'

const SESSION_TITLE_PROMPT = `Generate a concise, sentence-case title (3-7 words) that captures the main topic or goal of this coding session. The title should be clear enough that the user recognizes the session in a list. Use sentence case: capitalize only the first word and proper nouns.

Return JSON with a single "title" field.

Good examples:
{"title": "Fix login button on mobile"}
{"title": "Add OAuth authentication"}
{"title": "Debug failing CI tests"}
{"title": "Refactor API client error handling"}

Bad (too vague): {"title": "Code changes"}
Bad (too long): {"title": "Investigate and fix the issue where the login button does not respond on mobile devices"}
Bad (wrong case): {"title": "Fix Login Button On Mobile"}`

const TITLE_OUTPUT_FORMAT = {
  type: 'json_schema' as const,
  schema: {
    type: 'object',
    properties: { title: { type: 'string' } },
    required: ['title'],
    additionalProperties: false,
  },
}

function titleLanguageDirective(): string | null {
  try {
    const language = sanitizeLanguagePreference(
      getSettingsWithSources().effective.language,
    )
    if (!language) return null
    return `Output-language requirement: write the JSON "title" value in ${language}. The English examples above illustrate format and brevity only, not the output language. Preserve code identifiers, commands, product names, and established technical terms in their original form. For languages that do not separate words with spaces, use a short natural phrase instead of applying the 3-7 word count literally.`
  } catch {
    return null
  }
}

export async function generateSessionTitleDirect(
  description: string,
  signal: AbortSignal,
  options?: { model?: string },
): Promise<string | null> {
  const trimmed = description.trim()
  if (!trimmed) return null

  try {
    const languageDirective = titleLanguageDirective()
    const baseArgs = {
      systemPrompt: asSystemPrompt(
        languageDirective
          ? [SESSION_TITLE_PROMPT, languageDirective]
          : [SESSION_TITLE_PROMPT],
      ),
      userPrompt: trimmed,
      outputFormat: TITLE_OUTPUT_FORMAT,
      signal,
    }
    const sharedOptions = {
      agents: [],
      isNonInteractiveSession: getIsNonInteractiveSession(),
      hasAppendSystemPrompt: false,
      mcpTools: [],
      querySource: 'generate_session_title' as const,
    }
    const result = options?.model
      ? await queryWithModel({
          ...baseArgs,
          options: { ...sharedOptions, model: options.model },
        })
      : await queryFastMode({
          ...baseArgs,
          options: sharedOptions,
        })
    const parsed = safeParseJSON(
      extractTextContent(result.message.content),
      false,
    )
    const title =
      parsed &&
      typeof parsed === 'object' &&
      'title' in parsed &&
      typeof parsed.title === 'string'
        ? parsed.title.trim() || null
        : null
    logEvent('tengu_session_title_generated', { success: title !== null })
    return title
  } catch (error) {
    logForDebugging(`generateSessionTitleDirect failed: ${error}`, {
      level: 'error',
    })
    logEvent('tengu_session_title_generated', { success: false })
    return null
  }
}
