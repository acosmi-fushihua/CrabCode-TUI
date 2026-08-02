/**
 * Sandbox adapter layer — TS-side wrapper around the Rust acosmi-sandbox runtime.
 *
 * The retired `@acosmi-ai/sandbox-runtime`
 * package 整体退场；types 转入 `./types.ts` 内联，BaseSandboxManager 调用面
 * 全部退场（commit 2 已切 settings 派生 + shape adapter）。Rust 真后端经
 * ExecBridge sandboxMode='enforced' 链路调度 (Shell.ts:349-362 useEnforcedRouting →
 * ExecBridge.spawnManaged → acosmi-supervisor::ipc → AsyncSandboxRunner)。
 */

import type {
  FsReadRestrictionConfig,
  FsWriteRestrictionConfig,
  IgnoreViolationsConfig,
  NetworkHostPattern,
  NetworkRestrictionConfig,
  SandboxAskCallback,
  SandboxDependencyCheck,
  SandboxRuntimeConfig,
  SandboxViolationEvent,
  SandboxViolationStore,
} from './types.js'
import {
  SandboxRuntimeUnavailableError,
  type SandboxRuntimeKind,
} from './errors.js'
import { rmSync, statSync } from 'fs'
import { readFile } from 'fs/promises'
import { memoize } from 'lodash-es'
import { join, resolve, sep } from 'path'
import {
  getAdditionalDirectoriesForCrabcodeMd,
  getOriginalCwd,
} from '../../bootstrap/state.js'
// A20 verification gate — sandbox config is rebuilt PER worker turn (BashTool
// runs inside the agent loop). Read the ALS-aware `getCwd()` so the deny-write
// / worktree-detect paths bind to THIS turn's cwd, not the racing global
// `STATE.cwd` clobbered by overlapping cross-thread turns. In the TUI (no
// override installed) `getCwd()` resolves to the same global — byte-identical.
import { getCwd } from '../cwd.js'
import { logForDebugging } from '../debug.js'
import { expandPath } from '../path.js'
import { getPlatform, type Platform } from '../platform.js'
import { settingsChangeDetector } from '../settings/changeDetector.js'
import { SETTING_SOURCES, type SettingSource } from '../settings/constants.js'
import { getManagedSettingsDropInDir } from '../settings/managedPath.js'
import {
  getInitialSettings,
  getSettings_DEPRECATED,
  getSettingsFilePathForSource,
  getSettingsForSource,
  getSettingsRootPathForSource,
  updateSettingsForSource,
} from '../settings/settings.js'
import type { SettingsJson } from '../settings/types.js'

// ============================================================================
// Settings Converter
// ============================================================================

import { BASH_TOOL_NAME } from 'src/tools/BashTool/toolName.js'
import { FILE_EDIT_TOOL_NAME } from 'src/tools/FileEditTool/constants.js'
import { FILE_READ_TOOL_NAME } from 'src/tools/FileReadTool/prompt.js'
import { WEB_FETCH_TOOL_NAME } from 'src/tools/WebFetchTool/prompt.js'
import { errorMessage } from '../errors.js'
import { getCrabCodeTempDir } from '../permissions/filesystem.js'
import type { PermissionRuleValue } from '../permissions/PermissionRule.js'
import { ripgrepCommand } from '../ripgrep.js'

/**
 * Stub violation store used by `getSandboxViolationStore()` in path B.
 *
 * Caller surface: `SandboxPromptFooterHint.tsx:19` + `SandboxViolationExpandedView.tsx:36`.
 * - `subscribe(listener)` → returns no-op unsubscribe
 * - `getTotalCount()` → 0
 *
 * Phase 5 acosmi-protocol 接通后由 Rust 端经 IPC 推 violation 事件，本 store
 * 被替换为真实现。R-2 退化已在 audit doc + CHANGELOG 标注。
 */
const STUB_VIOLATION_STORE: SandboxViolationStore = {
  subscribe: () => () => {},
  getTotalCount: () => 0,
}

// Local copies to avoid circular dependency
// (permissions.ts imports SandboxManager, bashPermissions.ts imports permissions.ts)
function permissionRuleValueFromString(
  ruleString: string,
): PermissionRuleValue {
  const matches = ruleString.match(/^([^(]+)\(([^)]+)\)$/)
  if (!matches) {
    return { toolName: ruleString }
  }
  const toolName = matches[1]
  const ruleContent = matches[2]
  if (!toolName || !ruleContent) {
    return { toolName: ruleString }
  }
  return { toolName, ruleContent }
}

function permissionRuleExtractPrefix(permissionRule: string): string | null {
  const match = permissionRule.match(/^(.+):\*$/)
  return match?.[1] ?? null
}

/**
 * Resolve CrabCode-specific path patterns for sandbox-runtime.
 *
 * CrabCode uses special path prefixes in permission rules:
 * - `//path` → absolute from filesystem root (becomes `/path`)
 * - `/path` → relative to settings file directory (becomes `$SETTINGS_DIR/path`)
 * - `~/path` → passed through (sandbox-runtime handles this)
 * - `./path` or `path` → passed through (sandbox-runtime handles this)
 *
 * This function only handles CC-specific conventions (`//` and `/`).
 * Standard path patterns like `~/` and relative paths are passed through
 * for sandbox-runtime's normalizePathForSandbox to handle.
 *
 * @param pattern The path pattern from a permission rule
 * @param source The settings source this pattern came from (needed to resolve `/path` patterns)
 */
