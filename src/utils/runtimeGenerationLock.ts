type RuntimeGenerationLock = () => Promise<void>

let generationLock: RuntimeGenerationLock | undefined

export function installRuntimeGenerationLock(
  lock: RuntimeGenerationLock,
): void {
  generationLock = lock
}

/**
 * Native TUI children are already held by their process-owning executable's
 * generation lease. Ordinary CLI installs the existing JS native-installer
 * lock; other process-owned runtimes intentionally have no second installer
 * dependency.
 */
export async function lockActiveRuntimeGeneration(): Promise<void> {
  await generationLock?.()
}
