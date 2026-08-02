/**
 * W-MEMORY-KB-UPLIFT P4 (2026-07-17) — SQLite read-only snapshot ingestion
 * (决策 D7: v1 DB provider = SQLite via `bun:sqlite`, zero new deps;
 * PostgreSQL / MySQL are explicitly OUT of v1 scope).
 *
 * Security model (the user's「连接数据库获取的安全问题」ask):
 *   - read-only open (`{ readonly: true }`) — the engine cannot write even
 *     if validation were bypassed;
 *   - SELECT-only single-statement SQL validation, fail-closed word scan
 *     (false positives acceptable; a rejected legitimate query is a prompt
 *     rewrite away, a mutating query landing is not);
 *   - row cap (1000) + byte cap (256 KiB) on the rendered snapshot;
 *   - the snapshot is a DISABLED draft with provenance — never live query
 *     plumbing into context.
 */
import { Database } from 'bun:sqlite'

export const DB_INGEST_MAX_ROWS = 1_000
export const DB_INGEST_MAX_BYTES = 256 * 1024

const FORBIDDEN_SQL_WORDS =
  /\b(attach|detach|pragma|insert|update|delete|drop|create|alter|replace|vacuum|reindex|analyze|begin|commit|rollback|savepoint|release)\b/i

/**
 * Validate an ingest SQL string: single statement, SELECT/WITH head, no
 * mutating/DDL/transaction keywords anywhere (word-boundary scan — fail
 * closed on false positives by design).
 */
export function assertSelectOnlySql(sql: string): string {
  const trimmed = sql.trim().replace(/;\s*$/, '')
  if (trimmed.length === 0) {
    throw new Error('SQL 为空')
  }
  if (trimmed.includes(';')) {
    throw new Error('拒绝执行：仅允许单条语句（检测到多语句分号）')
  }
  if (!/^(select|with)\b/i.test(trimmed)) {
    throw new Error('拒绝执行：仅允许 SELECT / WITH 只读查询')
  }
  const forbidden = trimmed.match(FORBIDDEN_SQL_WORDS)
  if (forbidden) {
    throw new Error(
      `拒绝执行：查询含受限关键词「${forbidden[0]}」（只读快照白名单为 fail-closed，字符串字面量里含该词也会拒绝 — 请改写查询）`,
    )
  }
  return trimmed
}

export type DbSnapshot = {
  markdown: string
  rowCount: number
  truncated: boolean
}

function cell(value: unknown): string {
  if (value === null || value === undefined) return ''
  const text =
    value instanceof Uint8Array
      ? `<blob ${value.byteLength}B>`
      : String(value)
  return text.replace(/\|/g, '\\|').replace(/\r?\n/g, ' ')
}

/**
 * Run a validated read-only query against a local SQLite file and render the
 * result as a markdown table, capped by rows and bytes.
 */
export function snapshotSqlite(dbPath: string, sql: string): DbSnapshot {
  const validated = assertSelectOnlySql(sql)
  const db = new Database(dbPath, { readonly: true })
  try {
    const query = db.query(validated)
    const lines: string[] = []
    let header: string[] | null = null
    let rowCount = 0
    let bytes = 0
    let truncated = false

    for (const row of query.iterate() as IterableIterator<
      Record<string, unknown>
    >) {
      if (header === null) {
        header = Object.keys(row)
        const head = `| ${header.join(' | ')} |`
        const sep = `| ${header.map(() => '---').join(' | ')} |`
        lines.push(head, sep)
        bytes += head.length + sep.length + 2
      }
      const line = `| ${header.map(k => cell(row[k])).join(' | ')} |`
      if (rowCount >= DB_INGEST_MAX_ROWS || bytes + line.length > DB_INGEST_MAX_BYTES) {
        truncated = true
        break
      }
      lines.push(line)
      bytes += line.length + 1
      rowCount++
    }

    if (header === null) {
      return { markdown: '（查询返回 0 行）', rowCount: 0, truncated: false }
    }
    if (truncated) {
      lines.push('', `> （已按快照上限截断：${DB_INGEST_MAX_ROWS} 行 / ${DB_INGEST_MAX_BYTES} 字节）`)
    }
    return { markdown: lines.join('\n'), rowCount, truncated }
  } finally {
    db.close()
  }
}
