/**
 * W-MEMORY-KB-UPLIFT P5 (2026-07-17) — minimal Qdrant-compatible REST
 * client for push-only sync (决策 D9): ensure-collection + upsert + delete.
 * No query surface — v1 remote is a truth-source mirror（远端检索 = v2 边界
 * 外，本地 acosmi-se 仍是唯一检索底座）.
 *
 * Points carry `vector: [0]`（dim-1 占位，与本地 Phase-1 词法集合同构 —
 * 远端要建真向量索引由远端自跑 embedding，不在本链路）+ full payload
 * (path / name / content / frontmatter / hash / mtime)。
 */

export type QdrantTarget = {
  endpoint: string
  collection: string
  apiKey: string | null
}

export type QdrantPoint = {
  id: string
  payload: Record<string, unknown>
}

const REQUEST_TIMEOUT_MS = 15_000

async function qdrantRequest(
  target: QdrantTarget,
  method: 'GET' | 'PUT' | 'POST',
  path: string,
  body?: unknown,
): Promise<Response> {
  const response = await fetch(`${target.endpoint}${path}`, {
    method,
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    headers: {
      'Content-Type': 'application/json',
      ...(target.apiKey ? { 'api-key': target.apiKey } : {}),
    },
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  })
  if (!response.ok) {
    const text = await response.text().catch(() => '')
    throw new Error(
      `远程库请求失败 ${method} ${path}: HTTP ${response.status} ${text.slice(0, 300)}`,
    )
  }
  return response
}

/** Idempotently ensure the collection exists (dim-1 dot placeholder vectors). */
export async function ensureCollection(target: QdrantTarget): Promise<void> {
  const probe = await fetch(
    `${target.endpoint}/collections/${target.collection}`,
    {
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      headers: target.apiKey ? { 'api-key': target.apiKey } : {},
    },
  )
  if (probe.ok) return
  await qdrantRequest(target, 'PUT', `/collections/${target.collection}`, {
    vectors: { size: 1, distance: 'Dot' },
  })
}

export async function upsertPoints(
  target: QdrantTarget,
  points: QdrantPoint[],
): Promise<void> {
  if (points.length === 0) return
  await qdrantRequest(
    target,
    'PUT',
    `/collections/${target.collection}/points?wait=true`,
    {
      points: points.map(point => ({
        id: point.id,
        vector: [0],
        payload: point.payload,
      })),
    },
  )
}

export async function deletePoints(
  target: QdrantTarget,
  ids: string[],
): Promise<void> {
  if (ids.length === 0) return
  await qdrantRequest(
    target,
    'POST',
    `/collections/${target.collection}/points/delete?wait=true`,
    { points: ids },
  )
}
