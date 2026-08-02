import { randomUUID, type UUID } from 'crypto'
import { validateUuid } from './uuid.js'

export type ParsedSessionIdentifier = {
  sessionId: UUID
  jsonlFile: string | null
  isJsonlFile: boolean
}

/**
 * Parses a local session UUID or JSONL transcript path.
 *
 * @param resumeIdentifier - The URL or session ID to parse
 * @returns Parsed local session information or null if invalid
 */
export function parseSessionIdentifier(
  resumeIdentifier: string,
): ParsedSessionIdentifier | null {
  if (resumeIdentifier.toLowerCase().endsWith('.jsonl')) {
    return {
      sessionId: randomUUID() as UUID,
      jsonlFile: resumeIdentifier,
      isJsonlFile: true,
    }
  }

  // Check if it's a plain UUID
  if (validateUuid(resumeIdentifier)) {
    return {
      sessionId: resumeIdentifier as UUID,
      jsonlFile: null,
      isJsonlFile: false,
    }
  }

  return null
}
