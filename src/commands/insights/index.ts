import { execFileSync } from 'child_process'
import { constants as fsConstants } from 'fs'
import {
  copyFile,
  mkdir,
  mkdtemp,
  readdir,
  rm,
  writeFile,
} from 'fs/promises'
import { tmpdir } from 'os'
import { join } from 'path'
import type { Command } from '../../commands.js'
import type { LogOption } from '../../types/logs.js'
import { getCrabCodeConfigHomeDir } from '../../utils/envUtils.js'
import { resolveMacroVersion } from '../../utils/macroVersion.js'
import {
  getProjectsDir,
  getSessionFilesWithMtime,
  loadAllLogsFromSessionFile,
} from '../../utils/sessionStorage.js'
import { jsonParse, jsonStringify } from '../../utils/slowOperations.js'
import { localExecBridge } from 'src/runtime/localProcess.js'
import {
  extractFacetsFromAPI,
  loadCachedFacets,
  loadCachedSessionMeta,
  saveFacets,
  saveSessionMeta,
} from './facets.js'
import { generateHtmlReport } from './htmlReport.js'
import { generateParallelInsights } from './insightGeneration.js'
import {
  aggregateData,
  hasValidDates,
  logToSessionMeta,
} from './sessionProcessing.js'
import type {
  AggregatedData,
  InsightResults,
  InsightsExport,
  LiteSessionInfo,
  RemoteHostInfo,
  SessionFacets,
  SessionMeta,
} from './types.js'
import { getDataDir, getFacetsDir, safeEntries, safeKeys } from './types.js'
import { t } from '../../i18n/index.js'

// Re-export public API
export { deduplicateSessionBranches, detectMultiClauding } from './sessionProcessing.js'
export type { InsightsExport } from './types.js'

// ============================================================================
// Homespace Data Collection
// ============================================================================

/* eslint-disable custom-rules/no-process-env-top-level */
const getRunningRemoteHosts: () => Promise<string[]> =
  process.env.USER_TYPE === 'ant'
    ? async () => {
        const { stdout, code } = await localExecBridge.execCommand({
          command: 'coder',
          args: ['list', '-o', 'json'],
          timeout: 30000,
        })
        if (code !== 0) return []
        try {
          const workspaces = jsonParse(stdout) as Array<{
            name: string
            latest_build?: { status?: string }
          }>
          return workspaces
            .filter(w => w.latest_build?.status === 'running')
            .map(w => w.name)
        } catch {
          return []
        }
      }
    : async () => []

const getRemoteHostSessionCount: (hs: string) => Promise<number> =
  process.env.USER_TYPE === 'ant'
    ? async (homespace: string) => {
        const { stdout, code } = await localExecBridge.execCommand({
          command: 'ssh',
          args: [
            `${homespace}.coder`,
            'find /root/.crabcode/projects -name "*.jsonl" 2>/dev/null | wc -l',
          ],
          timeout: 30000,
        })
        if (code !== 0) return 0
        return parseInt(stdout.trim(), 10) || 0
      }
    : async () => 0

