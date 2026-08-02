import { mkdir, readdir, readFile, realpath, stat, writeFile } from 'node:fs/promises'
import { basename, join, resolve, sep } from 'node:path'

import { z } from 'zod/v4'

import { memoryBridgeIpc } from 'src/services/memoryRuntime/client.js'
import { getSessionId } from '../../bootstrap/state.js'
import { getAutoMemPath, getKnowledgeDir } from '../../memdir/paths.js'
import { getRustDerivedMemoryPath } from '../../memdir/rustDerivedPaths.js'
import {
  fnv1a64Hex,
  writeIngestDraft,
} from '../../services/knowledgeIngest/draft.js'
import {
  ingestDbToDraft,
  ingestFileToDraft,
  ingestUrlToDraft,
} from '../../services/knowledgeIngest/index.js'
import { runKnowledgeSyncNow } from '../../services/knowledgeSync/index.js'
import { getMembershipGateInput } from '../../utils/auth.js'
import {
  kbPremiumBlockedDetail,
  resolveKbPremiumEntitlement,
} from '../../utils/entitlements/knowledgeBase.js'
import { buildTool, type ToolDef } from '../../Tool.js'
import { logForDebugging } from '../../utils/debug.js'
import { errorMessage } from '../../utils/errors.js'
import { lazySchema } from '../../utils/lazySchema.js'
import { jsonStringify } from '../../utils/slowOperations.js'
import { DESCRIPTION, MEMORY_MANAGE_TOOL_NAME, PROMPT } from './prompt.js'

const DREAM_RUN_NOW_TIMEOUT_MS = 5_000
const WATCH_LIST_TIMEOUT_MS = 3_000
/** 草稿正文上限（与 dispatcher knowledge/write 的 512KiB 防线同级收紧）。 */
const DRAFT_MAX_CONTENT_CHARS = 100_000
const DRAFT_MAX_TAGS = 6

const inputSchema = lazySchema(() =>
  z.object({
    action: z
      .enum([
        'dream_now',
        'knowledge_draft',
        'watch_list',
        'evolution_status',
        'knowledge_list',
        'knowledge_read',
        'knowledge_promote',
        'knowledge_ingest_url',
        'knowledge_ingest_file',
        'knowledge_ingest_db',
        'knowledge_sync_now',
      ])
      .describe(
        'dream_now = trigger one dream consolidation for the current project; ' +
          'knowledge_draft = save a disabled draft into the personal knowledge base for user review; ' +
          'watch_list = read-only list of 专项检测 watch targets; ' +
          'evolution_status = read-only fitness/trial/variant status of the memory self-evolution engine; ' +
          'knowledge_list = read-only listing of personal knowledge-base entries (name/enabled/injection); ' +
          'knowledge_read = read-only body of one knowledge entry by absolute path; ' +
          'knowledge_promote = distill a memory file (source_path) or inline content into a disabled knowledge draft with provenance; ' +
          'knowledge_ingest_url = fetch a web page into a disabled knowledge draft (member feature, SSRF-guarded); ' +
          'knowledge_ingest_file = import a local .md/.txt/.html file into a disabled knowledge draft (member feature); ' +
          'knowledge_ingest_db = snapshot a read-only SELECT query from a local SQLite file into a disabled knowledge draft (member feature); ' +
          'knowledge_sync_now = push enabled knowledge + global memory to the user-configured remote database (member feature; enable switch is user-file-only).',
      ),
    title: z
      .string()
      .max(120)
      .optional()
      .describe('knowledge_draft only: entry title (required for drafts).'),
    description: z
      .string()
      .max(300)
      .optional()
      .describe('knowledge_draft only: one-line summary.'),
    content: z
      .string()
      .max(DRAFT_MAX_CONTENT_CHARS)
      .optional()
      .describe('knowledge_draft only: markdown body (required for drafts).'),
    tags: z
      .array(z.string().max(40))
      .max(DRAFT_MAX_TAGS)
      .optional()
      .describe('knowledge_draft / promote / ingest actions: up to 6 tags.'),
    path: z
      .string()
      .optional()
      .describe('knowledge_read only: absolute path of the entry (from knowledge_list).'),
    source_path: z
      .string()
      .optional()
      .describe(
        'knowledge_promote only: absolute path of the memory file to distill; omit when passing inline content.',
      ),
    url: z
      .string()
      .max(2000)
      .optional()
      .describe('knowledge_ingest_url only: http(s) URL to fetch.'),
    file_path: z
      .string()
      .optional()
      .describe('knowledge_ingest_file only: local .md/.txt/.html file path.'),
    db_path: z
      .string()
      .optional()
      .describe('knowledge_ingest_db only: local SQLite database file path.'),
    sql: z
      .string()
      .max(4000)
      .optional()
      .describe('knowledge_ingest_db only: single read-only SELECT/WITH statement.'),
  }),
)
type InputSchema = ReturnType<typeof inputSchema>