export function resolvePathPatternForSandbox(
  pattern: string,
  source: SettingSource,
): string {
  // Handle // prefix - absolute from root (CC-specific convention)
  if (pattern.startsWith('//')) {
    return pattern.slice(1) // "//.aws/**" → "/.aws/**"
  }

  // Handle / prefix - relative to settings file directory (CC-specific convention)
  // Note: ~/path and relative paths are passed through for sandbox-runtime to handle
  if (pattern.startsWith('/') && !pattern.startsWith('//')) {
    const root = getSettingsRootPathForSource(source)
    // Pattern like "/foo/**" becomes "${root}/foo/**"
    return resolve(root, pattern.slice(1))
  }

  // Other patterns (~/path, ./path, path) pass through as-is
  // sandbox-runtime's normalizePathForSandbox will handle them
  return pattern
}

/**
 * Resolve paths from sandbox.filesystem.* settings (allowWrite, denyWrite, etc).
 *
 * Unlike permission rules (Edit/Read), these settings use standard path semantics:
 * - `/path` → absolute path (as written, NOT settings-relative)
 * - `~/path` → expanded to home directory
 * - `./path` or `path` → relative to settings file directory
 * - `//path` → absolute (legacy permission-rule syntax, accepted for compat)
 *
 * Fix for #30067: resolvePathPatternForSandbox treats `/Users/foo/.cargo` as
 * settings-relative (permission-rule convention). Users reasonably expect
 * absolute paths in sandbox.filesystem.allowWrite to work as-is.
 *
 * Also expands `~` here rather than relying on sandbox-runtime, because
 * sandbox-runtime's getFsWriteConfig() does not call normalizePathForSandbox
 * on allowWrite paths (it only strips trailing glob suffixes).
 */
export function resolveSandboxFilesystemPath(
  pattern: string,
  source: SettingSource,
): string {
  // Legacy permission-rule escape: //path → /path. Kept for compat with
  // users who worked around #30067 by writing //Users/foo/.cargo in config.
  if (pattern.startsWith('//')) return pattern.slice(1)
  return expandPath(pattern, getSettingsRootPathForSource(source))
}

/**
 * Check if only managed sandbox domains should be used.
 * This is true when policySettings has sandbox.network.allowManagedDomainsOnly: true
 */
export function shouldAllowManagedSandboxDomainsOnly(): boolean {
  return (
    getSettingsForSource('policySettings')?.sandbox?.network
      ?.allowManagedDomainsOnly === true
  )
}

function shouldAllowManagedReadPathsOnly(): boolean {
  return (
    getSettingsForSource('policySettings')?.sandbox?.filesystem
      ?.allowManagedReadPathsOnly === true
  )
}

/**
 * Convert CrabCode settings format to SandboxRuntimeConfig format
 * (Function exported for testing)
 *
 * @param settings Merged settings (used for sandbox config like network, ripgrep, etc.)
 */
