/**
 * W-MEMORY-KB-UPLIFT P4 (2026-07-17) — knowledge ingestion service.
 *
 * Three ingest lanes (决策 D7), all producing DISABLED drafts behind the
 * human review gate, all fail-closed on policy violations:
 *   - `ingestUrlToDraft`  — hardened fetch (WebFetch SSRF stack reuse) →
 *     markdown → provenance draft;
 *   - `ingestFileToDraft` — local .md / .txt / .html（≤2 MB, html →
 *     markdown）; office/pdf 文本抽取 = v2 边界外（现有 office/pdf 管线的
 *     产物是页图像服务视觉链，无文本产物 — 场景由「agent 视觉读文件→对话
 *     提炼→knowledge_promote」覆盖）;
 *   - `ingestDbToDraft`   — SQLite read-only snapshot（SELECT-only 校验 +
 *     行/字节帽）.
 *
 * Callers (MemoryManage actions) enforce the KB premium gate BEFORE calling
 * in; this module is mechanism, not policy.
 */
import { readFile, stat } from 'node:fs/promises'
import { basename, extname, isAbsolute, resolve } from 'node:path'
import {
  fnv1a64Hex,
  writeIngestDraft,
  type IngestDraftResult,
} from './draft.js'
import { htmlToMarkdown } from './htmlToMarkdown.js'
import { snapshotSqlite } from './ingestDb.js'
import { safeIngestFetch } from './safeIngestFetch.js'

export { INGEST_DATA_BANNER, fnv1a64Hex } from './draft.js'
export { assertNoSecrets, scanForSecrets } from './secretScan.js'
export { assertSelectOnlySql } from './ingestDb.js'

const FILE_INGEST_MAX_BYTES = 2 * 1024 * 1024
const FILE_INGEST_EXTENSIONS = new Set(['.md', '.txt', '.html', '.htm'])

/** Extract an HTML <title> (best-effort) for draft naming. */
function extractHtmlTitle(html: string): string | null {
  const match = html.match(/<title[^>]*>([^<]{1,200})<\/title>/i)
  const title = match?.[1]?.trim()
  return title && title.length > 0 ? title : null
}

export async function ingestUrlToDraft(args: {
  url: string
  title?: string
  tags?: string[]
}): Promise<IngestDraftResult & { finalUrl: string }> {
  const fetched = await safeIngestFetch(args.url)
  const isHtml =
    fetched.contentType.startsWith('text/html') ||
    fetched.contentType.startsWith('application/xhtml+xml')
  const markdown = isHtml ? await htmlToMarkdown(fetched.body) : fetched.body
  const parsed = new URL(fetched.finalUrl)
  const title =
    args.title?.trim() ||
    (isHtml ? extractHtmlTitle(fetched.body) : null) ||
    `${parsed.hostname}${parsed.pathname}`
  const result = await writeIngestDraft({
    title,
    description: `网页抓取：${fetched.finalUrl}`,
    ...(args.tags ? { tags: args.tags } : {}),
    content: markdown,
    provenance: {
      source: 'web_ingest',
      sourceUrl: fetched.finalUrl,
      fetchedAtMs: Date.now(),
      contentHash: fnv1a64Hex(markdown),
    },
  })
  return { ...result, finalUrl: fetched.finalUrl }
}

export async function ingestFileToDraft(args: {
  filePath: string
  title?: string
  tags?: string[]
}): Promise<IngestDraftResult> {
  const abs = isAbsolute(args.filePath)
    ? args.filePath
    : resolve(args.filePath)
  const ext = extname(abs).toLowerCase()
  if (!FILE_INGEST_EXTENSIONS.has(ext)) {
    throw new Error(
      `不支持的文件类型「${ext || '(无扩展名)'}」— v1 支持 .md / .txt / .html。office/pdf 请让我用视觉阅读后提炼，再经 knowledge_promote 入库。`,
    )
  }
  const info = await stat(abs)
  if (!info.isFile()) {
    throw new Error('目标不是普通文件')
  }
  if (info.size > FILE_INGEST_MAX_BYTES) {
    throw new Error(
      `文件超过导入上限（${Math.round(info.size / 1024)}KiB > ${FILE_INGEST_MAX_BYTES / 1024}KiB）— 建议拆分或提炼后 knowledge_promote`,
    )
  }
  const raw = await readFile(abs, 'utf8')
  const markdown =
    ext === '.html' || ext === '.htm' ? await htmlToMarkdown(raw) : raw
  const title = args.title?.trim() || basename(abs)
  return writeIngestDraft({
    title,
    description: `文件导入：${abs}`,
    ...(args.tags ? { tags: args.tags } : {}),
    content: markdown,
    provenance: {
      source: 'file_ingest',
      sourcePath: abs,
      fetchedAtMs: Date.now(),
      contentHash: fnv1a64Hex(markdown),
    },
  })
}

export async function ingestDbToDraft(args: {
  dbPath: string
  sql: string
  title?: string
  tags?: string[]
}): Promise<IngestDraftResult & { rowCount: number; truncated: boolean }> {
  const abs = isAbsolute(args.dbPath) ? args.dbPath : resolve(args.dbPath)
  const info = await stat(abs)
  if (!info.isFile()) {
    throw new Error('SQLite 数据库路径不是普通文件')
  }
  const snapshot = snapshotSqlite(abs, args.sql)
  const title = args.title?.trim() || `DB 快照：${basename(abs)}`
  const content = [
    `查询（只读快照，${snapshot.rowCount} 行${snapshot.truncated ? '，已截断' : ''}）：`,
    '',
    '```sql',
    args.sql.trim(),
    '```',
    '',
    snapshot.markdown,
  ].join('\n')
  const result = await writeIngestDraft({
    title,
    description: `SQLite 只读快照：${abs}`,
    ...(args.tags ? { tags: args.tags } : {}),
    content,
    provenance: {
      source: 'db_ingest',
      sourcePath: abs,
      fetchedAtMs: Date.now(),
      contentHash: fnv1a64Hex(content),
    },
  })
  return { ...result, rowCount: snapshot.rowCount, truncated: snapshot.truncated }
}