const collectFromRemoteHost: (
  hs: string,
  destDir: string,
) => Promise<{ copied: number; skipped: number }> =
  process.env.USER_TYPE === 'ant'
    ? async (homespace: string, destDir: string) => {
        const result = { copied: 0, skipped: 0 }

        // Create temp directory
        const tempDir = await mkdtemp(join(tmpdir(), 'crabcode-hs-'))

        try {
          // SCP the projects folder
          const scpResult = await localExecBridge.execCommand({
            command: 'scp',
            args: ['-rq', `${homespace}.coder:/root/.crabcode/projects/`, tempDir],
            timeout: 300000,
          })
          if (scpResult.code !== 0) {
            // SCP failed
            return result
          }

          const projectsDir = join(tempDir, 'projects')
          let projectDirents: import('fs').Dirent<string>[]
          try {
            projectDirents = await readdir(projectsDir, { withFileTypes: true }) as import('fs').Dirent<string>[]
          } catch {
            return result
          }

          // Merge into destination (parallel per project directory)
          await Promise.all(
            projectDirents.map(async dirent => {
              const projectName = dirent.name
              const projectPath = join(projectsDir, projectName)

              // Skip if not a directory
              if (!dirent.isDirectory()) return

              const destProjectName = `${projectName}__${homespace}`
              const destProjectPath = join(destDir, destProjectName)

              try {
                await mkdir(destProjectPath, { recursive: true })
              } catch {
                // Directory may already exist
              }

              // Copy session files (skip existing)
              let files: import('fs').Dirent<string>[]
              try {
                files = await readdir(projectPath, { withFileTypes: true }) as import('fs').Dirent<string>[]
              } catch {
                return
              }
              await Promise.all(
                files.map(async fileDirent => {
                  const fileName = fileDirent.name
                  if (!fileName.endsWith('.jsonl')) return

                  const srcFile = join(projectPath, fileName)
                  const destFile = join(destProjectPath, fileName)

                  try {
                    await copyFile(srcFile, destFile, fsConstants.COPYFILE_EXCL)
                    result.copied++
                  } catch {
                    // EEXIST from COPYFILE_EXCL means dest already exists
                    result.skipped++
                  }
                }),
              )
            }),
          )
        } finally {
          try {
            await rm(tempDir, { recursive: true, force: true })
          } catch {
            // Ignore cleanup errors
          }
        }

        return result
      }
    : async () => ({ copied: 0, skipped: 0 })

const collectAllRemoteHostData: (destDir: string) => Promise<{
  hosts: RemoteHostInfo[]
  totalCopied: number
  totalSkipped: number
}> =
  process.env.USER_TYPE === 'ant'
    ? async (destDir: string) => {
        const rHosts = await getRunningRemoteHosts()
        const result: RemoteHostInfo[] = []
        let totalCopied = 0
        let totalSkipped = 0

        // Collect from all hosts in parallel (SCP per host can take seconds)
        const hostResults = await Promise.all(
          rHosts.map(async hs => {
            const sessionCount = await getRemoteHostSessionCount(hs)
            if (sessionCount > 0) {
              const { copied, skipped } = await collectFromRemoteHost(
                hs,
                destDir,
              )
              return { name: hs, sessionCount, copied, skipped }
            }
            return { name: hs, sessionCount, copied: 0, skipped: 0 }
          }),
        )

        for (const hr of hostResults) {
          result.push({ name: hr.name, sessionCount: hr.sessionCount })
          totalCopied += hr.copied
          totalSkipped += hr.skipped
        }

        return { hosts: result, totalCopied, totalSkipped }
      }
    : async () => ({ hosts: [], totalCopied: 0, totalSkipped: 0 })
/* eslint-enable custom-rules/no-process-env-top-level */

// ============================================================================
// Lite Session Scanning
// ============================================================================

/**
 * Scans all project directories using filesystem metadata only (no JSONL parsing).
 * Returns a list of session file info sorted by mtime descending.
 * Yields to the event loop between project directories to keep the UI responsive.
 */
async function scanAllSessions(): Promise<LiteSessionInfo[]> {
  const projectsDir = getProjectsDir()

  let dirents: import('fs').Dirent<string>[]
  try {
    dirents = await readdir(projectsDir, { withFileTypes: true }) as import('fs').Dirent<string>[]
  } catch {
    return []
  }

  const projectDirs = dirents
    .filter(dirent => dirent.isDirectory())
    .map(dirent => join(projectsDir, dirent.name))

  const allSessions: LiteSessionInfo[] = []

  for (let i = 0; i < projectDirs.length; i++) {
    const sessionFiles = await getSessionFilesWithMtime(projectDirs[i]!)
    for (const [sessionId, fileInfo] of sessionFiles) {
      allSessions.push({
        sessionId,
        path: fileInfo.path,
        mtime: fileInfo.mtime,
        size: fileInfo.size,
      })
    }
    // Yield to event loop every 10 project directories
    if (i % 10 === 9) {
      await new Promise<void>(resolve => setImmediate(resolve))
    }
  }

  // Sort by mtime descending (most recent first)
  allSessions.sort((a, b) => b.mtime - a.mtime)
  return allSessions
}

