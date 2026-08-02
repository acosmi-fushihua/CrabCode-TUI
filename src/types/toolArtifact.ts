export type ToolArtifactKind =
  | 'image'
  | 'video'
  | 'audio'
  | 'document'
  | 'archive'
  | 'other'

export const MAX_TOOL_ARTIFACT_BYTES_BY_KIND: Readonly<
  Record<ToolArtifactKind, number>
> = {
  image: 25 * 1024 * 1024,
  video: 500 * 1024 * 1024,
  audio: 250 * 1024 * 1024,
  document: 100 * 1024 * 1024,
  archive: 500 * 1024 * 1024,
  other: 100 * 1024 * 1024,
}

export const MAX_TOOL_ARTIFACTS_PER_RESULT = 16
export const MAX_TOTAL_TOOL_ARTIFACT_BYTES_PER_RESULT = 512 * 1024 * 1024
export const MAX_TOOL_ARTIFACT_CANDIDATES_SCANNED = 64
export const MAX_ARTIFACT_IMAGE_DIMENSION = 65_535
export const MAX_ARTIFACT_IMAGE_PIXELS = 40_000_000
export const MAX_ARTIFACT_ANIMATION_FRAMES = 120
export const MAX_ARTIFACT_ANIMATED_PIXEL_WORK = 80_000_000

export type ToolArtifactLocation =
  | { type: 'runtimePath'; path: string }
  | { type: 'externalUri'; uri: string }

/** Producer-declared candidate. The host overwrites provenance after validation. */
export type ToolArtifactCandidate = {
  id: string
  kind: ToolArtifactKind
  mimeType: string
  displayName: string
  location: ToolArtifactLocation
  byteSize?: number
  sha256?: string
}

/** Validated artifact persisted and projected with the producing tool-use id. */
export type ToolArtifact = ToolArtifactCandidate & {
  producerToolUseId: string
}