const outputSchema = lazySchema(() =>
  z.object({
    action: z.string(),
    ok: z.boolean(),
    /** Human-readable outcome (语言随内容，模型直接转述给用户即可)。 */
    detail: z.string(),
    /** knowledge_draft / promote / ingest: absolute path of the written draft; knowledge_read: entry path. */
    path: z.string().nullable(),
    /** knowledge_list: entry projection (newest-first, capped). Optional —
     * only the knowledge_list action sets it (legacy actions keep their
     * four-field shape untouched). */
    entries: z
      .array(
        z.object({
          path: z.string(),
          name: z.string().nullable(),
          description: z.string().nullable(),
          enabled: z.boolean(),
          injection: z.string(),
          pendingReview: z.boolean(),
          mtimeMs: z.number(),
        }),
      )
      .nullable()
      .optional(),
    /** knowledge_read: entry BODY (frontmatter stripped, capped). Optional —
     * only the knowledge_read action sets it. */
    content: z.string().nullable().optional(),
    /** watch_list: configured targets projection. */
    targets: z
      .array(
        z.object({
          id: z.string(),
          label: z.string().nullable(),
          path: z.string(),
          intervalHours: z.number(),
          enabled: z.boolean(),
          lastStatus: z.string().nullable(),
        }),
      )
      .nullable(),
    /** evolution_status: 进化引擎态势（W-MEMORY-SELF-EVOLVE-DGM 8a-8d）。 */
    evolution: z
      .object({
        /** 最新复合适应度（null = 数据尚不足）。 */
        composite: z.number().nullable(),
        hitRate: z.number().nullable(),
        duplicationRate: z.number().nullable(),
        survivorEfficiency: z.number().nullable(),
        gateWasteRatio: z.number().nullable(),
        /** 适应度历史条数。 */
        snapshots: z.number(),
        /** 参数试验台账尾部（人可读单行）。 */
        ledgerTail: z.array(z.string()),
        /** prompt 变体胜负（人可读单行）。 */
        variants: z.array(z.string()),
        /** knowledge/proposals 里待审提案数。 */
        pendingProposals: z.number(),
      })
      .nullable(),
  }),
)
type OutputSchema = ReturnType<typeof outputSchema>
export type Output = z.infer<OutputSchema>

function unavailable(action: string): Output {
  return {
    action,
    ok: false,
    detail:
      '记忆服务在当前环境不可用（memory orchestrator 未连接），无法执行该操作。',
    path: null,
    targets: null,
    evolution: null,
  }
}

async function readJsonFile(path: string): Promise<unknown> {
  try {
    return JSON.parse(await readFile(path, 'utf8')) as unknown
  } catch {
    return null
  }
}

const num = (value: unknown): number | null =>
  typeof value === 'number' && Number.isFinite(value) ? value : null

/**
 * W-MEMORY-SELF-EVOLVE-DGM (2026-07-16) — 进化引擎态势直读（纯只读、零
 * orchestrator 依赖：全部来自 `.memory-rust-derived/evolution/` 派生层与
 * `<base>/knowledge/proposals/`，与 Rust 侧文件形状契约对齐，两侧测试各
 * 自钉死）。任何缺失/损坏 → 字段级 null / 空数组，绝不 throw。目录经参
 * 数注入（路径解析函数进程级 memoize，测试注入夹具目录）。
 */