// ============================================================================
// Export Data Builder
// ============================================================================

/**
 * Build export data from already-computed values.
 * Used by background upload to S3.
 */
export function buildExportData(
  data: AggregatedData,
  insights: InsightResults,
  facets: Map<string, SessionFacets>,
  remoteStats?: { hosts: RemoteHostInfo[]; totalCopied: number },
): InsightsExport {
  // RC-1c (E10): direct MACRO.VERSION via shared helper — the old typeof guard
  // resolved to 'unknown' in bundled builds. See macroVersion.ts.
  const version = resolveMacroVersion()

  const remote_hosts_collected = remoteStats?.hosts
    .filter(h => h.sessionCount > 0)
    .map(h => h.name)

  const facets_summary = {
    total: facets.size,
    goal_categories: {} as Record<string, number>,
    outcomes: {} as Record<string, number>,
    satisfaction: {} as Record<string, number>,
    friction: {} as Record<string, number>,
  }
  for (const f of facets.values()) {
    for (const [cat, count] of safeEntries(f.goal_categories)) {
      if (count > 0) {
        facets_summary.goal_categories[cat] =
          (facets_summary.goal_categories[cat] || 0) + count
      }
    }
    facets_summary.outcomes[f.outcome] =
      (facets_summary.outcomes[f.outcome] || 0) + 1
    for (const [level, count] of safeEntries(f.user_satisfaction_counts)) {
      if (count > 0) {
        facets_summary.satisfaction[level] =
          (facets_summary.satisfaction[level] || 0) + count
      }
    }
    for (const [type, count] of safeEntries(f.friction_counts)) {
      if (count > 0) {
        facets_summary.friction[type] =
          (facets_summary.friction[type] || 0) + count
      }
    }
  }

  return {
    metadata: {
      username: process.env.SAFEUSER || process.env.USER || 'unknown',
      generated_at: new Date().toISOString(),
      crab_code_version: version,
      date_range: data.date_range,
      session_count: data.total_sessions,
      ...(remote_hosts_collected &&
        remote_hosts_collected.length > 0 && {
          remote_hosts_collected,
        }),
    },
    aggregated_data: data,
    insights,
    facets_summary,
  }
}

// ============================================================================
// Main Function
// ============================================================================