export function convertToSandboxRuntimeConfig(
  settings: SettingsJson,
): SandboxRuntimeConfig {
  const permissions = settings.permissions || {}

  // Extract network domains from WebFetch rules
  const allowedDomains: string[] = []
  const deniedDomains: string[] = []

  // When allowManagedSandboxDomainsOnly is enabled, only use domains from policy settings
  if (shouldAllowManagedSandboxDomainsOnly()) {
    const policySettings = getSettingsForSource('policySettings')
    for (const domain of policySettings?.sandbox?.network?.allowedDomains ||
      []) {
      allowedDomains.push(domain)
    }
    for (const ruleString of policySettings?.permissions?.allow || []) {
      const rule = permissionRuleValueFromString(ruleString)
      if (
        rule.toolName === WEB_FETCH_TOOL_NAME &&
        rule.ruleContent?.startsWith('domain:')
      ) {
        allowedDomains.push(rule.ruleContent.substring('domain:'.length))
      }
    }
  } else {
    for (const domain of settings.sandbox?.network?.allowedDomains || []) {
      allowedDomains.push(domain)
    }
    for (const ruleString of permissions.allow || []) {
      const rule = permissionRuleValueFromString(ruleString)
      if (
        rule.toolName === WEB_FETCH_TOOL_NAME &&
        rule.ruleContent?.startsWith('domain:')
      ) {
        allowedDomains.push(rule.ruleContent.substring('domain:'.length))
      }
    }
  }

  for (const ruleString of permissions.deny || []) {
    const rule = permissionRuleValueFromString(ruleString)
    if (
      rule.toolName === WEB_FETCH_TOOL_NAME &&
      rule.ruleContent?.startsWith('domain:')
    ) {
      deniedDomains.push(rule.ruleContent.substring('domain:'.length))
    }
  }

  // Extract filesystem paths from Edit and Read rules
  // Always include current directory and CrabCode temp directory as writable
  // The temp directory is needed for Shell.ts cwd tracking files
  const allowWrite: string[] = ['.', getCrabCodeTempDir()]
  const denyWrite: string[] = []
  const denyRead: string[] = []
  const allowRead: string[] = []

  // Always deny writes to settings.json files to prevent sandbox escape
  // This blocks settings in the original working directory (where CrabCode started)
  const settingsPaths = SETTING_SOURCES.map(source =>
    getSettingsFilePathForSource(source),
  ).filter((p): p is string => p !== undefined)
  denyWrite.push(...settingsPaths)
  denyWrite.push(getManagedSettingsDropInDir())

  // Also block settings files in the current working directory if it differs from original
  // This handles the case where the user has cd'd to a different directory
  const cwd = getCwd()
  const originalCwd = getOriginalCwd()
  if (cwd !== originalCwd) {
    denyWrite.push(resolve(cwd, '.crabcode', 'settings.json'))
    denyWrite.push(resolve(cwd, '.crabcode', 'settings.local.json'))
  }

  // Block writes to .crabcode/skills in both original and current working directories.
  // The sandbox-runtime's getDangerousDirectories() protects .crabcode/commands and
  // .crabcode/agents but not .crabcode/skills. Skills have the same privilege level
  // (auto-discovered, auto-loaded, full CrabCode capabilities) so they need the
  // same OS-level sandbox protection.
  denyWrite.push(resolve(originalCwd, '.crabcode', 'skills'))
  if (cwd !== originalCwd) {
    denyWrite.push(resolve(cwd, '.crabcode', 'skills'))
  }

  // SECURITY: Git's is_git_directory() treats cwd as a bare repo if it has
  // HEAD + objects/ + refs/. An attacker planting these (plus a config with
  // core.fsmonitor) escapes the sandbox when CrabCode's unsandboxed git runs.
  //
  // Unconditionally denying these paths makes sandbox-runtime mount
  // /dev/null at non-existent ones, which (a) leaves a 0-byte HEAD stub on
  // the host and (b) breaks `git log HEAD` inside bwrap ("ambiguous argument").
  // So: if a file exists, denyWrite (ro-bind in place, no stub). If not, scrub
  // it post-command in scrubBareGitRepoFiles() — planted files are gone before
  // unsandboxed git runs; inside the command, git is itself sandboxed.
  bareGitRepoScrubPaths.length = 0
  const bareGitRepoFiles = ['HEAD', 'objects', 'refs', 'hooks', 'config']
  for (const dir of cwd === originalCwd ? [originalCwd] : [originalCwd, cwd]) {
    for (const gitFile of bareGitRepoFiles) {
      const p = resolve(dir, gitFile)
      try {
        // eslint-disable-next-line custom-rules/no-sync-fs -- refreshConfig() must be sync
        statSync(p)
        denyWrite.push(p)
      } catch {
        bareGitRepoScrubPaths.push(p)
      }
    }
  }

  // If we detected a git worktree during initialize(), the main repo path is
  // cached in worktreeMainRepoPath. Git operations in a worktree need write
  // access to the main repo's .git directory for index.lock etc.
  // This is resolved once at init time (worktree status doesn't change mid-session).
  if (worktreeMainRepoPath && worktreeMainRepoPath !== cwd) {
    allowWrite.push(worktreeMainRepoPath)
  }

  // Include directories added via --add-dir CLI flag or /add-dir command.
  // These must be in allowWrite so that Bash commands (which run inside the
  // sandbox) can access them — not just file tools, which check permissions
  // at the app level via pathInAllowedWorkingPath().
  // Two sources: persisted in settings, and session-only in bootstrap state.
  const additionalDirs = new Set([
    ...(settings.permissions?.additionalDirectories || []),
    ...getAdditionalDirectoriesForCrabcodeMd(),
  ])
  allowWrite.push(...additionalDirs)

  // Iterate through each settings source to resolve paths correctly
  // Path patterns like `/foo` are relative to the settings file directory,
  // so we need to know which source each rule came from
  for (const source of SETTING_SOURCES) {
    const sourceSettings = getSettingsForSource(source)

    // Extract filesystem paths from permission rules
    if (sourceSettings?.permissions) {
      for (const ruleString of sourceSettings.permissions.allow || []) {
        const rule = permissionRuleValueFromString(ruleString)
        if (rule.toolName === FILE_EDIT_TOOL_NAME && rule.ruleContent) {
          allowWrite.push(
            resolvePathPatternForSandbox(rule.ruleContent, source),
          )
        }
      }

      for (const ruleString of sourceSettings.permissions.deny || []) {
        const rule = permissionRuleValueFromString(ruleString)
        if (rule.toolName === FILE_EDIT_TOOL_NAME && rule.ruleContent) {
          denyWrite.push(resolvePathPatternForSandbox(rule.ruleContent, source))
        }
        if (rule.toolName === FILE_READ_TOOL_NAME && rule.ruleContent) {
          denyRead.push(resolvePathPatternForSandbox(rule.ruleContent, source))
        }
      }
    }

    // Extract filesystem paths from sandbox.filesystem settings
    // sandbox.filesystem.* uses standard path semantics (/path = absolute),
    // NOT the permission-rule convention (/path = settings-relative). #30067
    const fs = sourceSettings?.sandbox?.filesystem
    if (fs) {
      for (const p of fs.allowWrite || []) {
        allowWrite.push(resolveSandboxFilesystemPath(p, source))
      }
      for (const p of fs.denyWrite || []) {
        denyWrite.push(resolveSandboxFilesystemPath(p, source))
      }
      for (const p of fs.denyRead || []) {
        denyRead.push(resolveSandboxFilesystemPath(p, source))
      }
      if (!shouldAllowManagedReadPathsOnly() || source === 'policySettings') {
        for (const p of fs.allowRead || []) {
          allowRead.push(resolveSandboxFilesystemPath(p, source))
        }
      }
    }
  }
  // Ripgrep config for sandbox. User settings take priority; otherwise pass our rg.
  // In embedded mode (argv0='rg' dispatch), sandbox-runtime spawns with argv0 set.
  const { rgPath, rgArgs, argv0 } = ripgrepCommand()
  const ripgrepConfig = settings.sandbox?.ripgrep ?? {
    command: rgPath,
    args: rgArgs,
    argv0,
  }

  return {
    network: {
      allowedDomains,
      deniedDomains,
      allowUnixSockets: settings.sandbox?.network?.allowUnixSockets,
      allowAllUnixSockets: settings.sandbox?.network?.allowAllUnixSockets,
      allowLocalBinding: settings.sandbox?.network?.allowLocalBinding,
      httpProxyPort: settings.sandbox?.network?.httpProxyPort,
      socksProxyPort: settings.sandbox?.network?.socksProxyPort,
    },
    filesystem: {
      denyRead,
      allowRead,
      allowWrite,
      denyWrite,
    },
    ignoreViolations: settings.sandbox?.ignoreViolations,
    enableWeakerNestedSandbox: settings.sandbox?.enableWeakerNestedSandbox,
    enableWeakerNetworkIsolation:
      settings.sandbox?.enableWeakerNetworkIsolation,
    ripgrep: ripgrepConfig,
  }
}

