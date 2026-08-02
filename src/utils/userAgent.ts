/**
 * User-Agent string helpers.
 *
 * Kept dependency-free so transport and API code can
 * import without pulling in auth.ts and its transitive dependency tree.
 */

export function getCrabCodeUserAgent(): string {
  return `crabcode/${MACRO.VERSION}`
}