export async function generateUsageReport(options?: {
  collectRemote?: boolean
}): Promise<{
  insights: InsightResults
  htmlPath: string
  data: AggregatedData
  remoteStats?: { hosts: RemoteHostInfo[]; totalCopied: number }
  facets: Map<string, SessionFacets>
}> {
  let remoteStats: { hosts: RemoteHostInfo[]; totalCopied: number } | undefined

  // Optionally collect data from remote hosts first (ant-only)
  if (process.env.USER_TYPE === 'ant' && options?.collectRemote) {
    const destDir = join(getCrabCodeConfigHomeDir(), 'projects')
    const { hosts, totalCopied } = await collectAllRemoteHostData(destDir)
    remoteStats = { hosts, totalCopied }
  }

  // Phase 1: Lite scan — filesystem metadata only (no JSONL parsing)
  const allScannedSessions = await scanAllSessions()
  const totalSessionsScanned = allScannedSessions.length

  // Phase 2: Load SessionMeta — use cache where available, parse only uncached
  // Read cached metas in parallel batches to avoid blocking the event loop
  const META_BATCH_SIZE = 50
  const MAX_SESSIONS_TO_LOAD = 200
  let allMetas: SessionMeta[] = []
  const uncachedSessions: LiteSessionInfo[] = []

  for (let i = 0; i < allScannedSessions.length; i += META_BATCH_SIZE) {
    const batch = allScannedSessions.slice(i, i + META_BATCH_SIZE)
    const results = await Promise.all(
      batch.map(async sessionInfo => ({
        sessionInfo,
        cached: await loadCachedSessionMeta(sessionInfo.sessionId),
      })),
    )
    for (const { sessionInfo, cached } of results) {
      if (cached) {
        allMetas.push(cached)
      } else if (uncachedSessions.length < MAX_SESSIONS_TO_LOAD) {
        uncachedSessions.push(sessionInfo)
      }
    }
  }

  // Load full message data only for uncached sessions and compute SessionMeta
  const logsForFacets = new Map<string, LogOption>()

  // Filter out /insights meta-sessions (facet extraction API calls get logged as sessions)
  const isMetaSession = (log: LogOption): boolean => {
    for (const msg of log.messages.slice(0, 5)) {
      if (msg.type === 'user' && msg.message) {
        const content = msg.message.content
        if (typeof content === 'string') {
          if (
            content.includes('RESPOND WITH ONLY A VALID JSON OBJECT') ||
            content.includes('record_facets')
          ) {
            return true
          }
        }
      }
    }
    return false
  }

  // Load uncached sessions in batches to yield to event loop between batches
  const LOAD_BATCH_SIZE = 10
  for (let i = 0; i < uncachedSessions.length; i += LOAD_BATCH_SIZE) {
    const batch = uncachedSessions.slice(i, i + LOAD_BATCH_SIZE)
    const batchResults = await Promise.all(
      batch.map(async sessionInfo => {
        try {
          return await loadAllLogsFromSessionFile(sessionInfo.path)
        } catch {
          return []
        }
      }),
    )
    // Collect metas synchronously, then save them in parallel (independent writes)
    const metasToSave: SessionMeta[] = []
    for (const logs of batchResults) {
      for (const log of logs) {
        if (isMetaSession(log) || !hasValidDates(log)) continue
        const meta = logToSessionMeta(log)
        allMetas.push(meta)
        metasToSave.push(meta)
        // Keep the log around for potential facet extraction
        logsForFacets.set(meta.session_id, log)
      }
    }
    await Promise.all(metasToSave.map(meta => saveSessionMeta(meta)))
  }

  // Deduplicate session branches (keep the one with most user messages per session_id)
  // This prevents inflated totals when a session has multiple conversation branches
  const bestBySession = new Map<string, SessionMeta>()
  for (const meta of allMetas) {
    const existing = bestBySession.get(meta.session_id)
    if (
      !existing ||
      meta.user_message_count > existing.user_message_count ||
      (meta.user_message_count === existing.user_message_count &&
        meta.duration_minutes > existing.duration_minutes)
    ) {
      bestBySession.set(meta.session_id, meta)
    }
  }
  // Replace allMetas with deduplicated list and remove unused logs from logsForFacets
  const keptSessionIds = new Set(bestBySession.keys())
  allMetas = [...bestBySession.values()]
  for (const sessionId of logsForFacets.keys()) {
    if (!keptSessionIds.has(sessionId)) {
      logsForFacets.delete(sessionId)
    }
  }

  // Sort all metas by start_time descending (most recent first)
  allMetas.sort((a, b) => b.start_time.localeCompare(a.start_time))

  // Pre-filter obviously minimal sessions to save API calls
  // (matching Python's substantive filtering concept)
  const isSubstantiveSession = (meta: SessionMeta): boolean => {
    // Skip sessions with very few user messages
    if (meta.user_message_count < 2) return false
    // Skip very short sessions (< 1 minute)
    if (meta.duration_minutes < 1) return false
    return true
  }

  const substantiveMetas = allMetas.filter(isSubstantiveSession)

  // Phase 3: Facet extraction — only for sessions without cached facets
  const facets = new Map<string, SessionFacets>()
  const toExtract: Array<{ log: LogOption; sessionId: string }> = []
  const MAX_FACET_EXTRACTIONS = 50

  // Load cached facets for all substantive sessions in parallel
  const cachedFacetResults = await Promise.all(
    substantiveMetas.map(async meta => ({
      sessionId: meta.session_id,
      cached: await loadCachedFacets(meta.session_id),
    })),
  )
  for (const { sessionId, cached } of cachedFacetResults) {
    if (cached) {
      facets.set(sessionId, cached)
    } else {
      const log = logsForFacets.get(sessionId)
      if (log && toExtract.length < MAX_FACET_EXTRACTIONS) {
        toExtract.push({ log, sessionId })
      }
    }
  }

  // Extract facets for sessions that need them (50 concurrent)
  const CONCURRENCY = 50
  for (let i = 0; i < toExtract.length; i += CONCURRENCY) {
    const batch = toExtract.slice(i, i + CONCURRENCY)
    const results = await Promise.all(
      batch.map(async ({ log, sessionId }) => {
        const newFacets = await extractFacetsFromAPI(log, sessionId)
        return { sessionId, newFacets }
      }),
    )
    // Collect facets synchronously, save in parallel (independent writes)
    const facetsToSave: SessionFacets[] = []
    for (const { sessionId, newFacets } of results) {
      if (newFacets) {
        facets.set(sessionId, newFacets)
        facetsToSave.push(newFacets)
      }
    }
    await Promise.all(facetsToSave.map(f => saveFacets(f)))
  }

  // Filter out warmup/minimal sessions (matching Python's is_minimal)
  // A session is minimal if warmup_minimal is the ONLY goal category
  const isMinimalSession = (sessionId: string): boolean => {
    const sessionFacets = facets.get(sessionId)
    if (!sessionFacets) return false
    const cats = sessionFacets.goal_categories
    const catKeys = safeKeys(cats).filter(k => (cats[k] ?? 0) > 0)
    return catKeys.length === 1 && catKeys[0] === 'warmup_minimal'
  }

  const substantiveSessions = substantiveMetas.filter(
    s => !isMinimalSession(s.session_id),
  )

  const substantiveFacets = new Map<string, SessionFacets>()
  for (const [sessionId, f] of facets) {
    if (!isMinimalSession(sessionId)) {
      substantiveFacets.set(sessionId, f)
    }
  }

  const aggregated = aggregateData(substantiveSessions, substantiveFacets)
  aggregated.total_sessions_scanned = totalSessionsScanned

  // Generate parallel insights from CrabCode (6 sections)
  const insights = await generateParallelInsights(aggregated, facets)

  // Generate HTML report
  const htmlReport = generateHtmlReport(aggregated, insights)

  // Save reports
  try {
    await mkdir(getDataDir(), { recursive: true })
  } catch {
    // Directory may already exist
  }

  const htmlPath = join(getDataDir(), 'report.html')
  await writeFile(htmlPath, htmlReport, {
    encoding: 'utf-8',
    mode: 0o600,
  })

  return {
    insights,
    htmlPath,
    data: aggregated,
    remoteStats,
    facets: substantiveFacets,
  }
}