// ============================================================================
// BaseSandboxManager getter shape adapters (R-Sandbox Phase 4 wave 2 path B)
// 实现移到 ./configAdapters.ts 以让单测独立 import；本文件 re-export 以保留
// 内部 caller 通过 sandbox-adapter 访问的简洁路径。
// ============================================================================

import {
  deriveFsReadConfig,
  deriveFsWriteConfig,
  deriveNetworkRestrictionConfig,
} from './configAdapters.js'
export {
  deriveFsReadConfig,
  deriveFsWriteConfig,
  deriveNetworkRestrictionConfig,
}

// ============================================================================
// CrabCode CLI-specific state
// ============================================================================

let initializationPromise: Promise<void> | undefined
let settingsSubscriptionCleanup: (() => void) | undefined

// Cached main repo path for git worktrees, resolved once during initialize().
// In a worktree, .git is a file containing "gitdir: /path/to/main/repo/.git/worktrees/name".
// undefined = not yet resolved; null = not a worktree or detection failed.
let worktreeMainRepoPath: string | null | undefined

// Bare-repo files at cwd that didn't exist at config time and should be
// scrubbed if they appear after a sandboxed command. See acosmi/crabcode#29316.
const bareGitRepoScrubPaths: string[] = []

/**
 * Delete bare-repo files planted at cwd during a sandboxed command, before
 * CrabCode's unsandboxed git calls can see them. See the SECURITY block above
 * bareGitRepoFiles. acosmi/crabcode#29316.
 */
function scrubBareGitRepoFiles(): void {
  for (const p of bareGitRepoScrubPaths) {
    try {
      // eslint-disable-next-line custom-rules/no-sync-fs -- cleanupAfterCommand must be sync (Shell.ts:367)
      rmSync(p, { recursive: true })
      logForDebugging(`[Sandbox] scrubbed planted bare-repo file: ${p}`)
    } catch {
      // ENOENT is the expected common case — nothing was planted
    }
  }
}

/**
 * Detect if cwd is a git worktree and resolve the main repo path.
 * Called once during initialize() and cached for the session.
 * In a worktree, .git is a file (not a directory) containing "gitdir: ...".
 * If .git is a directory, readFile throws EISDIR and we return null.
 */
async function detectWorktreeMainRepoPath(cwd: string): Promise<string | null> {
  const gitPath = join(cwd, '.git')
  try {
    const gitContent = await readFile(gitPath, { encoding: 'utf8' })
    const gitdirMatch = gitContent.match(/^gitdir:\s*(.+)$/m)
    if (!gitdirMatch?.[1]) {
      return null
    }
    // gitdir may be relative (rare, but git accepts it) — resolve against cwd
    const gitdir = resolve(cwd, gitdirMatch[1].trim())
    // gitdir format: /path/to/main/repo/.git/worktrees/worktree-name
    // Match the /.git/worktrees/ segment specifically — indexOf('.git') alone
    // would false-match paths like /home/user/.github-projects/...
    const marker = `${sep}.git${sep}worktrees${sep}`
    const markerIndex = gitdir.lastIndexOf(marker)
    if (markerIndex > 0) {
      return gitdir.substring(0, markerIndex)
    }
    return null
  } catch {
    // Not in a worktree, .git is a directory (EISDIR), or can't read .git file
    return null
  }
}

/**
 * Check if dependencies are available (memoized)
 * Returns { errors, warnings } - errors mean sandbox cannot run
 *
 * R-Sandbox Phase 4 wave 2 path B：BaseSandboxManager 退场，依赖探测下沉到
 * Rust acosmi-sandbox 启动期 (`acosmi-sandbox::lib::select_runner` 走
 * MacosAsyncRunner / LinuxAsyncRunner / WindowsRunner，缺 vendor 时 supervisor
 * 返 SandboxRuntimeUnavailable，TS 经 ExecBridge 收回 + warn UI)。TS 端不再
 * 探测 bwrap / landlock-userspace / ripgrep；release packaging supplies them.
 */
const checkDependencies = memoize((): SandboxDependencyCheck => {
  return { errors: [], warnings: [] }
})

function getSandboxEnabledSetting(): boolean {
  try {
    const settings = getSettings_DEPRECATED()
    return settings?.sandbox?.enabled ?? false
  } catch (error) {
    logForDebugging(`Failed to get settings for sandbox check: ${error}`)
    return false
  }
}

function isAutoAllowBashIfSandboxedEnabled(): boolean {
  const settings = getSettings_DEPRECATED()
  return settings?.sandbox?.autoAllowBashIfSandboxed ?? true
}

