/**
 * Feature flag for the media-generation tool.
 *
 * Env-first so it can be set in CI or a one-off run without touching
 * settings.json.
 */

const TRUTHY = /^(1|true|yes|on)$/i

function readEnv(key: string): string | undefined {
  const raw = (globalThis as { process?: { env?: Record<string, string | undefined> } })
    .process?.env?.[key]
  return typeof raw === 'string' ? raw : undefined
}

/**
 * True when the media-generation tool should be registered at all.
 *
 * Direct-TUI resolution:
 *  - env `CRABCODE_MEDIA_GENERATION` explicitly truthy (1/true/yes/on)
 *    → enabled.
 *  - env explicitly set to anything else (0/false/off/…)
 *    → disabled.
 *  - env unset or blank → disabled. The native TUI keeps this paid capability
 *    opt-in-only and does not infer an interactive-surface default.
 */
export function isMediaGenerationEnabled(): boolean {
  const raw = readEnv('CRABCODE_MEDIA_GENERATION')?.trim()
  if (raw != null && raw !== '') return TRUTHY.test(raw)
  return false
}
