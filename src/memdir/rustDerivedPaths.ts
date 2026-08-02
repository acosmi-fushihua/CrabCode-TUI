import { dirname, join, sep } from 'path'
import { getAutoMemPath, getMemoryBaseDir } from './paths.js'

const RUST_DERIVED_DIRNAME = '.memory-rust-derived'

/**
 * Canonical v6 Rust-derived data root.
 *
 * It is intentionally a sibling of the auto-memory truth tree:
 *   <project_state_dir>/memory/
 *   <project_state_dir>/.memory-rust-derived/
 */
export function getRustDerivedMemoryPath(): string {
  return (
    join(dirname(getAutoMemPath()), RUST_DERIVED_DIRNAME) + sep
  ).normalize('NFC')
}

/** Global lease directory for unscoped memory Tier work. */
export function getGlobalExecutorLeaseDir(): string {
  return (join(getMemoryBaseDir(), RUST_DERIVED_DIRNAME) + sep).normalize('NFC')
}
