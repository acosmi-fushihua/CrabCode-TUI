/**
 * Sandbox adapter layer — TS-side wrapper around the Rust acosmi-sandbox runtime.
 *
 * The retired `@acosmi-ai/sandbox-runtime`
 * package 整体退场；types 转入 `./types.ts` 内联，BaseSandboxManager 调用面
 * 全部退场。TUI 在 `Shell.ts` 内直接派生一次性配置，并以前缀 argv 调用同目录
 * `crabcode sandbox-exec` helper；没有长驻通信层或中转服务。
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
} from './types.js'
import {
  SandboxRuntimeUnavailableError,
  type SandboxRuntimeKind,
} from './errors.js'
import {
  isEnforcedBackendProbeResolved,
  probeEnforcedBackend,
  probeReasonTelemetrySlug,
  resetEnforcedBackendProbeCache,
  type EnforcedBackendProbeResult,
  type SandboxBackendId,
} from './enforcedBackendProbe.js'
import {
  computeSandboxFidelity,
  type SandboxFidelity,
} from './fidelity.js'
// 循环 import（本模块 ← sandboxNetworkProxy）**只在调用期成立**：那边在模块顶层
// 一个 binding 都不取（只在函数体里调 `convertToSandboxRuntimeConfig`），这边也
// 只在 `getSandboxFidelity()` 里调它 —— ESM 的 live binding 对这种形态是安全的。
import { currentSandboxFilteringProxyPort } from './sandboxNetworkProxy.js'
import {
  collectSandboxDenials,
  filterIgnoredDenials,
  formatSandboxViolationAnnotation,
} from './violationPatterns.js'
import {
  type AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
  logEvent,
} from 'src/services/analytics/index.js'
import { rmSync, statSync } from 'fs'
import { readFile } from 'fs/promises'
import { memoize } from 'lodash-es'
import { join, resolve, sep } from 'path'
import {
  getAdditionalDirectoriesForCrabcodeMd,
  getOriginalCwd,
} from '../../bootstrap/state.js'
// Sandbox config is rebuilt for every BashTool execution. Read the current TUI
// cwd so deny-write and worktree-detection paths bind to that command's state.
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
import type { PermissionRuleValue } from '../permissions/PermissionRule.js'
import { ripgrepCommand } from '../ripgrep.js'

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

  // Extract filesystem paths from Edit and Read rules. Per-command temp access
  // is added only by sandboxExecConfig; granting the shared CrabCode temp root
  // would let one sandbox replace another task's host-owned artifacts.
  const allowWrite: string[] = ['.']
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
 * BaseSandboxManager 退场后，能力探测由 `crabcode sandbox-probe --json` 直接
 * 完成；每条命令再由 `sandbox-exec` helper 施加平台规则。TS 端不重复探测
 * bwrap / landlock-userspace / ripgrep；release packaging supplies them.
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
 * W-SANDBOX-ENFORCED-DEADCODE PR-0：沙箱执行后端是否真的接线可用。
 *
 * 这是「诚实降级层」的判据。事故形态：`sandbox.enabled=true` 时 enforced 执行
 * 后端从未接线，Bash 首跑必抛 `SandboxRuntimeUnavailableError`，而系统提示词
 * 教模型立刻 `dangerouslyDisableSandbox:true` 绕过 —— 用户以为有沙箱，实际
 * 恒裸跑。有了这道门，后端不可用时整条链自动归 false（提示词沙箱节消失、
 * auto-allow 宽免收回、status/logo/审批标题/遥测同步转真话）。
 *
 * 进程级 memoize 在 `enforcedBackendProbe.ts`；`reset()` 联动清。
 */
function isEnforcedBackendWired(): boolean {
  return probeWithTelemetry().wired
}

/** 后端不可用的具体原因；可用时为 undefined。 */
function getEnforcedBackendUnavailableReason(): string | undefined {
  const probe = probeWithTelemetry()
  if (probe.wired) return undefined
  return probe.reason ?? 'unspecified'
}

