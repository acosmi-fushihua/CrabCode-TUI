import memoize from 'lodash-es/memoize.js'
import { homedir } from 'os'
import { isAbsolute, join, normalize, sep } from 'path'
import {
  getIsNonInteractiveSession,
  getProjectRoot,
  getSessionOverrideProjectRootCwd,
} from '../bootstrap/state.js'
import { getFeatureValue_CACHED_MAY_BE_STALE } from '../services/analytics/growthbook.js'
import {
  getCrabCodeConfigHomeDir,
  isEnvDefinedFalsy,
  isEnvTruthy,
} from '../utils/envUtils.js'
import { findCanonicalGitRoot } from '../utils/git.js'
import { sanitizePath } from '../utils/path.js'
import {
  getInitialSettings,
  getSettingsForSource,
} from '../utils/settings/settings.js'

/**
 * Whether auto-memory features are enabled (memdir, agent memory, past session search).
 * Enabled by default. Priority chain (first defined wins):
 *   1. CRABCODE_DISABLE_AUTO_MEMORY env var (1/true → OFF, 0/false → ON)
 *   2. CRABCODE_SIMPLE (--bare) → OFF
 *   3. CCR without persistent storage → OFF (no CRABCODE_REMOTE_MEMORY_DIR)
 *   4. autoMemoryEnabled in settings.json (supports project-level opt-out)
 *   5. Default: enabled
 */
export function isAutoMemoryEnabled(): boolean {
  const envVal = process.env.CRABCODE_DISABLE_AUTO_MEMORY
  if (isEnvTruthy(envVal)) {
    return false
  }
  if (isEnvDefinedFalsy(envVal)) {
    return true
  }
  // --bare / SIMPLE: prompts.ts already drops the memory section from the
  // system prompt via its SIMPLE early-return; this gate stops the other half
  // (extractMemories turn-end fork, autoDream, /remember, /dream, team sync).
  if (isEnvTruthy(process.env.CRABCODE_SIMPLE)) {
    return false
  }
  if (
    isEnvTruthy(process.env.CRABCODE_REMOTE) &&
    !process.env.CRABCODE_REMOTE_MEMORY_DIR
  ) {
    return false
  }
  const settings = getInitialSettings()
  if (settings.autoMemoryEnabled !== undefined) {
    return settings.autoMemoryEnabled
  }
  return true
}

/**
 * Whether the extract-memories background agent will run this session.
 *
 * The main agent's prompt always has full save instructions regardless of
 * this gate — when the main agent writes memories, the background agent
 * skips that range (hasMemoryWritesSince in extractMemories.ts); when it
 * doesn't, the background agent catches anything missed.
 *
 * Callers must also gate on feature('EXTRACT_MEMORIES') — that check cannot
 * live inside this helper because feature() only tree-shakes when used
 * directly in an `if` condition.
 */
export function isExtractModeActive(): boolean {
  // Local settings are authoritative when set — the GrowthBook flag is
  // remote-eval and never true for builds the gateway doesn't serve, so the
  // dream-space enable flow writes `extractMemoriesEnabled` instead of
  // depending on the gateway (W-MEMORY-SELF-EVOLUTION A1). The
  // non-interactive guard stays: headless/print sessions never run the
  // background extraction agent regardless of the toggle.
  const setting = getInitialSettings().extractMemoriesEnabled
  if (setting !== undefined) {
    return setting && !getIsNonInteractiveSession()
  }
  // W-MEMORY-ALIVE 裁决① (2026-07-01): default ON — an absent gateway flag no
  // longer disables Tier-2 extraction (only an explicit local setting or an
  // explicit gateway false does).
  if (!getFeatureValue_CACHED_MAY_BE_STALE('tengu_passport_quail', true)) {
    return false
  }
  return (
    !getIsNonInteractiveSession() ||
    getFeatureValue_CACHED_MAY_BE_STALE('tengu_slate_thimble', false)
  )
}