function areUnsandboxedCommandsAllowed(): boolean {
  const settings = getSettings_DEPRECATED()
  return settings?.sandbox?.allowUnsandboxedCommands ?? true
}

function isSandboxRequired(): boolean {
  const settings = getSettings_DEPRECATED()
  return (
    getSandboxEnabledSetting() &&
    (settings?.sandbox?.failIfUnavailable ?? false)
  )
}

/**
 * Check if the current platform is supported for sandboxing (memoized)
 * Supports: macOS, Linux, and Windows (Phase 3 P3-T2 ✅ released)
 *
 * R-Sandbox Phase 4 wave 2 path B：原走 BaseSandboxManager.isSupportedPlatform()
 * 经 stub fallback；BaseSandboxManager 退场后由 TS 端直判 platform。真实平台
 * 能力探测在 Rust acosmi-sandbox 启动期完成，本函数仅作前置粗筛。
 */
const isSupportedPlatform = memoize((): boolean => {
  const p = getPlatform()
  return p === 'macos' || p === 'linux' || p === 'windows' || p === 'wsl'
})

/**
 * Check if the current platform is in the enabledPlatforms list.
 *
 * This is an undocumented setting that allows restricting sandbox to specific platforms.
 * When enabledPlatforms is not set, all supported platforms are allowed.
 *
 * Added to unblock NVIDIA enterprise rollout: they want to enable autoAllowBashIfSandboxed
 * but only on macOS initially, since Linux/WSL sandbox support is newer. This allows
 * setting enabledPlatforms: ["macos"] to disable sandbox (and auto-allow) on other platforms.
 */
function isPlatformInEnabledList(): boolean {
  try {
    const settings = getInitialSettings()
    const enabledPlatforms = (
      settings?.sandbox as { enabledPlatforms?: Platform[] } | undefined
    )?.enabledPlatforms

    if (enabledPlatforms === undefined) {
      return true
    }

    if (enabledPlatforms.length === 0) {
      return false
    }

    const currentPlatform = getPlatform()
    return enabledPlatforms.includes(currentPlatform)
  } catch (error) {
    logForDebugging(`Failed to check enabledPlatforms: ${error}`)
    return true // Default to enabled if we can't read settings
  }
}

/**
 * Check if sandboxing is enabled
 * This checks the user's enabled setting, platform support, and enabledPlatforms restriction
 */
function isSandboxingEnabled(): boolean {
  if (!isSupportedPlatform()) {
    return false
  }

  if (checkDependencies().errors.length > 0) {
    return false
  }

  // Check if current platform is in the enabledPlatforms list (undocumented setting)
  if (!isPlatformInEnabledList()) {
    return false
  }

  return getSandboxEnabledSetting()
}

/**
 * If the user explicitly enabled sandbox (sandbox.enabled: true in settings)
 * but it cannot actually run, return a human-readable reason. Otherwise
 * return undefined.
 *
 * Fix for #34044: previously isSandboxingEnabled() silently returned false
 * when dependencies were missing, giving users zero feedback that their
 * explicit security setting was being ignored. This is a security footgun —
 * users configure allowedDomains expecting enforcement, get none.
 *
 * Call this once at startup (REPL/print) and surface the reason if present.
 * Does not cover the case where the user never enabled sandbox (no noise).
 */
function getSandboxUnavailableReason(): string | undefined {
  // Only warn if user explicitly asked for sandbox. If they didn't enable
  // it, missing deps are irrelevant.
  if (!getSandboxEnabledSetting()) {
    return undefined
  }

  if (!isSupportedPlatform()) {
    const platform = getPlatform()
    if (platform === 'wsl') {
      return 'sandbox.enabled is set but WSL1 is not supported (requires WSL2)'
    }
    return `sandbox.enabled is set but ${platform} is not supported (requires macOS, Linux, or WSL2)`
  }

  if (!isPlatformInEnabledList()) {
    return `sandbox.enabled is set but ${getPlatform()} is not in sandbox.enabledPlatforms`
  }

  const deps = checkDependencies()
  if (deps.errors.length > 0) {
    const platform = getPlatform()
    const hint =
      platform === 'macos'
        ? 'run /sandbox or /doctor for details'
        : 'install missing tools (e.g. apt install bubblewrap socat) or run /sandbox for details'
    return `sandbox.enabled is set but dependencies are missing: ${deps.errors.join(', ')} · ${hint}`
  }

  return undefined
}

/**
 * Get glob patterns that won't work fully on Linux/WSL
 */
function getLinuxGlobPatternWarnings(): string[] {
  // Only return warnings on Linux/WSL (bubblewrap doesn't support globs)
  const platform = getPlatform()
  if (platform !== 'linux' && platform !== 'wsl') {
    return []
  }

  try {
    const settings = getSettings_DEPRECATED()

    // Only return warnings when sandboxing is enabled (check settings directly, not cached value)
    if (!settings?.sandbox?.enabled) {
      return []
    }

    const permissions = settings?.permissions || {}
    const warnings: string[] = []

    // Helper to check if a path has glob characters (excluding trailing /**)
    const hasGlobs = (path: string): boolean => {
      const stripped = path.replace(/\/\*\*$/, '')
      return /[*?[\]]/.test(stripped)
    }

    // Check all permission rules
    for (const ruleString of [
      ...(permissions.allow || []),
      ...(permissions.deny || []),
    ]) {
      const rule = permissionRuleValueFromString(ruleString)
      if (
        (rule.toolName === FILE_EDIT_TOOL_NAME ||
          rule.toolName === FILE_READ_TOOL_NAME) &&
        rule.ruleContent &&
        hasGlobs(rule.ruleContent)
      ) {
        warnings.push(ruleString)
      }
    }

    return warnings
  } catch (error) {
    logForDebugging(`Failed to get Linux glob pattern warnings: ${error}`)
    return []
  }
}

