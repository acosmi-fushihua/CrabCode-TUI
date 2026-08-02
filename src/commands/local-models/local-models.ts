import type { ToolUseContext } from '../../Tool.js'
import type { LocalCommandResult } from '../../types/command.js'
import {
  createDefaultLocalModelTuiClient,
  type LocalModelDirectClient as LocalModelTuiClient,
} from '../../services/localModel/directClient.js'

// /local-models is the direct TUI entry point over the shared local-model
// lifecycle authority.

const MANAGED_NOTE =
  'Use /local-models to inspect, register, install, or remove local models. Server lifecycle remains process-local to this CrabCode session.'

export async function call(
  args: string,
  _context: ToolUseContext,
): Promise<LocalCommandResult> {
  const parts = args.trim().length > 0 ? args.trim().split(/\s+/) : []
  const sub = (parts[0] ?? 'status').toLowerCase()
  const rest = parts.slice(1)
  const client = createDefaultLocalModelTuiClient()

  try {
    switch (sub) {
      case 'status':
        return text(await renderStatus(client))
      case 'install':
        return text(await renderInstall(client, rest[0]))
      case 'add':
        return text(await renderByoAdd(client, rest))
      case 'remove':
        return text(await renderRemove(client, rest[0]))
      case 'remove-byo':
        return text(await renderByoRemove(client, rest[0]))
      case 'help':
        return text(usage())
      default:
        return text(`Unknown subcommand: ${sub}\n\n${usage()}`)
    }
  } catch (err) {
    return text(
      `Local model command failed: ${errorMessage(err)}\n\n${MANAGED_NOTE}`,
    )
  }
}

function usage(): string {
  return [
    'Usage: /local-models [status|install <id>|add <path> [name]|remove <id>|remove-byo <id>]',
    '',
    '  status            Show device profile, inference server state, and catalog (default).',
    '  install <id>      Start downloading a curated local model by its catalog id.',
    '  add <path> [name] Register your own .gguf file (absolute path) as a local model.',
    '                    Optional display name (remaining words). Use /model local:<id> to select it.',
    '  remove <id>       Remove an installed curated local model and delete its files.',
    '  remove-byo <id>   Unregister a bring-your-own model added via "add" (your .gguf file is not deleted).',
    '  help              Show this help.',
    '',
    MANAGED_NOTE,
  ].join('\n')
}

async function renderStatus(client: LocalModelTuiClient): Promise<string> {
  const [catalog, profile, server] = await Promise.all([
    client.catalogRead(),
    client.systemProfileRead(),
    client.serverStatus(),
  ])

  const lines: string[] = ['Local models', '']

  lines.push(
    `Device: ${profile.platform}/${profile.arch}, memory ${formatBytes(profile.memoryBytes)}`,
  )
  lines.push(`Recommended runtime: ${profile.recommendedRuntime ?? 'none'}`)
  lines.push('')

  const s = server.status
  lines.push(`Inference server: ${s.state}${s.reason ? ` (${s.reason})` : ''}`)
  if (s.url) lines.push(`  endpoint: ${s.url}`)
  if (s.modelId) lines.push(`  model: ${s.modelId}`)
  if (s.error) lines.push(`  error: ${s.error}`)
  lines.push('')

  lines.push(
    `Catalog (${catalog.source}, manifest ${catalog.manifestStatus} v${formatNumber(catalog.manifestVersion)}):`,
  )
  if (catalog.data.length === 0) {
    lines.push('  No models in the curated catalog yet.')
  } else {
    for (const e of catalog.data) {
      lines.push(`  - ${e.id} — ${e.displayName}`)
      lines.push(
        `    runtime ${e.runtime}, format ${e.format}, ${formatBytes(e.sizeBytes)}, status ${e.status}, ${e.installed ? 'installed' : 'not installed'}`,
      )
      if (e.reason) lines.push(`    reason: ${e.reason}`)
    }
  }
  lines.push('')
  lines.push(MANAGED_NOTE)
  return lines.join('\n')
}