/**
 * 读探测结论，并在**真的探测了一次**时发一条 `tengu_sandbox_backend_probe`
 * （SoT §3 关联方 #23）。
 *
 * 为什么发在这一层而不是探测模块里：`enforcedBackendProbe.ts` 保持只依赖 Node
 * 内置模块，以便在 TUI 运行时初始化前执行。在那里调用 `logEvent` 会把整个
 * analytics 依赖图拉入早期探测路径，因此遥测留在已经初始化的适配层。
 *
 * 「真的探测了一次」= 调用前 memoize 还没落定。所以 `reset()` 之后的重新探测
 * 会**再发一条**（那确实是一次新的探测），而同一会话里的几百次读结论只发一条。
 */
function probeWithTelemetry(): EnforcedBackendProbeResult {
  const alreadyResolved = isEnforcedBackendProbeResolved()
  const probe = probeEnforcedBackend()
  if (!alreadyResolved) {
    logEvent('tengu_sandbox_backend_probe', {
      wired: probe.wired,
      // 脱敏见 `probeReasonTelemetrySlug` —— 原始 reason 可能带绝对路径。
      reason: probeReasonTelemetrySlug(
        probe.reason,
      ) as unknown as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
      backend: (probe.backend ??
        'none') as unknown as AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS,
    })
  }
  return probe
}

/**
 * 本会话实际生效的后端标识；没有生效的沙箱时返 undefined。
 *
 * 判据是 `isSandboxingEnabled()`（含第 4 道门）而不是只看探测：后端接线良好但
 * 用户没开沙箱时，这一轮命令并没有被任何东西隔离，报一个后端名就是谎话。
 */
function getSandboxBackend(): SandboxBackendId | undefined {
  if (!isSandboxingEnabled()) return undefined
  return probeWithTelemetry().backend ?? undefined
}

/**
 * Check if sandboxing is enabled
 * This checks the user's enabled setting, platform support, enabledPlatforms
 * restriction, and whether the sandbox execution backend is actually wired.
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

  if (!getSandboxEnabledSetting()) {
    return false
  }

  // 第 4 道门（PR-0）：设置说要沙箱 ≠ 这个构建跑得出沙箱。后端没接线时整条
  // 链归 false 并由 getSandboxUnavailableReason() 如实披露，而不是让用户以为
  // 隔离生效。放在最后是因为它是唯一需要 spawn 子进程的判据 —— 前三道门任一
  // 不过就不必付探测成本。
  return isEnforcedBackendWired()
}

/**
 * 本会话当前配置的隔离保真度（W-SANDBOX-ENFORCED-DEADCODE PR-5）。
 *
 * 判据实现在 `./fidelity.ts`，与 `sandboxExecConfig.ts` 给每条命令算的那份
 * **是同一个纯函数**。两者输入也等价：deny 四表与域名两表全部来自
 * settings + `getCwd()`/`getOriginalCwd()`，per-command 的
 * `options.cwd`/`cwdFile` 只会往 `allowWrite` 里再加两三个条目，动不了任何
 * 判据位。所以「会话级 fidelity」不是一个更粗的近似，而是与 per-command
 * 结论逐字节相等的同一件事（`sandbox-fidelity.test.ts` 有断言钉着这条等价）。
 *
 * PR-8 Slice 4 新加的 `httpProxyPort` 也守着同一条等价：两侧读的是**同一个
 * 进程级单例**、用**同一组规则**做键（per-command 那边由
 * `buildSandboxExecConfig` 把 `ensureSandboxFilteringProxy` 的返回值当 override
 * 传下去，规则来自同一对 `convertToSandboxRuntimeConfig` +
 * `shouldAllowManagedSandboxDomainsOnly`）。唯一会分叉的时刻是「代理已经跑起来、
 * 但有人绕开 `buildSandboxExecConfig` 直接调 `deriveSandboxExecConfig`」——那条路
 * 只有测试在走，且分叉方向是 per-command 更保守（端口 0 ⇒ partial）。
 *
 * 这一点是 auto-allow 能在**权限层**（命令还没派生配置文件的时刻）问出
 * 「这台机器兑现得了吗」的前提；否则就只能在授权时刻猜。
 *
 * **副作用来自共享派生，不是本函数新引入的**：`convertToSandboxRuntimeConfig`
 * 会重算 `bareGitRepoScrubPaths`（#29316 的 post-command 清扫台账）。既有的
 * `getFsReadConfig` / `getFsWriteConfig` / `getNetworkRestrictionConfig`
 * （提示词每次构建都调）与 `refreshConfig()` 早就在任意时刻这么做了，而真正
 * 生效的那份台账是 `Shell.ts` 在 spawn 前一刻派生的那次。故这里多一次调用既不
 * 改变清扫语义，也不新增危险面。开销同理：`isSandboxAutoAllowActive()` 先过
 * `isSandboxingEnabled()`，沙箱没开（默认）时根本不会走到这里。
 */
