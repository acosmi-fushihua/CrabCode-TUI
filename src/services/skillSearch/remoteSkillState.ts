// Stub for remote skill state (ANT-ONLY)
export type DiscoveredRemoteSkill = { url: string; name?: string; [key: string]: unknown }

export function stripCanonicalPrefix(_slug: string): string {
  return _slug
}

export function getDiscoveredRemoteSkill(_slug: string): DiscoveredRemoteSkill | undefined {
  return undefined
}