async function renderInstall(
  client: LocalModelTuiClient,
  modelId: string | undefined,
): Promise<string> {
  if (!modelId) return `Missing model id.\n\n${usage()}`

  // Only curated catalog ids may be installed from the TUI — no arbitrary URL
  // or path downloads. Validate against the curated catalog first.
  const catalog = await client.catalogRead()
  const curated = catalog.data.filter(e => e.source === 'curated')
  const entry = curated.find(e => e.id === modelId)
  if (!entry) {
    const lines: string[] = [`"${modelId}" is not a curated local model.`]
    if (curated.length === 0) {
      lines.push(
        'The curated catalog is currently empty — no models can be installed yet.',
      )
    } else {
      lines.push('Curated models:')
      for (const e of curated) lines.push(`  - ${e.id}`)
    }
    lines.push('')
    lines.push(MANAGED_NOTE)
    return lines.join('\n')
  }

  const res = await client.downloadStart({ modelId })
  const d = res.status
  const lines: string[] = [`Download for ${modelId}: ${d.state}`]
  if (isPresent(d.percentage)) {
    lines.push(`  progress: ${formatPercent(d.percentage)}`)
  }
  if (isPresent(d.bytesReceived)) {
    const total = isPresent(d.totalBytes)
      ? ` / ${formatBytes(d.totalBytes)}`
      : ''
    lines.push(`  received: ${formatBytes(d.bytesReceived)}${total}`)
  }
  if (d.reason) lines.push(`  reason: ${d.reason}`)
  if (d.error) lines.push(`  error: ${d.error}`)
  lines.push('')
  lines.push(MANAGED_NOTE)
  return lines.join('\n')
}

async function renderRemove(
  client: LocalModelTuiClient,
  modelId: string | undefined,
): Promise<string> {
  if (!modelId) return `Missing model id.\n\n${usage()}`

  const res = await client.installRemove({ modelId, removeFiles: true })
  const lines: string[] = [`Remove ${res.modelId ?? modelId}: ${res.state}`]
  if (res.reason) lines.push(`  reason: ${res.reason}`)
  lines.push('')
  lines.push(MANAGED_NOTE)
  return lines.join('\n')
}

export async function renderByoAdd(
  client: LocalModelTuiClient,
  rest: string[],
): Promise<string> {
  const ggufPath = rest[0]
  if (!ggufPath) {
    return `Missing path to a .gguf file.\n\nUsage: /local-models add <path> [display name]\n\n${MANAGED_NOTE}`
  }

  // Everything after the path is an optional human-friendly display name.
  const displayNameRaw = rest.slice(1).join(' ').trim()
  const displayName = displayNameRaw.length > 0 ? displayNameRaw : undefined

  // The shared authority validates the path (absolute, readable, regular
  // `.gguf`). Forward it unchanged so validation stays single-source.
  const res = await client.byoAdd({ ggufPath, displayName })
  const entry = res.entry
  const name = entry.displayName || entry.id
  const lines: string[] = [
    `Added local model ${entry.id} (${name}).`,
    `Select it with /model local:${entry.id}, or run /local-models status to view it.`,
  ]
  if (entry.modelPath) lines.push(`  file: ${entry.modelPath}`)
  lines.push(`  size: ${formatBytes(entry.sizeBytes)}`)
  lines.push('')
  lines.push(MANAGED_NOTE)
  return lines.join('\n')
}

export async function renderByoRemove(
  client: LocalModelTuiClient,
  id: string | undefined,
): Promise<string> {
  if (!id) {
    return `Missing model id.\n\nUsage: /local-models remove-byo <id>\n\n${MANAGED_NOTE}`
  }

  // BYO removal only unregisters the catalog entry; it never deletes the user's
  // own .gguf file (that file lives outside the managed store). This is distinct
  // from "remove", which deletes the downloaded files of a curated model.
  const res = await client.byoRemove({ id })
  const lines: string[] = res.removed
    ? [
        `Removed local model ${id} from the catalog.`,
        '(Your .gguf file on disk was not deleted.)',
      ]
    : [`No bring-your-own model with id "${id}" was found — nothing removed.`]
  lines.push('')
  lines.push(MANAGED_NOTE)
  return lines.join('\n')
}

function text(value: string): LocalCommandResult {
  return { type: 'text', value }
}

function isPresent<T>(value: T | null | undefined): value is T {
  return value !== null && value !== undefined
}

function formatNumber(value: number | null | undefined): string {
  if (!isPresent(value) || !Number.isFinite(value)) return 'unknown'
  return String(value)
}

function formatBytes(value: bigint | number | null | undefined): string {
  if (!isPresent(value)) return 'unknown size'
  const n = typeof value === 'bigint' ? Number(value) : value
  if (!Number.isFinite(n) || n < 0) return 'unknown size'
  if (n < 1024) return `${Math.round(n)} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let v = n / 1024
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i += 1
  }
  return `${v.toFixed(1)} ${units[i]}`
}

function formatPercent(value: number | null | undefined): string {
  if (!isPresent(value) || !Number.isFinite(value)) return 'unknown'
  const clamped = Math.max(0, Math.min(100, value))
  return `${clamped.toFixed(1)}%`
}

function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message
  if (typeof err === 'string') return err
  try {
    return JSON.stringify(err)
  } catch {
    return String(err)
  }
}