/**
 * Returns the base directory for persistent memory storage.
 * Resolution order:
 *   1. CRABCODE_REMOTE_MEMORY_DIR env var (explicit override, set in CCR)
 *   2. ~/.crabcode (default config home)
 */
export function getMemoryBaseDir(): string {
  if (process.env.CRABCODE_REMOTE_MEMORY_DIR) {
    return process.env.CRABCODE_REMOTE_MEMORY_DIR
  }
  return getCrabCodeConfigHomeDir()
}

const AUTO_MEM_DIRNAME = 'memory'
const AUTO_MEM_ENTRYPOINT_NAME = 'MEMORY.md'
const LEGACY_RUST_DERIVED_DIRNAME = '.rust-derived'
const GLOBAL_MEM_STATE_DIRNAME = '.global-memory-state'
const KNOWLEDGE_DIRNAME = 'knowledge'
const KNOWLEDGE_STATE_DIRNAME = '.knowledge-state'

/**
 * User-global memory root: `<base>/memory/` where base is resolved by
 * getMemoryBaseDir() (CRABCODE_REMOTE_MEMORY_DIR > CRABCODE_CONFIG_DIR >
 * CRABCODE_HOME/.crabcode > ~/.crabcode). Unlike the per-project
 * getAutoMemPath() this is NOT keyed on a project slug — memories promoted
 * here are visible in every session regardless of the anchored project
 * (W-MEMORY-LIFECYCLE K4).
 */
export function getGlobalMemDir(): string {
  return join(getMemoryBaseDir(), AUTO_MEM_DIRNAME)
}

/** Entrypoint of the user-global memory root (`<base>/memory/MEMORY.md`). */
export function getGlobalMemEntrypoint(): string {
  return join(getGlobalMemDir(), AUTO_MEM_ENTRYPOINT_NAME)
}

/**
 * SE state dir for the user-global memory root (`<base>/.global-memory-state`).
 * Sibling of the root, never inside it — mirrors the per-project
 * `.memory-rust-derived` isolation so index state is not scanned as memory.
 */
export function getGlobalMemStateDir(): string {
  return join(getMemoryBaseDir(), GLOBAL_MEM_STATE_DIRNAME)
}

/**
 * Personal knowledge base root (`<base>/knowledge/`) — user-curated markdown
 * entries surfaced through memory.search `scopes:['knowledge']`
 * (W-MEMORY-LIFECYCLE K9).
 */
export function getKnowledgeDir(): string {
  return join(getMemoryBaseDir(), KNOWLEDGE_DIRNAME)
}

/** SE state dir for the knowledge base root (`<base>/.knowledge-state`). */
export function getKnowledgeStateDir(): string {
  return join(getMemoryBaseDir(), KNOWLEDGE_STATE_DIRNAME)
}

/**
 * Normalize and validate a candidate auto-memory directory path.
 *
 * SECURITY: Rejects paths that would be dangerous as a read-allowlist root
 * or that normalize() doesn't fully resolve:
 * - relative (!isAbsolute): "../foo" — would be interpreted relative to CWD
 * - root/near-root (length < 3): "/" → "" after strip; "/a" too short
 * - Windows drive-root (C: regex): "C:\" → "C:" after strip
 * - UNC paths (\\server\share): network paths — opaque trust boundary
 * - null byte: survives normalize(), can truncate in syscalls
 *
 * Returns the normalized path with exactly one trailing separator,
 * or undefined if the path is unset/empty/rejected.
 */