function getSandboxFidelity(): SandboxFidelity {
  const runtime = convertToSandboxRuntimeConfig(getInitialSettings())
  // 域名三件套 + 代理传输安全语义先落成一个对象：它既是保真度的输入，又是
  // 「代理是不是为**这套**规则跑着」的查询键。分成两处各读一遍就是第二个漂移
  // 面 —— 那时代理按 A 组规则背书、报告按 B 组规则声明，而没有测试看得见这个差。
  const rules = {
    allowedDomains: runtime.network?.allowedDomains ?? [],
    deniedDomains: runtime.network?.deniedDomains ?? [],
    allowManagedDomainsOnly: shouldAllowManagedSandboxDomainsOnly(),
    policy: 'restricted' as const,
    allowLocalBinding: runtime.network?.allowLocalBinding ?? false,
  }
  return computeSandboxFidelity({
    platform: getPlatform(),
    filesystem: {
      allowRead: runtime.filesystem?.allowRead ?? [],
      allowWrite: runtime.filesystem?.allowWrite ?? [],
      denyRead: runtime.filesystem?.denyRead ?? [],
      denyWrite: runtime.filesystem?.denyWrite ?? [],
    },
    network: {
      ...rules,
      // 查而不起（同步路径上没得 await）：代理由 `buildSandboxExecConfig` 在派生
      // 每条命令的配置时起。所以「本会话还没跑过沙箱命令」时这里是 0 ⇒ partial
      // ⇒ 第一条命令仍走审批，之后代理起来了才恢复宽免。方向宁严。
      httpProxyPort: currentSandboxFilteringProxyPort(rules),
    },
  })
}

/**
 * 沙箱 auto-allow 免审批宽免此刻是否成立 —— **唯一判据点**
 * （W-SANDBOX-ENFORCED-DEADCODE PR-5，SoT §2.4 + §3 关联方 #20）。
 *
 * 事故前这个条件被逐字抄在 6 处（3 个授权点 + 3 个「规则不可达」提示面），
 * 每处都是 `isSandboxingEnabled() && isAutoAllowBashIfSandboxedEnabled()`。
 * 收进一个函数不是洁癖：**宽免的含义正在变**，而分散的合取式没有任何机制
 * 保证 6 处一起变。
 *
 * 三个乘数，各自回答一个不同的问题：
 *   1. `isSandboxingEnabled()` —— 这一轮到底隔不隔离（含 PR-0 第 4 道门
 *      `isEnforcedBackendWired()`，所以「后端没接线」已经被它拦掉了）
 *   2. `isAutoAllowBashIfSandboxedEnabled()` —— 用户要不要这个便利
 *   3. `getSandboxFidelity().level === 'full'` —— **沙箱真拦得住吗**
 *
 * 第 3 条是本 PR 新加的，也是唯一一条改变产品行为的：免审批的交换条件是
 * 「反正有沙箱兜着」，那么当沙箱兑现不了自己承诺的规则时，这个交换条件就
 * 不成立。**Linux 与 Windows 上它实际恒为 false**（Linux：`.` 恒在 allowWrite
 * 而 settings.json / `.crabcode/skills` 恒在 denyWrite，deny-within-allow 恒
 * 命中；Windows：整个路径层与网络层都不施加）。那不是回归，是把一件本来就
 * 不成立的事从「悄悄当成立」改成「明说不成立」—— 用户会多按审批按钮，而不是
 * 多一个没人拦的命令。macOS 上 fidelity 为 full 时照常发放。
 *
 * **调用方仍需自行判断这条命令会不会被沙箱**（`shouldUseSandbox(input)`）：
 * 那是 per-command 的（excludedCommands / dangerouslyDisableSandbox），本函数
 * 是会话级的，硬塞进来会把一个纯粹的会话级判据变成需要输入的函数，反而挡住
 * 「规则不可达」那三个没有命令可谈的展示面。
 */