/**
 * Check if sandbox settings are locked by policy
 */
function areSandboxSettingsLockedByPolicy(): boolean {
  // Check if sandbox settings are explicitly set in any source that overrides localSettings
  // These sources have higher priority than localSettings and would make local changes ineffective
  const overridingSources = ['flagSettings', 'policySettings'] as const

  for (const source of overridingSources) {
    const settings = getSettingsForSource(source)
    if (
      settings?.sandbox?.enabled !== undefined ||
      settings?.sandbox?.autoAllowBashIfSandboxed !== undefined ||
      settings?.sandbox?.allowUnsandboxedCommands !== undefined
    ) {
      return true
    }
  }

  return false
}

/**
 * Set sandbox settings
 */
async function setSandboxSettings(options: {
  enabled?: boolean
  autoAllowBashIfSandboxed?: boolean
  allowUnsandboxedCommands?: boolean
}): Promise<void> {
  const existingSettings = getSettingsForSource('localSettings')

  // Note: Memoized caches auto-invalidate when settings change because they use
  // the settings object as the cache key (new settings object = cache miss)

  updateSettingsForSource('localSettings', {
    sandbox: {
      ...existingSettings?.sandbox,
      ...(options.enabled !== undefined && { enabled: options.enabled }),
      ...(options.autoAllowBashIfSandboxed !== undefined && {
        autoAllowBashIfSandboxed: options.autoAllowBashIfSandboxed,
      }),
      ...(options.allowUnsandboxedCommands !== undefined && {
        allowUnsandboxedCommands: options.allowUnsandboxedCommands,
      }),
    },
  })
}

/**
 * Get excluded commands (commands that should not be sandboxed)
 */
function getExcludedCommands(): string[] {
  const settings = getSettings_DEPRECATED()
  return settings?.sandbox?.excludedCommands ?? []
}

/**
 * Wrap command with sandbox — fail-loud guard for enforced routing edge cases.
 *
 * R-Sandbox Phase 4 wave 3 (P4-T6 final cleanup, 2026-05-06)：保留语义重写消息
 * 文案。本函数无真后端可调（wave 2 已删 BaseSandboxManager / stub package）；
 * 沙箱真隔离走 `Shell.ts` enforced routing → ExecBridge IPC → Rust AsyncSandboxRunner
 * (`Shell.ts:349-362 useEnforcedRouting` 命中即早返)。
 *
 * 触发场景（`shouldUseSandbox=true` AND `useEnforcedRouting=false`，详
 * `computeUseEnforcedRouting` 5 条件）：
 * 1. `isExecBridgeFallback=true` — 循环防御 sentinel（spawnManaged catch 兜底回路）
 * 2. `shellType==='powershell'` — PowerShell sandbox macOS/Linux 暂未接通（R5 独立门）
 * 3. `!isIpcAvailable` — Rust supervisor 离线（异常环境）
 *
 * 三场景下 user 期望真隔离但 enforced 路径不可达 → 抛 `SandboxRuntimeUnavailableError`
 * 给清晰诊断；不静默降级走 host 直跑（安全 footgun）。
 *
 * 详 `src/utils/sandbox/errors.ts` + 主真源 doc wave 3 实施计划。
 */
// eslint-disable-next-line @typescript-eslint/no-unused-vars
async function wrapWithSandbox(
  _command: string,
  _binShell?: string,
  _customConfig?: Partial<SandboxRuntimeConfig>,
  _abortSignal?: AbortSignal,
): Promise<string> {
  throw new SandboxRuntimeUnavailableError('wrapWithSandbox')
}

/**
 * Detect runtime kind so telemetry / UI banner / health check can report the
 * effective sandbox backend.
 *
 * Phase 3 闭环后默认返 `'rust'`：enforced 路径三平台均经 ExecBridge IPC →
 * Rust `AsyncSandboxRunner`；本函数仅在 stub `@acosmi-ai/sandbox-runtime`
 * package 还在 `node_modules/` 时被调用（P4-T7 退场后整体下线）。`'stub'`
 * 分支保留给 stub package 加载失败的兜底，作为 telemetry 信号。
 */
function getSandboxRuntimeKind(): SandboxRuntimeKind {
  // R-Sandbox Phase 4 wave 2 path B：BaseSandboxManager 退场后永远返 'rust'。
  // 5 UI banner condition `kind === 'stub'` 永不命中，commit 3 一并删 banner。
  return 'rust'
}

/**
 * Initialize sandbox with log monitoring enabled by default
 */