function validateMemoryPath(
  raw: string | undefined,
  expandTilde: boolean,
): string | undefined {
  if (!raw) {
    return undefined
  }
  let candidate = raw
  // Settings.json paths support ~/ expansion (user-friendly). The env var
  // override does not (it's set programmatically by Cowork/SDK, which should
  // always pass absolute paths). Bare "~", "~/", "~/.", "~/..", etc. are NOT
  // expanded — they would make isAutoMemPath() match all of $HOME or its
  // parent (same class of danger as "/" or "C:\").
  if (
    expandTilde &&
    (candidate.startsWith('~/') || candidate.startsWith('~\\'))
  ) {
    const rest = candidate.slice(2)
    // Reject trivial remainders that would expand to $HOME or an ancestor.
    // normalize('') = '.', normalize('.') = '.', normalize('foo/..') = '.',
    // normalize('..') = '..', normalize('foo/../..') = '..'
    const restNorm = normalize(rest || '.')
    if (restNorm === '.' || restNorm === '..') {
      return undefined
    }
    candidate = join(homedir(), rest)
  }
  // normalize() may preserve a trailing separator; strip before adding
  // exactly one to match the trailing-sep contract of getAutoMemPath()
  const normalized = normalize(candidate).replace(/[/\\]+$/, '')
  if (
    !isAbsolute(normalized) ||
    normalized.length < 3 ||
    /^[A-Za-z]:$/.test(normalized) ||
    normalized.startsWith('\\\\') ||
    normalized.startsWith('//') ||
    normalized.includes('\0')
  ) {
    return undefined
  }
  return (normalized + sep).normalize('NFC')
}

/**
 * Direct override for the full auto-memory directory path via env var.
 * When set, getAutoMemPath()/getAutoMemEntrypoint() return this path directly
 * instead of computing `{base}/projects/{sanitized-cwd}/memory/`.
 *
 * Used by Cowork to redirect memory to a space-scoped mount where the
 * per-session cwd (which contains the VM process name) would otherwise
 * produce a different project-key for every session.
 */
function getAutoMemPathOverride(): string | undefined {
  return validateMemoryPath(
    process.env.CRABCODE_COWORK_MEMORY_PATH_OVERRIDE,
    false,
  )
}

/**
 * Settings.json override for the full auto-memory directory path.
 * Supports ~/ expansion for user convenience.
 *
 * SECURITY: projectSettings (.crabcode/settings.json committed to the repo) is
 * intentionally excluded — a malicious repo could otherwise set
 * autoMemoryDirectory: "~/.ssh" and gain silent write access to sensitive
 * directories via the filesystem.ts write carve-out (which fires when
 * isAutoMemPath() matches and hasAutoMemPathOverride() is false). This follows
 * the same pattern as hasSkipDangerousModePermissionPrompt() etc.
 */
function getAutoMemPathSetting(): string | undefined {
  const dir =
    getSettingsForSource('policySettings')?.autoMemoryDirectory ??
    getSettingsForSource('flagSettings')?.autoMemoryDirectory ??
    getSettingsForSource('localSettings')?.autoMemoryDirectory ??
    getSettingsForSource('userSettings')?.autoMemoryDirectory
  return validateMemoryPath(dir, true)
}

/**
 * Check if CRABCODE_COWORK_MEMORY_PATH_OVERRIDE is set to a valid override.
 * Use this as a signal that the SDK caller has explicitly opted into
 * the auto-memory mechanics — e.g. to decide whether to inject the
 * memory prompt when a custom system prompt replaces the default.
 */
export function hasAutoMemPathOverride(): boolean {
  return getAutoMemPathOverride() !== undefined
}

/**
 * The project root that memory paths anchor to. A per-turn workspace override
 * wins over the process-level project root.
 */
function getEffectiveMemoryProjectRoot(): string {
  return getSessionOverrideProjectRootCwd() ?? getProjectRoot()
}

/**
 * Returns the canonical git repo root if available, otherwise falls back to
 * the stable project root. Uses findCanonicalGitRoot so all worktrees of the
 * same repo share one auto-memory directory (acosmi/crabcode#24382).
 */
function getAutoMemBase(): string {
  const root = getEffectiveMemoryProjectRoot()
  return findCanonicalGitRoot(root) ?? root
}

/**
 * Returns the auto-memory directory path.
 *
 * Resolution order:
 *   1. CRABCODE_COWORK_MEMORY_PATH_OVERRIDE env var (full-path override, used by Cowork)
 *   2. autoMemoryDirectory in settings.json (trusted sources only: policy/local/user)
 *   3. <memoryBase>/projects/<sanitized-git-root>/memory/
 *      where memoryBase is resolved by getMemoryBaseDir()
 *
 * Memoized: render-path callers (collapseReadSearchGroups → isAutoManagedMemoryFile)
 * fire per tool-use message per Messages re-render; each miss costs
 * getSettingsForSource × 4 → parseSettingsFile (realpathSync + readFileSync).
 * Keyed on the effective memory project root so each TUI workspace resolves
 * its own memory directory and tests that change the project root recompute.
 */
