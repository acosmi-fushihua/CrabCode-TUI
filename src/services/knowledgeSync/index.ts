/**
 * W-MEMORY-KB-UPLIFT P5 (2026-07-17) — remote knowledge sync engine
 * (push-only, local-first; 决策 D9).
 *
 * Sync unit = truth-source markdown, NOT binary index segments (远端可自行
 * 重建索引；段格式不成为跨版本契约)：
 *   - scope `knowledge`: `<base>/knowledge/*.md`，**仅 enabled 条目**（草稿
 *     未过人审门不出本机 — 与检索信任边界一致）；
 *   - scope `global`:    `<base>/memory/*.md`（用户全局记忆）。
 *
 * Change detection = content hash (FNV-1a 64) diffed against
 * `<base>/.knowledge-sync-state.json`; deletions become remote tombstones
 * (points/delete). Push is leader-gated at the wiring layer + periodic per
 * `intervalHours`, with `knowledge_sync_now` as the member-gated manual
 * trigger. All failures are honest errors — never a fake "synced".
 */
import { readdir, readFile, stat, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { getCrabCodeConfigHomeDir } from '../../utils/envUtils.js'
import { getGlobalMemDir, getKnowledgeDir } from '../../memdir/paths.js'
import { fnv1a64Hex } from '../knowledgeIngest/draft.js'
import {
  loadKnowledgeSyncConfig,
  resolveApiKey,
  type KnowledgeSyncConfig,
  type KnowledgeSyncScope,
} from './config.js'
import {
  deletePoints,
  ensureCollection,
  upsertPoints,
  type QdrantPoint,
  type QdrantTarget,
} from './qdrant.js'

export { loadKnowledgeSyncConfig, knowledgeSyncConfigPath } from './config.js'

const STATE_FILENAME = '.knowledge-sync-state.json'
const UPSERT_BATCH = 32

type SyncStateFile = {
  lastRunMs?: number
  lastError?: string | null
  /** key = `<scope>/<fileName>` → last pushed content hash. */
  files?: Record<string, string>
}

function statePath(): string {
  return join(getCrabCodeConfigHomeDir(), STATE_FILENAME)
}

async function loadState(): Promise<SyncStateFile> {
  try {
    const raw = await readFile(statePath(), 'utf8')
    const parsed = JSON.parse(raw) as SyncStateFile
    return parsed && typeof parsed === 'object' ? parsed : {}
  } catch {
    return {}
  }
}

async function saveState(state: SyncStateFile): Promise<void> {
  await writeFile(statePath(), JSON.stringify(state, null, 2), 'utf8')
}

/** Deterministic pseudo-UUID point id from the sync key (Qdrant accepts UUIDs). */
export function pointIdForKey(key: string): string {
  const a = fnv1a64Hex(key)
  const b = fnv1a64Hex(`${key}#2`)
  const hex = `${a}${b}`
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20, 32)}`
}

function frontmatterFlag(content: string, key: string): string | null {
  const match = content.match(
    new RegExp(`^${key}\\s*:\\s*(.+)$`, 'm'),
  )
  return match?.[1]?.trim() ?? null
}

type ScopeFile = {
  key: string
  absPath: string
  name: string
  content: string
  mtimeMs: number
}

async function collectScopeFiles(scope: KnowledgeSyncScope): Promise<ScopeFile[]> {
  const dir = scope === 'knowledge' ? getKnowledgeDir() : getGlobalMemDir()
  let names: string[]
  try {
    names = (await readdir(dir)).filter(n => n.endsWith('.md'))
  } catch {
    return []
  }
  const out: ScopeFile[] = []
  for (const name of names) {
    const absPath = join(dir, name)
    try {
      const info = await stat(absPath)
      if (!info.isFile()) continue
      const content = await readFile(absPath, 'utf8')
      if (scope === 'knowledge') {
        // Un-enabled drafts / proposals never leave the machine (review gate).
        if (frontmatterFlag(content, 'enabled') === 'false') continue
        if (frontmatterFlag(content, 'pending_review') === 'true') continue
      }
      out.push({
        key: `${scope}/${name}`,
        absPath,
        name,
        content,
        mtimeMs: Math.round(info.mtimeMs),
      })
    } catch {
      // Single unreadable file must not abort the pass.
    }
  }
  return out
}

export type KnowledgeSyncSummary = {
  pushed: number
  deleted: number
  unchanged: number
  scopes: KnowledgeSyncScope[]
  endpoint: string
}

/**
 * Run one full push-sync against the configured remote. Throws on config /
 * transport errors (caller renders the message); returns an honest summary.
 */
export async function runKnowledgeSync(
  config: KnowledgeSyncConfig,
): Promise<KnowledgeSyncSummary> {
  const target: QdrantTarget = {
    endpoint: config.endpoint,
    collection: config.collection,
    apiKey: resolveApiKey(config),
  }
  if (config.apiKeyRef && target.apiKey === null) {
    throw new Error(
      `apiKeyRef 指向的环境变量 ${config.apiKeyRef} 未设置 — 凭证不落配置文件，需在运行环境提供`,
    )
  }
  await ensureCollection(target)

  const state = await loadState()
  const knownFiles = state.files ?? {}
  const seen = new Set<string>()
  let pushed = 0
  let unchanged = 0

  const pending: QdrantPoint[] = []
  const flush = async (): Promise<void> => {
    await upsertPoints(target, pending.splice(0, pending.length))
  }

  for (const scope of config.scopes) {
    for (const file of await collectScopeFiles(scope)) {
      seen.add(file.key)
      const hash = fnv1a64Hex(file.content)
      if (knownFiles[file.key] === hash) {
        unchanged++
        continue
      }
      pending.push({
        id: pointIdForKey(file.key),
        payload: {
          key: file.key,
          scope,
          name: file.name,
          path: file.absPath,
          content: file.content,
          content_hash: hash,
          mtime_ms: file.mtimeMs,
        },
      })
      knownFiles[file.key] = hash
      pushed++
      if (pending.length >= UPSERT_BATCH) {
        await flush()
      }
    }
  }
  await flush()

  // Tombstones: previously-pushed keys whose file is gone (or scope removed).
  const tombstoneKeys = Object.keys(knownFiles).filter(key => !seen.has(key))
  await deletePoints(
    target,
    tombstoneKeys.map(pointIdForKey),
  )
  for (const key of tombstoneKeys) {
    delete knownFiles[key]
  }

  state.files = knownFiles
  state.lastRunMs = Date.now()
  state.lastError = null
  await saveState(state)

  return {
    pushed,
    deleted: tombstoneKeys.length,
    unchanged,
    scopes: config.scopes,
    endpoint: config.endpoint,
  }
}

export type KnowledgeSyncNowResult =
  | { ok: true; summary: KnowledgeSyncSummary }
  | { ok: false; detail: string }

/**
 * Manual trigger (`knowledge_sync_now`): loads config, runs one sync, and
 * records the error into the state file on failure. The enable switch stays
 * user-edited-file-only by design.
 */
export async function runKnowledgeSyncNow(): Promise<KnowledgeSyncNowResult> {
  const loaded = await loadKnowledgeSyncConfig()
  if (loaded.status === 'missing') {
    return {
      ok: false,
      detail:
        '远程同步未配置。在 <配置根>/knowledge-sync.json 写入 {"version":1,"enabled":true,"endpoint":"https://…","collection":"crabcode-kb"}（可选 apiKeyRef=存放 API key 的环境变量名、scopes、intervalHours）后重试。enabled 开关只能人工编辑该文件。',
    }
  }
  if (loaded.status === 'disabled') {
    return {
      ok: false,
      detail:
        '远程同步已配置但未启用（enabled != true）。外发数据的开关权留给用户：请人工编辑 knowledge-sync.json 置 enabled: true。',
    }
  }
  if (loaded.status === 'invalid') {
    return { ok: false, detail: `knowledge-sync.json 配置无效：${loaded.reason}` }
  }
  try {
    const summary = await runKnowledgeSync(loaded.config)
    return { ok: true, summary }
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e)
    const state = await loadState()
    state.lastError = message
    state.lastRunMs = Date.now()
    await saveState(state).catch(() => {})
    return { ok: false, detail: `远程同步失败：${message}` }
  }
}

/**
 * Leader-gated periodic hook: runs a sync when enabled and the interval has
 * elapsed. Never throws (background lane); errors land in the state file.
 */
export async function maybeRunPeriodicKnowledgeSync(): Promise<void> {
  const loaded = await loadKnowledgeSyncConfig()
  if (loaded.status !== 'ready') return
  const state = await loadState()
  const intervalMs = loaded.config.intervalHours * 3_600_000
  if (state.lastRunMs && Date.now() - state.lastRunMs < intervalMs) return
  try {
    await runKnowledgeSync(loaded.config)
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e)
    const fresh = await loadState()
    fresh.lastError = message
    fresh.lastRunMs = Date.now()
    await saveState(fresh).catch(() => {})
  }
}