async function initialize(
  sandboxAskCallback?: SandboxAskCallback,
): Promise<void> {
  // If already initializing or initialized, return the promise
  if (initializationPromise) {
    return initializationPromise
  }

  // Check if sandboxing is enabled in settings
  if (!isSandboxingEnabled()) {
    return
  }

  // Wrap the callback to enforce allowManagedDomainsOnly policy.
  // This ensures all code paths (REPL, print/SDK) are covered.
  const wrappedCallback: SandboxAskCallback | undefined = sandboxAskCallback
    ? async (hostPattern: NetworkHostPattern) => {
        if (shouldAllowManagedSandboxDomainsOnly()) {
          logForDebugging(
            `[sandbox] Blocked network request to ${hostPattern.host} (allowManagedDomainsOnly)`,
          )
          return false
        }
        return sandboxAskCallback(hostPattern)
      }
    : undefined

  // Create the initialization promise synchronously (before any await) to prevent
  // race conditions where wrapWithSandbox() is called before the promise is assigned.
  initializationPromise = (async () => {
    try {
      // Resolve worktree main repo path once before building config.
      // Worktree status doesn't change mid-session, so this is cached for all
      // subsequent refreshConfig() calls (which must be synchronous to avoid
      // race conditions where pending requests slip through with stale config).
      if (worktreeMainRepoPath === undefined) {
        worktreeMainRepoPath = await detectWorktreeMainRepoPath(getCwd())
      }

      // R-Sandbox Phase 4 wave 2 path B：BaseSandboxManager 退场。Rust 端 acosmi-sandbox
      // 由 ExecBridge sandboxMode='enforced' 每次 spawnManaged 时从 settings 派生
      // SandboxRuntimeConfig (`acosmi-supervisor::ipc::handle_spawn_managed_sandboxed`
      // → `select_async_runner`)，TS 端不再主动 initialize 后端。wrappedCallback 与
      // settingsChangeDetector subscription 暂保留作占位，wave 3 P4-T6 默认开
      // enforced 后整体下线。
      void wrappedCallback

      // Subscribe to settings changes — kept as no-op：Rust runtime 每次 spawnManaged
      // 重读 settings；TS 端不需主动通知。保留 subscription 让 reset() 清理路径
      // 不破，且未来 Phase 5 acosmi-protocol 接通时可恢复实义。
      settingsSubscriptionCleanup = settingsChangeDetector.subscribe(() => {
        // no-op
      })
    } catch (error) {
      // Clear the promise on error so initialization can be retried
      initializationPromise = undefined

      // Log error but don't throw - let sandboxing fail gracefully
      logForDebugging(`Failed to initialize sandbox: ${errorMessage(error)}`)
    }
  })()

  return initializationPromise
}

/**
 * Refresh sandbox config from current settings immediately
 * Call this after updating permissions to avoid race conditions
 *
 * R-Sandbox Phase 4 wave 2 path B：BaseSandboxManager.updateConfig 退场。Rust 端
 * 每次 spawnManaged 都重读 settings 派生 config，无需 TS 主动 push。本函数保
 * no-op 兼容现 caller surface（add-dir / setSandboxSettings 等调用点）。
 */
function refreshConfig(): void {
  // no-op (path B)
}

/**
 * Reset sandbox state and clear memoized values
 *
 * R-Sandbox Phase 4 wave 2 path B：BaseSandboxManager.reset 退场，TS 端仅清自身
 * 缓存与订阅。Rust 端无 session-scoped state（每次 spawnManaged 重读 settings），
 * 不需 cross-language reset。
 */
async function reset(): Promise<void> {
  // Clean up settings subscription
  settingsSubscriptionCleanup?.()
  settingsSubscriptionCleanup = undefined
  worktreeMainRepoPath = undefined
  bareGitRepoScrubPaths.length = 0

  // Clear memoized caches
  checkDependencies.cache.clear?.()
  isSupportedPlatform.cache.clear?.()
  initializationPromise = undefined
}

/**
 * Add a command to the excluded commands list (commands that should not be sandboxed)
 * This is a CrabCode CLI-specific function that updates local settings.
 */
export function addToExcludedCommands(
  command: string,
  permissionUpdates?: Array<{
    type: string
    rules: Array<{ toolName: string; ruleContent?: string }>
  }>,
): string {
  const existingSettings = getSettingsForSource('localSettings')
  const existingExcludedCommands =
    existingSettings?.sandbox?.excludedCommands || []

  // Determine the command pattern to add
  // If there are suggestions with Bash rules, extract the pattern (e.g., "npm run test" from "npm run test:*")
  // Otherwise use the exact command
  let commandPattern: string = command

  if (permissionUpdates) {
    const bashSuggestions = permissionUpdates.filter(
      update =>
        update.type === 'addRules' &&
        update.rules.some(rule => rule.toolName === BASH_TOOL_NAME),
    )

    if (bashSuggestions.length > 0 && bashSuggestions[0]!.type === 'addRules') {
      const firstBashRule = bashSuggestions[0]!.rules.find(
        rule => rule.toolName === BASH_TOOL_NAME,
      )
      if (firstBashRule?.ruleContent) {
        // Extract pattern from Bash(command) or Bash(command:*) format
        const prefix = permissionRuleExtractPrefix(firstBashRule.ruleContent)
        commandPattern = prefix || firstBashRule.ruleContent
      }
    }
  }

  // Add to excludedCommands if not already present
  if (!existingExcludedCommands.includes(commandPattern)) {
    updateSettingsForSource('localSettings', {
      sandbox: {
        ...existingSettings?.sandbox,
        excludedCommands: [...existingExcludedCommands, commandPattern],
      },
    })
  }

  return commandPattern
}

// ============================================================================
// Export interface and implementation
// ============================================================================

