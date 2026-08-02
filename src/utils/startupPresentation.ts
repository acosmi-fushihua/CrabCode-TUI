type StartupPresentationPrefetch = () => Promise<void>

let startupPresentationPrefetch: StartupPresentationPrefetch | undefined

export function installStartupPresentationPrefetch(
  prefetch: StartupPresentationPrefetch,
): void {
  startupPresentationPrefetch = prefetch
}

export async function prefetchStartupPresentationIfInstalled(): Promise<void> {
  await startupPresentationPrefetch?.()
}