export async function readEvolutionStatus(dirs: {
  evolutionDir: string
  proposalsDir: string
}): Promise<NonNullable<Output['evolution']>> {
  const { evolutionDir, proposalsDir } = dirs

  const history = (await readJsonFile(
    join(evolutionDir, 'fitness-history.json'),
  )) as unknown[] | null
  const snapshots = Array.isArray(history) ? history : []
  const latest = (snapshots.at(-1) ?? null) as Record<string, unknown> | null

  let ledgerTail: string[] = []
  try {
    const raw = await readFile(join(evolutionDir, 'evolution-ledger.jsonl'), 'utf8')
    ledgerTail = raw
      .split('\n')
      .filter(line => line.trim().length > 0)
      .slice(-5)
      .map(line => {
        try {
          const entry = JSON.parse(line) as Record<string, unknown>
          return `${String(entry.status)} ${String(entry.param)} ${String(entry.from)} → ${String(entry.to)}`
        } catch {
          return line.slice(0, 120)
        }
      })
  } catch {
    // 无台账 = 尚未试验过，正常。
  }

  const archive = (await readJsonFile(
    join(evolutionDir, 'variant-archive.json'),
  )) as { stats?: Record<string, { wins?: number; losses?: number }> } | null
  const variants = Object.entries(archive?.stats ?? {}).map(
    ([id, stats]) =>
      `${id}: ${stats?.wins ?? 0} wins / ${(stats?.wins ?? 0) + (stats?.losses ?? 0)} samples`,
  )

  let pendingProposals = 0
  try {
    const entries = (await readdir(proposalsDir)).filter(
      name => name.startsWith('proposal-') && name.endsWith('.md'),
    )
    for (const name of entries.slice(0, 20)) {
      try {
        const content = await readFile(join(proposalsDir, name), 'utf8')
        if (content.includes('pending_review: true')) pendingProposals++
      } catch {
        // 单文件读失败不影响计数其余。
      }
    }
  } catch {
    // 无提案目录 = 0，正常。
  }

  return {
    composite: num(latest?.composite),
    hitRate: num(latest?.retrieval_hit_rate),
    duplicationRate: num(latest?.duplication_rate),
    survivorEfficiency: num(latest?.survivor_efficiency),
    gateWasteRatio: num(latest?.gate_waste_ratio),
    snapshots: snapshots.length,
    ledgerTail,
    variants,
    pendingProposals,
  }
}

/** `title` → 知识库草稿文件名 slug（镜像 dispatcher knowledge/write 的
 * slug 化防撞语义：非字母数字折叠为 `-`，防路径逃逸）。 */
export function draftFileName(title: string): string {
  const slug = title
    .toLowerCase()
    .replace(/[^\p{L}\p{N}]+/gu, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 64)
  return `draft-${slug.length > 0 ? slug : 'untitled'}.md`
}

/** 组装草稿 frontmatter + 正文。`enabled: false` + `pending_review: true`
 * 是硬契约：用户必须在返回的文件中审阅并显式启用后才参与召回。 */
export function renderDraft(input: {
  title: string
  description?: string
  tags?: string[]
  content: string
}): string {
  // 冒号/换行会破坏 knowledge/list 使用的行级 frontmatter 解析，
  // 值内统一替换为全角/空格（毁灭复核修正）。
  const fmSafe = (value: string): string =>
    value.replace(/\n/g, ' ').replace(/:/g, '：').trim()
  const lines = ['---', 'type: knowledge']
  lines.push(`name: ${fmSafe(input.title)}`)
  if (input.description) {
    lines.push(`description: ${fmSafe(input.description)}`)
  }
  if (input.tags && input.tags.length > 0) {
    lines.push(`tags: [${input.tags.map(tag => tag.replace(/[\[\],\n]/g, ' ').trim()).join(', ')}]`)
  }
  lines.push('enabled: false')
  lines.push('injection: manual')
  lines.push('pending_review: true')
  lines.push('source: agent_draft')
  lines.push(`created_at_ms: ${Date.now()}`)
  lines.push('---', '', input.content.trim(), '')
  return lines.join('\n')
}