export interface ISandboxManager {
  initialize(sandboxAskCallback?: SandboxAskCallback): Promise<void>
  isSupportedPlatform(): boolean
  isPlatformInEnabledList(): boolean
  getSandboxUnavailableReason(): string | undefined
  isSandboxingEnabled(): boolean
  isSandboxEnabledInSettings(): boolean
  checkDependencies(): SandboxDependencyCheck
  isAutoAllowBashIfSandboxedEnabled(): boolean
  areUnsandboxedCommandsAllowed(): boolean
  isSandboxRequired(): boolean
  areSandboxSettingsLockedByPolicy(): boolean
  setSandboxSettings(options: {
    enabled?: boolean
    autoAllowBashIfSandboxed?: boolean
    allowUnsandboxedCommands?: boolean
  }): Promise<void>
  getFsReadConfig(): FsReadRestrictionConfig
  getFsWriteConfig(): FsWriteRestrictionConfig
  getNetworkRestrictionConfig(): NetworkRestrictionConfig
  getAllowUnixSockets(): string[] | undefined
  getAllowLocalBinding(): boolean | undefined
  getIgnoreViolations(): IgnoreViolationsConfig | undefined
  getEnableWeakerNestedSandbox(): boolean | undefined
  getExcludedCommands(): string[]
  getProxyPort(): number | undefined
  getSocksProxyPort(): number | undefined
  getLinuxHttpSocketPath(): string | undefined
  getLinuxSocksSocketPath(): string | undefined
  waitForNetworkInitialization(): Promise<boolean>
  wrapWithSandbox(
    command: string,
    binShell?: string,
    customConfig?: Partial<SandboxRuntimeConfig>,
    abortSignal?: AbortSignal,
  ): Promise<string>
  cleanupAfterCommand(): void
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  getSandboxViolationStore(): SandboxViolationStore
  annotateStderrWithSandboxFailures(command: string, stderr: string): string
  getLinuxGlobPatternWarnings(): string[]
  refreshConfig(): void
  reset(): Promise<void>
  getSandboxRuntimeKind(): SandboxRuntimeKind
}

/**
 * CrabCode CLI sandbox manager - wraps sandbox-runtime with CrabCode-specific features
 */
export const SandboxManager: ISandboxManager = {
  // Custom implementations
  initialize,
  isSandboxingEnabled,
  isSandboxEnabledInSettings: getSandboxEnabledSetting,
  isPlatformInEnabledList,
  getSandboxUnavailableReason,
  isAutoAllowBashIfSandboxedEnabled,
  areUnsandboxedCommandsAllowed,
  isSandboxRequired,
  areSandboxSettingsLockedByPolicy,
  setSandboxSettings,
  getExcludedCommands,
  wrapWithSandbox,
  refreshConfig,
  reset,
  checkDependencies,

  // R-Sandbox Phase 4 wave 2 path B：BaseSandboxManager 退场，13 个 fs/network/socket
  // getter 全部从 settings 直派生（fs/network 经 deriveXxx shape adapter）。
  // 4 个 runtime-allocated 字段 (proxy ports / socket paths) 永远 undefined —
  // Rust 端分配，TS 不可见；execHttpHook.ts 退化为不经 sandbox proxy 路由
  // (enforced 模式下子进程仍 Rust 真隔离，安全实质不破)。
  // getSandboxViolationStore 永远返 STUB_VIOLATION_STORE：UI violation count
  // 永远 0，subscribe noop（Phase 5 acosmi-protocol 接通后恢复）。
  // annotateStderrWithSandboxFailures 保留接口 + TS noop 返回原 stderr
  // (Phase 5 增强 regex)。cleanupAfterCommand 仅保留 scrubBareGitRepoFiles。
  getFsReadConfig: () =>
    deriveFsReadConfig(convertToSandboxRuntimeConfig(getInitialSettings())),
  getFsWriteConfig: () =>
    deriveFsWriteConfig(convertToSandboxRuntimeConfig(getInitialSettings())),
  getNetworkRestrictionConfig: () =>
    deriveNetworkRestrictionConfig(
      convertToSandboxRuntimeConfig(getInitialSettings()),
    ),
  getIgnoreViolations: () => getInitialSettings().sandbox?.ignoreViolations,
  getLinuxGlobPatternWarnings,
  isSupportedPlatform,
  getAllowUnixSockets: () =>
    getInitialSettings().sandbox?.network?.allowUnixSockets ?? [],
  getAllowLocalBinding: () =>
    getInitialSettings().sandbox?.network?.allowLocalBinding ?? false,
  getEnableWeakerNestedSandbox: () =>
    getInitialSettings().sandbox?.enableWeakerNestedSandbox ?? false,
  getProxyPort: () => undefined,
  getSocksProxyPort: () => undefined,
  getLinuxHttpSocketPath: () => undefined,
  getLinuxSocksSocketPath: () => undefined,
  waitForNetworkInitialization: () => Promise.resolve(true),
  getSandboxViolationStore: () => STUB_VIOLATION_STORE,
  annotateStderrWithSandboxFailures: (
    _command: string,
    stderr: string,
  ): string => {
    // path B noop：Phase 5 真接通时增强 regex /Operation not permitted/ → sandbox hint
    return stderr
  },
  cleanupAfterCommand: (): void => {
    scrubBareGitRepoFiles()
  },
  getSandboxRuntimeKind,
}

// ============================================================================
// Re-export types (formerly from @acosmi-ai/sandbox-runtime; now ./types.ts)
// ============================================================================

export type {
  SandboxAskCallback,
  SandboxDependencyCheck,
  FsReadRestrictionConfig,
  FsWriteRestrictionConfig,
  NetworkRestrictionConfig,
  NetworkHostPattern,
  SandboxViolationEvent,
  SandboxViolationStore,
  SandboxRuntimeConfig,
  IgnoreViolationsConfig,
}