function isSandboxAutoAllowActive(): boolean {
  if (!isSandboxingEnabled()) return false
  if (!isAutoAllowBashIfSandboxedEnabled()) return false
  return getSandboxFidelity().level === 'full'
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

  // W-SANDBOX-ENFORCED-DEADCODE PR-0：第 4 道门的披露面。这是本立项之前
  // **完全没有声音**的那一条 —— 后端从未接线，用户却一路看到「沙箱已启用」。
  // 三要素：具体原因 / 当前实际效果 / 出路。
  const backendReason = getEnforcedBackendUnavailableReason()
  if (backendReason !== undefined) {
    return (
      `sandbox.enabled is set but the sandbox execution backend is not wired: ${backendReason} · ` +
      `effect: sandboxing is DEGRADED TO OFF — commands run without filesystem or network isolation · ` +
      `way out: update to a CrabCode build that ships the sandbox helper, or set sandbox.enabled=false ` +
      `to stop requesting an isolation this build cannot deliver (run /sandbox for details)`
    )
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
 * Wrap command with sandbox — **构造性不可达 tripwire**（PR-0 之后）。
 *
 * 旧的命令字符串包装路径已退役；真实后端由 `Shell.ts` 直接加 helper argv
 * 前缀。后端不可用时 `isEnforcedBackendWired()` 会让 `shouldUseSandbox` 归
 * false，因此本函数没有生产调用者 —— 命中说明有路径绕过了直连入口，是 bug。
 *
 * 保留 throw 而不是静默降级：静默走 host 直跑 = 用户以为有隔离、实际没有，
 * 正是本立项要消灭的形态。
 *
 * 详 `src/utils/sandbox/errors.ts` +
 * `the sandbox direct-execution contract`。
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
 * 本会话里到底是谁在隔离命令 —— 遥测 (`sandbox_runtime_kind`) 与健康检查的真源。
 *
 * 旧实现无条件返回 `rust`，无法区分 helper 真正可用与用户仅打开了设置。
 * 当前值以本地 probe 为准，让「用户开了沙箱但后端不可用」成为显式状态。
 *
 * 三档判定顺序即语义（见 `SandboxRuntimeKind`）：先分「这一轮到底隔不隔离」，
 * 隔离才谈后端名；不隔离时再分「后端坏了」与「用户就没开」——两者的处置完全
 * 不同，混成一个值等于把这次立项要暴露的那件事又藏回去。
 */
function getSandboxRuntimeKind(): SandboxRuntimeKind {
  const backend = getSandboxBackend()
  if (backend !== undefined) return backend
  // 短路顺序有意：设置没开时**不付探测成本**（probe 要 spawn 一个子进程）。
  if (getSandboxEnabledSetting() && !isEnforcedBackendWired()) return 'degraded'
  return 'off'
}

/**
 * 把命令输出里的隔离拒绝行注解成模型读得懂的信息。
 *
 * ## `context.sandboxed` 必须由调用方给，且默认 false
 *
 * 判据表里的 `Permission denied` / `Operation not permitted` 在没有沙箱的机器上
 * 同样天天出现。把它们标成「沙箱拦截」的前提**只有一个**：这条命令确实经
 * `crabcode sandbox-exec` 跑过。这个事实只有调用方知道（它算过
 * `shouldUseSandbox(input)`），本函数不去猜 —— 不传就当没跑在沙箱里，
 * 原样返回、不计数。凭空造证据比不注解糟得多。
 *
 * ## 两件事都做
 *
 * 1. 注解文本（返回值）：给模型看的定性 + 点名 + 出路
 * 2. `sandbox.ignoreViolations` 过滤：用户明确忽略的拒绝不做注解
 */
function annotateStderrWithSandboxFailures(
  command: string,
  stderr: string,
  context?: { readonly sandboxed?: boolean },
): string {
  if (context?.sandboxed !== true) return stderr
  if (stderr.length === 0) return stderr

  const denials = filterIgnoredDenials(
    collectSandboxDenials(stderr),
    getInitialSettings().sandbox?.ignoreViolations,
  )
  if (denials.length === 0) return stderr

  const annotation = formatSandboxViolationAnnotation({
    denials,
    backend: getSandboxBackend() ?? null,
    allowUnsandboxedCommands: areUnsandboxedCommandsAllowed(),
    command,
  })
  // 前置而不是追加：下游有多处按长度截断输出，注解排在后面就会在「输出很长的
  // 失败命令」——正是最需要解释的那一类——上被切掉。原文一个字节不动。
  return `${annotation}\n${stderr}`
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

      // 这里不 initialize 任何后端，因为**没有需要被 initialize 的长驻后端**。
      //
      // Rust 不读取 settings：隔离配置由 TS 单源，经
      // `sandboxExecConfig.ts::buildSandboxExecConfig` 从 settings
      // 全保真派生、落 0600 临时文件，helper（`crabcode sandbox-exec`）只做严格
      // 反序列化与平台映射，读不出任何 TS 没写进去的东西。
      //
      // 配置因此是**每条命令现派生**的：不存在需要被 push 的会话级后端状态，
      // 这才是 refreshConfig / 下面这个 subscription 可以是 no-op 的真实理由。
      void wrappedCallback

      // Subscribe to settings changes — kept as no-op：配置每条命令现派生（见上），
      // 没有要通知的对象。保留 subscription 只为让 reset() 的清理路径不破。
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
 * no-op —— 且这次是有理由的 no-op（PR-6 更正措辞）。隔离配置由
 * `buildSandboxExecConfig()` 在**每一条**沙箱命令 spawn 前从当前 settings 现场
 * 派生并落成一次性 0600 文件，没有任何一份被缓存的 config 需要被刷新。本函数
 * 保留只为兼容现 caller surface（add-dir / setSandboxSettings 等调用点）。
 */
function refreshConfig(): void {
  // no-op — 见上：没有缓存态可刷
}

/**
 * Reset sandbox state and clear memoized values
 *
 * TS 端仅清自身缓存与订阅。没有 cross-language reset 可做：helper 是 per-command
 * 进程，命令结束即消失，不持有任何 session-scoped 状态。
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
  // PR-0：后端探测缓存与上面三个同生命周期 —— 漏清会让 reset() 后的第 4 道门
  // 继续复用旧结论（例如换了 helper 二进制却仍报不可用）。
  resetEnforcedBackendProbeCache()
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
  isEnforcedBackendWired(): boolean
  getEnforcedBackendUnavailableReason(): string | undefined
  /** 本会话实际生效的后端标识；没有生效的沙箱时 undefined。 */
  getSandboxBackend(): SandboxBackendId | undefined
  checkDependencies(): SandboxDependencyCheck
  isAutoAllowBashIfSandboxedEnabled(): boolean
  /** 本会话配置的隔离保真度（哪些规则兑现不了）。 */
  getSandboxFidelity(): SandboxFidelity
  /** 沙箱 auto-allow 免审批宽免此刻是否成立 —— 唯一判据点。 */
  isSandboxAutoAllowActive(): boolean
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
  annotateStderrWithSandboxFailures(
    command: string,
    stderr: string,
    context?: { readonly sandboxed?: boolean },
  ): string
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
  isEnforcedBackendWired,
  getEnforcedBackendUnavailableReason,
  getSandboxBackend,
  isPlatformInEnabledList,
  getSandboxUnavailableReason,
  isAutoAllowBashIfSandboxedEnabled,
  getSandboxFidelity,
  isSandboxAutoAllowActive,
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
  // Text annotations are derived locally from command stderr. No presentation
  // store or cross-process feedback channel is part of the direct TUI closure.
  // cleanupAfterCommand only keeps scrubBareGitRepoFiles.
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
  annotateStderrWithSandboxFailures,
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
  SandboxRuntimeConfig,
  IgnoreViolationsConfig,
}