// ============================================================================
// Command Definition
// ============================================================================

// NOTE: execFileSync (line 1 import) is a synchronous blocking call used only
// for the ant-only S3 upload below. Legacy — not in scope for async migration.
const usageReport: Command = {
  type: 'prompt',
  name: 'insights',
  get description() {
    return t('cmd_insights_desc')
  },
  contentLength: 0, // Dynamic content
  get progressMessage() {
    return t('cmd_insights_progress')
  },
  source: 'builtin',
  async getPromptForCommand(args) {
    let collectRemote = false
    let remoteHosts: string[] = []
    let hasRemoteHosts = false

    if (process.env.USER_TYPE === 'ant') {
      // Parse --homespaces flag
      collectRemote = args?.includes('--homespaces') ?? false

      // Check for available remote hosts
      remoteHosts = await getRunningRemoteHosts()
      hasRemoteHosts = remoteHosts.length > 0

      // Show collection message if collecting
      if (collectRemote && hasRemoteHosts) {
        console.error(
          `Collecting sessions from ${remoteHosts.length} homespace(s): ${remoteHosts.join(', ')}...`,
        )
      }
    }

    const { insights, htmlPath, data, remoteStats } = await generateUsageReport(
      { collectRemote },
    )

    let reportUrl = `file://${htmlPath}`
    let uploadHint = ''

    if (process.env.USER_TYPE === 'ant') {
      // Try to upload to S3
      const timestamp = new Date()
        .toISOString()
        .replace(/[-:]/g, '')
        .replace('T', '_')
        .slice(0, 15)
      const username = process.env.SAFEUSER || process.env.USER || 'unknown'
      const filename = `${username}_insights_${timestamp}.html`
      const s3Path = `s3://acosmi-data/atamkin/cc-user-reports/${filename}`
      const s3Url = `https://s3-frontend.infra.ant.dev/acosmi-data/atamkin/cc-user-reports/${filename}`

      reportUrl = s3Url
      try {
        execFileSync('ff', ['cp', htmlPath, s3Path], {
          timeout: 60000,
          stdio: 'pipe', // Suppress output
        })
      } catch {
        // Upload failed - fall back to local file and show upload command
        reportUrl = `file://${htmlPath}`
        uploadHint = `\nAutomatic upload failed. Are you on the boron namespace? Try \`use-bo\` and ensure you've run \`sso\`.
To share, run: ff cp ${htmlPath} ${s3Path}
Then access at: ${s3Url}`
      }
    }

    // Build header with stats
    const sessionLabel =
      data.total_sessions_scanned &&
      data.total_sessions_scanned > data.total_sessions
        ? `${data.total_sessions_scanned.toLocaleString()} sessions total · ${data.total_sessions} analyzed`
        : `${data.total_sessions} sessions`
    const stats = [
      sessionLabel,
      `${data.total_messages.toLocaleString()} messages`,
      `${Math.round(data.total_duration_hours)}h`,
      `${data.git_commits} commits`,
    ].join(' · ')

    // Build remote host info (ant-only)
    let remoteInfo = ''
    if (process.env.USER_TYPE === 'ant') {
      if (remoteStats && remoteStats.totalCopied > 0) {
        const hsNames = remoteStats.hosts
          .filter(h => h.sessionCount > 0)
          .map(h => h.name)
          .join(', ')
        remoteInfo = `\n_Collected ${remoteStats.totalCopied} new sessions from: ${hsNames}_\n`
      } else if (!collectRemote && hasRemoteHosts) {
        // Suggest using --homespaces if they have remote hosts but didn't use the flag
        remoteInfo = `\n_Tip: Run \`/insights --homespaces\` to include sessions from your ${remoteHosts.length} running homespace(s)_\n`
      }
    }

    // Build markdown summary from insights
    const atAGlance = insights.at_a_glance
    const summaryText = atAGlance
      ? `## At a Glance

${atAGlance.whats_working ? `**What's working:** ${atAGlance.whats_working} See _Impressive Things You Did_.` : ''}

${atAGlance.whats_hindering ? `**What's hindering you:** ${atAGlance.whats_hindering} See _Where Things Go Wrong_.` : ''}

${atAGlance.quick_wins ? `**Quick wins to try:** ${atAGlance.quick_wins} See _Features to Try_.` : ''}

${atAGlance.ambitious_workflows ? `**Ambitious workflows:** ${atAGlance.ambitious_workflows} See _On the Horizon_.` : ''}`
      : '_No insights generated_'

    const header = `# CrabCode Insights

${stats}
${data.date_range.start} to ${data.date_range.end}
${remoteInfo}
`

    const userSummary = `${header}${summaryText}

Your full shareable insights report is ready: ${reportUrl}${uploadHint}`

    // Return prompt for CrabCode to respond to
    return [
      {
        type: 'text',
        text: `The user just ran /insights to generate a usage report analyzing their CrabCode sessions.

Here is the full insights data:
${jsonStringify(insights, null, 2)}

Report URL: ${reportUrl}
HTML file: ${htmlPath}
Facets directory: ${getFacetsDir()}

Here is what the user sees:
${userSummary}

Now output the following message exactly:

<message>
Your shareable insights report is ready:
${reportUrl}${uploadHint}

Want to dig into any section or try one of the suggestions?
</message>`,
      },
    ]
  },
}

export default usageReport