function parseWatchTargets(response: unknown): Output['targets'] {
  if (response === null || typeof response !== 'object') return []
  const raw = (response as { targets?: unknown }).targets
  if (!Array.isArray(raw)) return []
  const targets: NonNullable<Output['targets']> = []
  for (const entry of raw) {
    if (entry === null || typeof entry !== 'object') continue
    const target = entry as Record<string, unknown>
    if (typeof target.id !== 'string' || typeof target.path !== 'string') {
      continue
    }
    targets.push({
      id: target.id,
      label: typeof target.label === 'string' ? target.label : null,
      path: target.path,
      intervalHours:
        typeof target.interval_hours === 'number'
          ? target.interval_hours
          : typeof target.intervalHours === 'number'
            ? target.intervalHours
            : 48,
      enabled: target.enabled !== false,
      lastStatus:
        typeof target.last_status === 'string'
          ? target.last_status
          : typeof target.lastStatus === 'string'
            ? target.lastStatus
            : null,
    })
  }
  return targets
}

// ──────────────────────────────────────────────────────────────────────────
// W-MEMORY-KB-UPLIFT P2/P4/P5 (2026-07-17) — knowledge browse / promote /
// ingest / sync helpers.
// ──────────────────────────────────────────────────────────────────────────

/** Read cap for knowledge bodies (mirrors dispatcher `knowledge/read` 512KiB). */
const KNOWLEDGE_READ_MAX_BYTES = 512 * 1024
const KNOWLEDGE_LIST_MAX_ENTRIES = 200

/** Line-level frontmatter field grab (mirrors the dispatcher projection). */
function frontmatterField(content: string, key: string): string | null {
  const match = content.match(new RegExp(`^${key}\\s*:\\s*(.+)$`, 'm'))
  return match?.[1]?.trim() ?? null
}

/** Strip a leading `---…---` frontmatter block (dispatcher read contract: BODY only). */
export function stripFrontmatter(content: string): string {
  if (!content.startsWith('---')) return content
  const close = content.indexOf('\n---', 3)
  if (close === -1) return content
  const after = content.indexOf('\n', close + 4)
  return after === -1 ? '' : content.slice(after + 1)
}

/**
 * Path defense mirroring the dispatcher `resolve_knowledge_file_path`
 * contract: no control chars, absolute-resolved, canonicalized (symlink
 * escapes collapsed), must land INSIDE the canonical knowledge root, `.md`
 * only.
 */
async function resolveKnowledgePathDefended(raw: string): Promise<string> {
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f]/.test(raw)) throw new Error('路径含控制字符')
  const rootCanonical = await realpath(getKnowledgeDir())
  const candidate = await realpath(resolve(raw))
  if (!candidate.toLowerCase().endsWith('.md')) {
    throw new Error('仅允许读取 .md 条目')
  }
  const prefix = rootCanonical.endsWith(sep) ? rootCanonical : rootCanonical + sep
  if (!candidate.startsWith(prefix)) {
    throw new Error('路径必须位于个人知识库目录内')
  }
  return candidate
}

/** KB premium gate (决策 D8): null = allowed, else the honest blocked detail. */
function kbPremiumGateDetail(): string | null {
  const entitlement = resolveKbPremiumEntitlement(getMembershipGateInput())
  return entitlement.allowed ? null : kbPremiumBlockedDetail(entitlement.reason)
}

/**
 * 记忆系统的智能体受控写面，与只读的 `MemorySearch` 配对：
 *   * `dream_now` 走 orchestrator 全 gate（锁 / 空语料门），不绕任何护栏；
 *   * `knowledge_draft` 只产 `enabled: false` 草稿，用户在文件中显式启用
 *     前绝不参与召回；
 *   * `watch_list` 只读，避免在工具侧复制路径与 slug 写入规则。
 * 非只读工具 → 每次调用走权限确认门。CLI 无 orchestrator 时诚实降级。
 */