export const getAutoMemPath = memoize(
  (): string => {
    const override = getAutoMemPathOverride() ?? getAutoMemPathSetting()
    if (override) {
      return override
    }
    const projectsDir = join(getMemoryBaseDir(), 'projects')
    return (
      join(projectsDir, sanitizePath(getAutoMemBase()), AUTO_MEM_DIRNAME) + sep
    ).normalize('NFC')
  },
  () => getEffectiveMemoryProjectRoot(),
)

/**
 * Returns the daily log file path for the given date (defaults to today).
 * Shape: <autoMemPath>/logs/YYYY/MM/YYYY-MM-DD.md
 *
 * Used by assistant mode (feature('KAIROS')): rather than maintaining
 * MEMORY.md as a live index, the agent appends to a date-named log file
 * as it works. W-MEMORY-DREAM-REBUILD v7 Tier-3 AutoDream (Rust
 * orchestrator policy in `libs/acosmi-memory/acosmi-memory-orchestrator/
 * src/tier/dream.rs`) distills these logs into topic files + MEMORY.md.
 * 历史 TS 端 nightly `/dream` skill 已废弃（v7 之前的 stale 注释）—
 * Tier 调度走 orchestrator，不在 TS skill 体系内。
 */
export function getAutoMemDailyLogPath(date: Date = new Date()): string {
  const yyyy = date.getFullYear().toString()
  const mm = (date.getMonth() + 1).toString().padStart(2, '0')
  const dd = date.getDate().toString().padStart(2, '0')
  return join(getAutoMemPath(), 'logs', yyyy, mm, `${yyyy}-${mm}-${dd}.md`)
}

/**
 * Returns the auto-memory entrypoint (MEMORY.md inside the auto-memory dir).
 * Follows the same resolution order as getAutoMemPath().
 */
export function getAutoMemEntrypoint(): string {
  return join(getAutoMemPath(), AUTO_MEM_ENTRYPOINT_NAME)
}

/**
 * Legacy derived data root from pre-v6 plans.
 *
 * v6 keeps this path recognizable only so readers/scanners can skip, drain, or
 * migrate old files. New Rust-derived writes must use the sibling
 * .memory-rust-derived root instead.
 */
export function isLegacyRustDerivedMemoryPath(absolutePath: string): boolean {
  const normalizedPath = normalize(absolutePath)
  const legacyRoot = normalize(
    join(getAutoMemPath(), LEGACY_RUST_DERIVED_DIRNAME),
  )
  return (
    normalizedPath === legacyRoot ||
    normalizedPath.startsWith(legacyRoot + sep)
  )
}

/**
 * Check if an absolute path is within the auto-memory directory.
 *
 * When CRABCODE_COWORK_MEMORY_PATH_OVERRIDE is set, this matches against the
 * env-var override directory. Note that a true return here does NOT imply
 * write permission in that case — the filesystem.ts write carve-out is gated
 * on !hasAutoMemPathOverride() (it exists to bypass DANGEROUS_DIRECTORIES).
 *
 * The settings.json autoMemoryDirectory DOES get the write carve-out: it's the
 * user's explicit choice from a trusted settings source (projectSettings is
 * excluded — see getAutoMemPathSetting), and hasAutoMemPathOverride() remains
 * false for it.
 */
export function isAutoMemPath(absolutePath: string): boolean {
  // SECURITY: Normalize to prevent path traversal bypasses via .. segments
  const normalizedPath = normalize(absolutePath)
  return (
    normalizedPath.startsWith(getAutoMemPath()) &&
    !isLegacyRustDerivedMemoryPath(normalizedPath)
  )
}
