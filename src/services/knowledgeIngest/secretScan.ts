/**
 * W-MEMORY-KB-UPLIFT P4 (2026-07-17) — pre-write secret scan for knowledge
 * ingestion. Ingested content (web pages, files, DB snapshots) must never
 * land credentials into `<base>/knowledge/` — entries are long-lived,
 * cross-project, and (once enabled) recalled into model context.
 *
 * FAIL-CLOSED by design (决策 D7): a hit REJECTS the write with the matched
 * pattern names; the agent asks the user to redact and retry. This is a
 * deliberate high-signal / low-noise pattern set (well-known credential
 * shapes), not a general PII scanner.
 */

type SecretPattern = {
  name: string
  regex: RegExp
}

const SECRET_PATTERNS: SecretPattern[] = [
  { name: 'aws-access-key-id', regex: /\bAKIA[0-9A-Z]{16}\b/ },
  { name: 'github-token', regex: /\bgh[pousr]_[A-Za-z0-9]{36,}\b/ },
  { name: 'slack-token', regex: /\bxox[baprs]-[A-Za-z0-9-]{10,}\b/ },
  { name: 'private-key-block', regex: /-----BEGIN [A-Z ]*PRIVATE KEY-----/ },
  { name: 'openai-style-key', regex: /\bsk-[A-Za-z0-9]{20,}\b/ },
  { name: 'google-api-key', regex: /\bAIza[0-9A-Za-z_-]{35}\b/ },
  {
    name: 'password-assignment',
    // `password=...` / `passwd: ...` with a non-trivial value on the same
    // line. Anchored to a word boundary + 6+ non-space chars to keep prose
    // like「password managers」out.
    regex: /\b(?:password|passwd|pwd)\s*[:=]\s*['"]?[^\s'"]{6,}/i,
  },
  {
    name: 'connection-string-credentials',
    // scheme://user:pass@host — DB DSNs with inline credentials.
    regex: /\b[a-z][a-z0-9+.-]*:\/\/[^\s/:@]+:[^\s/@]+@[^\s/]+/i,
  },
]

export type SecretScanHit = {
  pattern: string
}

/** Returns the matched pattern names (empty = clean). */
export function scanForSecrets(text: string): SecretScanHit[] {
  const hits: SecretScanHit[] = []
  for (const { name, regex } of SECRET_PATTERNS) {
    if (regex.test(text)) {
      hits.push({ pattern: name })
    }
  }
  return hits
}

/**
 * Throwing wrapper for the ingest pipeline: rejects the write when any
 * credential-shaped content is present.
 */
export function assertNoSecrets(text: string, sourceLabel: string): void {
  const hits = scanForSecrets(text)
  if (hits.length > 0) {
    const names = hits.map(h => h.pattern).join(', ')
    throw new Error(
      `拒绝写入：${sourceLabel} 内容命中凭证特征（${names}）。请先脱敏（删除密钥/口令/带凭证的连接串）后重试 —— 知识库条目是跨项目长期保存并会进入模型上下文的。`,
    )
  }
}
