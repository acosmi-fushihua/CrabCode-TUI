/**
 * W-MEMORY-KB-UPLIFT P5 (2026-07-17) — remote knowledge-sync configuration.
 *
 * Config file: `<base>/knowledge-sync.json`. 决策 D9 安全条款：
 *   - `enabled` 只能由用户人工编辑该文件打开 — 工具/agent 不提供 enable
 *     动作（外发数据的开关权留给用户）；
 *   - endpoint 强制 https（localhost/127.0.0.1 的 http 例外，自托管本机）；
 *   - 凭证绝不落 json：`apiKeyRef` 是**环境变量名**（如
 *     `CRABCODE_KB_SYNC_API_KEY`），运行时解引用。
 */
import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { getCrabCodeConfigHomeDir } from '../../utils/envUtils.js'

export const KNOWLEDGE_SYNC_CONFIG_FILENAME = 'knowledge-sync.json'

export type KnowledgeSyncScope = 'knowledge' | 'global'

export type KnowledgeSyncConfig = {
  version: 1
  enabled: boolean
  /** Qdrant-compatible REST endpoint, e.g. `https://qdrant.example.com:6333`. */
  endpoint: string
  /** Target collection name prefix (per-scope suffixing happens in the engine). */
  collection: string
  /** Env-var NAME holding the api key (dereferenced at runtime; may be absent for keyless self-hosted). */
  apiKeyRef?: string
  scopes: KnowledgeSyncScope[]
  intervalHours: number
}

export function knowledgeSyncConfigPath(): string {
  return join(getCrabCodeConfigHomeDir(), KNOWLEDGE_SYNC_CONFIG_FILENAME)
}

export type KnowledgeSyncConfigResult =
  | { status: 'missing' }
  | { status: 'disabled' }
  | { status: 'invalid'; reason: string }
  | { status: 'ready'; config: KnowledgeSyncConfig }

const COLLECTION_RE = /^[A-Za-z0-9_-]{1,64}$/

function isLoopbackHost(hostname: string): boolean {
  return (
    hostname === 'localhost' ||
    hostname === '127.0.0.1' ||
    hostname === '::1' ||
    hostname === '[::1]'
  )
}

export function validateKnowledgeSyncEndpoint(raw: string): string | null {
  let parsed: URL
  try {
    parsed = new URL(raw)
  } catch {
    return 'endpoint 不是合法 URL'
  }
  if (parsed.protocol === 'https:') return null
  if (parsed.protocol === 'http:' && isLoopbackHost(parsed.hostname)) {
    return null
  }
  return 'endpoint 必须是 https（本机 localhost 可用 http）'
}

/** Load + validate the sync config. Fail-closed on any malformation. */
export async function loadKnowledgeSyncConfig(): Promise<KnowledgeSyncConfigResult> {
  let raw: string
  try {
    raw = await readFile(knowledgeSyncConfigPath(), 'utf8')
  } catch {
    return { status: 'missing' }
  }
  let parsed: unknown
  try {
    parsed = JSON.parse(raw)
  } catch {
    return { status: 'invalid', reason: 'knowledge-sync.json 不是合法 JSON' }
  }
  if (parsed === null || typeof parsed !== 'object') {
    return { status: 'invalid', reason: '配置根必须是对象' }
  }
  const obj = parsed as Record<string, unknown>
  if (obj.enabled !== true) {
    return { status: 'disabled' }
  }
  if (typeof obj.endpoint !== 'string' || obj.endpoint.length === 0) {
    return { status: 'invalid', reason: '缺少 endpoint' }
  }
  const endpointError = validateKnowledgeSyncEndpoint(obj.endpoint)
  if (endpointError) {
    return { status: 'invalid', reason: endpointError }
  }
  if (typeof obj.collection !== 'string' || !COLLECTION_RE.test(obj.collection)) {
    return {
      status: 'invalid',
      reason: 'collection 必须匹配 [A-Za-z0-9_-]{1,64}',
    }
  }
  if (obj.apiKeyRef !== undefined && typeof obj.apiKeyRef !== 'string') {
    return { status: 'invalid', reason: 'apiKeyRef 必须是环境变量名字符串' }
  }
  const scopes: KnowledgeSyncScope[] = []
  const rawScopes = Array.isArray(obj.scopes) ? obj.scopes : ['knowledge']
  for (const scope of rawScopes) {
    if (scope === 'knowledge' || scope === 'global') {
      if (!scopes.includes(scope)) scopes.push(scope)
    } else {
      return {
        status: 'invalid',
        reason: `未知 scope「${String(scope)}」（合法：knowledge / global）`,
      }
    }
  }
  if (scopes.length === 0) {
    return { status: 'invalid', reason: 'scopes 至少一个' }
  }
  const intervalRaw =
    typeof obj.intervalHours === 'number' && Number.isFinite(obj.intervalHours)
      ? obj.intervalHours
      : 24
  const intervalHours = Math.min(168, Math.max(1, intervalRaw))

  return {
    status: 'ready',
    config: {
      version: 1,
      enabled: true,
      endpoint: obj.endpoint.replace(/\/+$/, ''),
      collection: obj.collection,
      ...(obj.apiKeyRef ? { apiKeyRef: obj.apiKeyRef } : {}),
      scopes,
      intervalHours,
    },
  }
}

/** Dereference the api key env var (never stored in config/json). */
export function resolveApiKey(config: KnowledgeSyncConfig): string | null {
  if (!config.apiKeyRef) return null
  const value =
    typeof process !== 'undefined' ? process.env?.[config.apiKeyRef] : undefined
  return value && value.length > 0 ? value : null
}