export const MemoryManageTool = buildTool({
  name: MEMORY_MANAGE_TOOL_NAME,
  searchHint: 'trigger memory dream / draft knowledge / list watch targets',
  maxResultSizeChars: 20_000,
  isConcurrencySafe(input) {
    return (
      input.action === 'watch_list' ||
      input.action === 'evolution_status' ||
      input.action === 'knowledge_list' ||
      input.action === 'knowledge_read'
    )
  },
  isReadOnly(input) {
    return (
      input.action === 'watch_list' ||
      input.action === 'evolution_status' ||
      input.action === 'knowledge_list' ||
      input.action === 'knowledge_read'
    )
  },
  toAutoClassifierInput(input) {
    return input.action
  },
  async description() {
    return DESCRIPTION
  },
  async prompt() {
    return PROMPT
  },
  get inputSchema(): InputSchema {
    return inputSchema()
  },
  get outputSchema(): OutputSchema {
    return outputSchema()
  },
  async call(input) {
    const { action } = input
    const fail = (detail: string): { data: Output } => ({
      data: { action, ok: false, detail, path: null, targets: null, evolution: null },
    })
    if (action === 'evolution_status') {
      // 纯只读直读派生层（不经 orchestrator，bare CLI 也可用）。
      const evolution = await readEvolutionStatus({
        evolutionDir: join(getRustDerivedMemoryPath(), 'evolution'),
        // 提案 = 知识库顶层 proposal-*.md，可用终端编辑器直接审阅。
        proposalsDir: getKnowledgeDir(),
      })
      const detail =
        evolution.snapshots === 0
          ? '进化引擎尚无适应度数据（做梦周期运行后会自动累计）。'
          : `适应度快照 ${evolution.snapshots} 份，最新复合分 ${
              evolution.composite === null
                ? '（数据不足）'
                : evolution.composite.toFixed(3)
            }；试验台账 ${evolution.ledgerTail.length} 条尾部，待审提案 ${evolution.pendingProposals} 份。`
      return {
        data: {
          action,
          ok: true,
          detail,
          path: null,
          targets: null,
          evolution,
        },
      }
    }
    // ── W-MEMORY-KB-UPLIFT P2 — knowledge browse（只读，bare CLI 可用）──
    if (action === 'knowledge_list') {
      try {
        const dir = getKnowledgeDir()
        const names = (await readdir(dir).catch(() => [] as string[])).filter(
          name => name.endsWith('.md'),
        )
        const projected: NonNullable<Output['entries']> = []
        for (const name of names) {
          const entryPath = join(dir, name)
          try {
            const info = await stat(entryPath)
            if (!info.isFile()) continue
            const head = (await readFile(entryPath, 'utf8')).slice(0, 8192)
            projected.push({
              path: entryPath,
              name: frontmatterField(head, 'name'),
              description: frontmatterField(head, 'description'),
              enabled: frontmatterField(head, 'enabled') !== 'false',
              injection: frontmatterField(head, 'injection') ?? 'auto',
              pendingReview: frontmatterField(head, 'pending_review') === 'true',
              mtimeMs: Math.round(info.mtimeMs),
            })
          } catch {
            // 单文件失败不影响其余。
          }
        }
        projected.sort((a, b) => b.mtimeMs - a.mtimeMs)
        const entries = projected.slice(0, KNOWLEDGE_LIST_MAX_ENTRIES)
        return {
          data: {
            action,
            ok: true,
            detail:
              entries.length === 0
                ? '个人知识库为空。'
                : `共 ${projected.length} 条（展示最新 ${entries.length} 条；enabled=false 为待审草稿，不参与召回）。`,
            path: null,
            targets: null,
            evolution: null,
            entries,
          },
        }
      } catch (e) {
        return fail(`知识库列举失败：${errorMessage(e)}`)
      }
    }
    if (action === 'knowledge_read') {
      const raw = input.path?.trim()
      if (!raw) return fail('knowledge_read 需要 path 参数（来自 knowledge_list）。')
      try {
        const entryPath = await resolveKnowledgePathDefended(raw)
        const info = await stat(entryPath)
        const truncated = info.size > KNOWLEDGE_READ_MAX_BYTES
        const rawContent = await readFile(entryPath, 'utf8')
        const body = stripFrontmatter(
          truncated ? rawContent.slice(0, KNOWLEDGE_READ_MAX_BYTES) : rawContent,
        )
        return {
          data: {
            action,
            ok: true,
            detail: truncated ? '已读取（超 512KiB 截断）。' : '已读取。',
            path: entryPath,
            targets: null,
            evolution: null,
            content: body,
          },
        }
      } catch (e) {
        return fail(`知识条目读取失败：${errorMessage(e)}`)
      }
    }
    // ── W-MEMORY-KB-UPLIFT P2 — 提炼晋升（本地整理，免费面）──
    if (action === 'knowledge_promote') {
      const title = input.title?.trim()
      if (!title) return fail('knowledge_promote 需要 title。')
      let content = input.content?.trim() ?? ''
      let sourcePath: string | undefined
      if (input.source_path?.trim()) {
        sourcePath = resolve(input.source_path.trim())
        try {
          const info = await stat(sourcePath)
          if (!info.isFile() || info.size > KNOWLEDGE_READ_MAX_BYTES) {
            throw new Error('来源必须是 ≤512KiB 的普通文件')
          }
          const rawSource = await readFile(sourcePath, 'utf8')
          const sourceBody = stripFrontmatter(rawSource).trim()
          content =
            content.length > 0 ? `${content}\n\n---\n\n${sourceBody}` : sourceBody
        } catch (e) {
          return fail(`读取晋升来源失败：${errorMessage(e)}`)
        }
      }
      if (content.length === 0) {
        return fail('knowledge_promote 需要 content 或 source_path 至少一个。')
      }
      try {
        const result = await writeIngestDraft({
          title,
          ...(input.description ? { description: input.description } : {}),
          ...(input.tags ? { tags: input.tags } : {}),
          content,
          provenance: {
            source: 'promote',
            ...(sourcePath ? { sourcePath } : {}),
            fetchedAtMs: Date.now(),
            contentHash: fnv1a64Hex(content),
          },
        })
        return {
          data: {
            action,
            ok: true,
            detail:
              '提炼内容已写入个人知识库草稿（未启用，带来源出处）。请提醒用户到「做梦空间 → 个人知识库」审阅并启用后才会参与记忆召回。',
            path: result.path,
            targets: null,
            evolution: null,
          },
        }
      } catch (e) {
        return fail(`晋升草稿写入失败：${errorMessage(e)}`)
      }
    }
    // ── W-MEMORY-KB-UPLIFT P4/P5 — 会员面（进料三件套 + 远程同步）──
    if (
      action === 'knowledge_ingest_url' ||
      action === 'knowledge_ingest_file' ||
      action === 'knowledge_ingest_db' ||
      action === 'knowledge_sync_now'
    ) {
      const blocked = kbPremiumGateDetail()
      if (blocked) return fail(blocked)
    }
    if (action === 'knowledge_ingest_url') {
      const url = input.url?.trim()
      if (!url) return fail('knowledge_ingest_url 需要 url。')
      try {
        const result = await ingestUrlToDraft({
          url,
          ...(input.title ? { title: input.title } : {}),
          ...(input.tags ? { tags: input.tags } : {}),
        })
        return {
          data: {
            action,
            ok: true,
            detail: `已抓取「${result.finalUrl}」→ 知识库草稿（未启用，带 provenance 与外部数据横幅）。请提醒用户到「做梦空间 → 个人知识库」审阅启用；启用前不参与召回。`,
            path: result.path,
            targets: null,
            evolution: null,
          },
        }
      } catch (e) {
        return fail(`网页进料失败：${errorMessage(e)}`)
      }
    }
    if (action === 'knowledge_ingest_file') {
      const filePath = input.file_path?.trim()
      if (!filePath) return fail('knowledge_ingest_file 需要 file_path。')
      try {
        const result = await ingestFileToDraft({
          filePath,
          ...(input.title ? { title: input.title } : {}),
          ...(input.tags ? { tags: input.tags } : {}),
        })
        return {
          data: {
            action,
            ok: true,
            detail: `已导入「${basename(result.path)}」→ 知识库草稿（未启用）。请提醒用户到「做梦空间 → 个人知识库」审阅启用。`,
            path: result.path,
            targets: null,
            evolution: null,
          },
        }
      } catch (e) {
        return fail(`文件进料失败：${errorMessage(e)}`)
      }
    }
    if (action === 'knowledge_ingest_db') {
      const dbPath = input.db_path?.trim()
      const sql = input.sql?.trim()
      if (!dbPath || !sql) return fail('knowledge_ingest_db 需要 db_path 与 sql。')
      try {
        const result = await ingestDbToDraft({
          dbPath,
          sql,
          ...(input.title ? { title: input.title } : {}),
          ...(input.tags ? { tags: input.tags } : {}),
        })
        return {
          data: {
            action,
            ok: true,
            detail: `已生成只读快照（${result.rowCount} 行${result.truncated ? '，达上限截断' : ''}）→ 知识库草稿（未启用）。请提醒用户到「做梦空间 → 个人知识库」审阅启用。`,
            path: result.path,
            targets: null,
            evolution: null,
          },
        }
      } catch (e) {
        return fail(`数据库进料失败：${errorMessage(e)}`)
      }
    }
    if (action === 'knowledge_sync_now') {
      const result = await runKnowledgeSyncNow()
      if (!result.ok) return fail(result.detail)
      const s = result.summary
      return {
        data: {
          action,
          ok: true,
          detail: `远程同步完成：推送 ${s.pushed}、墓碑 ${s.deleted}、未变 ${s.unchanged}（scopes=${s.scopes.join('+')} → ${s.endpoint}）。`,
          path: null,
          targets: null,
          evolution: null,
        },
      }
    }
    if (action === 'knowledge_draft') {
      const title = input.title?.trim()
      const content = input.content?.trim()
      if (!title || !content) {
        return {
          data: {
            action,
            ok: false,
            detail: 'knowledge_draft 需要 title 与 content 两个参数。',
            path: null,
            targets: null,
            evolution: null,
          },
        }
      }
      try {
        const dir = getKnowledgeDir()
        await mkdir(dir, { recursive: true })
        const body = renderDraft({
          title,
          ...(input.description ? { description: input.description } : {}),
          ...(input.tags ? { tags: input.tags } : {}),
          content,
        })
        // 防撞 + 防并发覆盖：`wx` 独占创建（存在即 EEXIST → 换序号重试），
        // 绝不覆盖任何既有条目/草稿（毁灭复核修正：existsSync+write 有
        // TOCTOU 窗口）。
        let target: string | null = null
        for (let index = 1; index <= 9; index++) {
          const fileName = draftFileName(
            index === 1 ? title : `${title}-${index}`,
          )
          const candidate = join(dir, fileName)
          try {
            await writeFile(candidate, body, { encoding: 'utf8', flag: 'wx' })
            target = candidate
            break
          } catch (e) {
            if ((e as NodeJS.ErrnoException).code !== 'EEXIST') throw e
          }
        }
        if (target === null) {
          return {
            data: {
              action,
              ok: false,
              detail: '同名草稿过多，换个标题再试。',
              path: null,
              targets: null,
              evolution: null,
            },
          }
        }
        return {
          data: {
            action,
            ok: true,
            detail:
              '草稿已写入个人知识库（未启用）。请提醒用户到「做梦空间 → 个人知识库」审阅并启用后才会参与记忆召回。',
            path: target,
            targets: null,
            evolution: null,
          },
        }
      } catch (e) {
        return {
          data: {
            action,
            ok: false,
            detail: `草稿写入失败：${errorMessage(e)}`,
            path: null,
            targets: null,
            evolution: null,
          },
        }
      }
    }

    // dream_now / watch_list 都要 orchestrator 桥。
    if (!memoryBridgeIpc.isAvailable()) {
      return { data: unavailable(action) }
    }
    if (action === 'watch_list') {
      try {
        const response = await memoryBridgeIpc.send(
          'memory.watch.list',
          {},
          { timeout_ms: WATCH_LIST_TIMEOUT_MS },
        )
        const targets = parseWatchTargets(response)
        return {
          data: {
            action,
            ok: true,
            detail:
              targets !== null && targets.length > 0
                ? `共 ${targets.length} 个专项检测目标。`
                : '尚未配置专项检测目标（可在做梦空间 → 专项检测里添加）。',
            path: null,
            targets,
            evolution: null,
          },
        }
      } catch (e) {
        logForDebugging(
          `[MemoryManageTool] memory.watch.list failed: ${errorMessage(e)}`,
          { level: 'warn' },
        )
        return { data: unavailable(action) }
      }
    }

    // action === 'dream_now'
    try {
      const sessionId = getSessionId()
      const response = (await memoryBridgeIpc.send(
        'memory.dream.run_now',
        {
          session_id: sessionId,
          current_session_id: sessionId,
          memory_dir: getAutoMemPath(),
          now_ms: Date.now(),
        },
        { timeout_ms: DREAM_RUN_NOW_TIMEOUT_MS },
      )) as {
        gate_skip_reason?: unknown
        dream_run?: { started?: unknown; skip_reason?: unknown }
      } | null
      const started = response?.dream_run?.started === true
      const skipReason =
        typeof response?.gate_skip_reason === 'string'
          ? response.gate_skip_reason
          : typeof response?.dream_run?.skip_reason === 'string'
            ? response.dream_run.skip_reason
            : null
      if (started) {
        return {
          data: {
            action,
            ok: true,
            detail:
              '做梦整理已在后台启动（多相 LLM 管线，完成后做梦空间会自动刷新并通知）。',
            path: null,
            targets: null,
            evolution: null,
          },
        }
      }
      const reasonText =
        skipReason === 'corpus_empty'
          ? '当前项目没有可整理的新会话语料（先聊几轮再试）。'
          : skipReason === 'lock_held'
            ? '已有一次整理正在进行。'
            : (skipReason ?? '未知原因');
      return {
        data: {
          action,
          ok: false,
          detail: `本次做梦被安全门跳过：${reasonText}`,
          path: null,
          targets: null,
          evolution: null,
        },
      }
    } catch (e) {
      logForDebugging(
        `[MemoryManageTool] memory.dream.run_now failed: ${errorMessage(e)}`,
        { level: 'warn' },
      )
      return { data: unavailable(action) }
    }
  },
  userFacingName() {
    return 'Manage memory'
  },
  renderToolUseMessage(input) {
    switch (input.action) {
      case 'dream_now':
        return 'Triggering a dream consolidation'
      case 'knowledge_draft':
        return `Drafting knowledge entry "${input.title ?? ''}"`
      case 'evolution_status':
        return 'Reading memory evolution status'
      case 'knowledge_list':
        return 'Listing knowledge-base entries'
      case 'knowledge_read':
        return `Reading knowledge entry ${input.path ? basename(input.path) : ''}`
      case 'knowledge_promote':
        return `Promoting "${input.title ?? ''}" into the knowledge base`
      case 'knowledge_ingest_url':
        return `Ingesting web page ${input.url ?? ''}`
      case 'knowledge_ingest_file':
        return `Importing file ${input.file_path ? basename(input.file_path) : ''}`
      case 'knowledge_ingest_db':
        return `Snapshotting SQLite ${input.db_path ? basename(input.db_path) : ''}`
      case 'knowledge_sync_now':
        return 'Pushing knowledge to the remote store'
      default:
        return 'Listing watch targets'
    }
  },
  renderToolResultMessage(output) {
    return output.detail
  },
  mapToolResultToToolResultBlockParam(content, toolUseID) {
    return {
      tool_use_id: toolUseID,
      type: 'tool_result',
      content: jsonStringify(content),
    }
  },
} satisfies ToolDef<InputSchema, Output>)
