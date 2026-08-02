/**
 * Post-commit attribution hook installation.
 *
 * Installs a `prepare-commit-msg` git hook into the given repository/worktree
 * that injects CrabCode attribution metadata into commit messages.
 *
 * This module is imported dynamically (lazy) from worktree.ts so it is only
 * loaded when the COMMIT_ATTRIBUTION feature flag is enabled.
 */

import { mkdir, readFile, writeFile } from 'fs/promises'
import { join } from 'path'
import { CRAB_CODE_COMMIT_TRAILER } from '../constants/product.js'

const HOOK_FILENAME = 'prepare-commit-msg'

/**
 * The hook script content injected into the repository's hooks directory.
 * It runs as a shell script and amends commit messages with CrabCode
 * attribution when a session is active.
 */
const HOOK_SCRIPT = `#!/bin/sh
# CrabCode commit attribution hook (auto-installed)
# This hook is managed by CrabCode — do not edit manually.
if [ -n "$CRABCODE_SESSION_ID" ]; then
  echo "" >> "$1"
  echo "${CRAB_CODE_COMMIT_TRAILER}" >> "$1"
fi
`

const CRABCODE_MARKER = '# CrabCode commit attribution hook (auto-installed)'

/**
 * Installs the `prepare-commit-msg` hook in the given hooks directory.
 *
 * - If no hook file exists, the hook is created from scratch.
 * - If an existing hook already contains the CrabCode marker, it is left
 *   unchanged (idempotent).
 * - If an existing hook does NOT contain the marker, the CrabCode logic is
 *   appended so the existing hook is not broken.
 *
 * @param repoPath   Absolute path to the repository root (or worktree root).
 * @param hooksDir   Optional override for the hooks directory path.
 *                   Defaults to `<repoPath>/.git/hooks`.
 */
export async function installPrepareCommitMsgHook(
  repoPath: string,
  hooksDir?: string,
): Promise<void> {
  const resolvedHooksDir = hooksDir ?? join(repoPath, '.git', 'hooks')
  const hookPath = join(resolvedHooksDir, HOOK_FILENAME)

  // Ensure the hooks directory exists.
  await mkdir(resolvedHooksDir, { recursive: true })

  let existing: string | null = null
  try {
    existing = await readFile(hookPath, 'utf8')
  } catch {
    // File does not exist — will be created below.
  }

  if (existing !== null) {
    // Already installed — nothing to do.
    if (existing.includes(CRABCODE_MARKER)) {
      return
    }
    // Append to existing hook.
    await writeFile(hookPath, `${existing}\n${HOOK_SCRIPT}`, { mode: 0o755 })
  } else {
    // Create new hook file.
    await writeFile(hookPath, HOOK_SCRIPT, { mode: 0o755 })
  }
}
