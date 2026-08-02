import {
  MAX_ARTIFACT_ANIMATED_PIXEL_WORK,
  MAX_ARTIFACT_ANIMATION_FRAMES,
  MAX_ARTIFACT_IMAGE_DIMENSION,
  MAX_ARTIFACT_IMAGE_PIXELS,
} from '../types/toolArtifact.js'

export {
  MAX_ARTIFACT_ANIMATED_PIXEL_WORK,
  MAX_ARTIFACT_ANIMATION_FRAMES,
  MAX_ARTIFACT_IMAGE_DIMENSION,
  MAX_ARTIFACT_IMAGE_PIXELS,
} from '../types/toolArtifact.js'

type ImageSafety = { width: number; height: number; frames: number }

export function sniffSupportedImageMimeBytes(bytes: Buffer): string | null {
  const ascii = (start: number, end: number) =>
    bytes.subarray(start, end).toString('ascii')
  if (
    bytes.length >= 8 &&
    bytes.subarray(0, 8).equals(
      Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    )
  ) return 'image/png'
  if (
    bytes.length >= 3 &&
    bytes[0] === 0xff &&
    bytes[1] === 0xd8 &&
    bytes[2] === 0xff
  ) return 'image/jpeg'
  if (
    bytes.length >= 6 &&
    (ascii(0, 6) === 'GIF87a' || ascii(0, 6) === 'GIF89a')
  ) return 'image/gif'
  if (
    bytes.length >= 12 &&
    ascii(0, 4) === 'RIFF' &&
    ascii(8, 12) === 'WEBP'
  ) return 'image/webp'
  return null
}

function readPngSafety(bytes: Buffer): ImageSafety | null {
  if (bytes.length < 24) return null
  if (
    bytes.readUInt32BE(8) !== 13 ||
    bytes.subarray(12, 16).toString('ascii') !== 'IHDR'
  ) return null
  const width = bytes.readUInt32BE(16)
  const height = bytes.readUInt32BE(20)
  let frames = 1
  let offset = 8
  let sawIdat = false
  let sawIend = false
  while (offset + 12 <= bytes.length) {
    const length = bytes.readUInt32BE(offset)
    if (length > bytes.length - offset - 12) return null
    const type = bytes.subarray(offset + 4, offset + 8).toString('ascii')
    if (type === 'acTL') {
      if (length < 8) return null
      frames = bytes.readUInt32BE(offset + 8)
    }
    if (type === 'IDAT') sawIdat = true
    offset += length + 12
    if (type === 'IEND') {
      if (length !== 0 || offset !== bytes.length) return null
      sawIend = true
      break
    }
  }
  return sawIdat && sawIend ? { width, height, frames } : null
}

function skipGifSubBlocks(bytes: Buffer, start: number): number | null {
  let offset = start
  while (offset < bytes.length) {
    const length = bytes[offset]!
    offset += 1
    if (length === 0) return offset
    if (length > bytes.length - offset) return null
    offset += length
  }
  return null
}

function readGifSafety(bytes: Buffer): ImageSafety | null {
  if (bytes.length < 13) return null
  const width = bytes.readUInt16LE(6)
  const height = bytes.readUInt16LE(8)
  const globalPacked = bytes[10]!
  let offset = 13
  if ((globalPacked & 0x80) !== 0) {
    offset += 3 * 2 ** ((globalPacked & 0x07) + 1)
  }
  let frames = 0
  let sawTrailer = false
  while (offset < bytes.length) {
    const marker = bytes[offset++]!
    if (marker === 0x3b) {
      sawTrailer = offset === bytes.length
      break
    }
    if (marker === 0x21) {
      if (offset >= bytes.length) return null
      offset += 1
      const next = skipGifSubBlocks(bytes, offset)
      if (next === null) return null
      offset = next
      continue
    }
    if (marker !== 0x2c || offset + 9 > bytes.length) return null
    const localPacked = bytes[offset + 8]!
    offset += 9
    if ((localPacked & 0x80) !== 0) {
      offset += 3 * 2 ** ((localPacked & 0x07) + 1)
    }
    if (offset >= bytes.length) return null
    offset += 1
    const next = skipGifSubBlocks(bytes, offset)
    if (next === null) return null
    offset = next
    frames += 1
  }
  return frames > 0 && sawTrailer ? { width, height, frames } : null
}

function readJpegSafety(bytes: Buffer): ImageSafety | null {
  if (
    bytes.length < 4 ||
    bytes[0] !== 0xff ||
    bytes[1] !== 0xd8 ||
    bytes[bytes.length - 2] !== 0xff ||
    bytes[bytes.length - 1] !== 0xd9
  ) return null
  const sofMarkers = new Set([
    0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7,
    0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf,
  ])
  let offset = 2
  while (offset + 1 < bytes.length) {
    while (offset < bytes.length && bytes[offset] !== 0xff) offset += 1
    while (offset < bytes.length && bytes[offset] === 0xff) offset += 1
    if (offset >= bytes.length) return null
    const marker = bytes[offset++]!
    if (marker === 0xd9 || marker === 0xda) return null
    if (marker === 0x01 || (marker >= 0xd0 && marker <= 0xd7)) continue
    if (offset + 2 > bytes.length) return null
    const length = bytes.readUInt16BE(offset)
    if (length < 2 || length > bytes.length - offset) return null
    if (sofMarkers.has(marker)) {
      if (length < 7) return null
      return {
        width: bytes.readUInt16BE(offset + 5),
        height: bytes.readUInt16BE(offset + 3),
        frames: 1,
      }
    }
    offset += length
  }
  return null
}

function readWebpSafety(bytes: Buffer): ImageSafety | null {
  if (
    bytes.length < 20 ||
    bytes.subarray(0, 4).toString('ascii') !== 'RIFF' ||
    bytes.subarray(8, 12).toString('ascii') !== 'WEBP'
  ) return null
  const riffSize = bytes.readUInt32LE(4)
  if (riffSize + 8 !== bytes.length) return null
  const format = bytes.subarray(12, 16).toString('ascii')
  let width: number
  let height: number
  let animated = false
  if (format === 'VP8X') {
    if (bytes.length < 30) return null
    animated = (bytes[20]! & 0x02) !== 0
    width = 1 + bytes.readUIntLE(24, 3)
    height = 1 + bytes.readUIntLE(27, 3)
  } else if (format === 'VP8 ') {
    if (
      bytes.length < 30 ||
      bytes[23] !== 0x9d ||
      bytes[24] !== 0x01 ||
      bytes[25] !== 0x2a
    ) return null
    width = bytes.readUInt16LE(26) & 0x3fff
    height = bytes.readUInt16LE(28) & 0x3fff
  } else if (format === 'VP8L') {
    if (bytes.length < 25 || bytes[20] !== 0x2f) return null
    const bits = bytes.readUInt32LE(21)
    width = (bits & 0x3fff) + 1
    height = ((bits >>> 14) & 0x3fff) + 1
  } else {
    return null
  }

  let frames = 0
  let offset = 12
  let sawImagePayload = false
  while (offset + 8 <= bytes.length) {
    const type = bytes.subarray(offset, offset + 4).toString('ascii')
    const length = bytes.readUInt32LE(offset + 4)
    const dataOffset = offset + 8
    if (length > bytes.length - dataOffset) return null
    if (type === 'ANMF') frames += 1
    if (type === 'VP8 ' || type === 'VP8L' || type === 'ANMF') {
      sawImagePayload = true
    }
    offset = dataOffset + length + (length & 1)
  }
  if (
    offset !== bytes.length ||
    !sawImagePayload ||
    (animated && frames === 0)
  ) return null
  return { width, height, frames: Math.max(1, frames) }
}

function imageSafety(bytes: Buffer, mimeType: string): ImageSafety | null {
  switch (mimeType) {
    case 'image/png':
      return readPngSafety(bytes)
    case 'image/jpeg':
      return readJpegSafety(bytes)
    case 'image/gif':
      return readGifSafety(bytes)
    case 'image/webp':
      return readWebpSafety(bytes)
    default:
      return null
  }
}

/**
 * Parsed safety metadata for a supported image, or null when the container
 * structure cannot be strictly parsed. Callers that must not reject
 * loosely-structured-but-decodable images gate on a non-null result before
 * consulting the decode budget.
 */
export function readImageArtifactSafetyMetadata(
  bytes: Buffer,
  mimeType: string,
): { width: number; height: number; frames: number } | null {
  return imageSafety(bytes, mimeType)
}

/**
 * Pure dimension/frame budget predicate — the ACTUAL safety property behind the
 * decode-budget gate (it bounds decode work to defuse decode bombs). Dimensions
 * may come from the strict structural parser (`readImageArtifactSafetyMetadata`)
 * OR from a header-only decoder metadata read; callers that must admit
 * loosely-structured-but-decodable images resolve the dimensions the lenient way
 * and still enforce this same budget. Kept separate from the structural parse so
 * "unparseable structure" and "over budget" are no longer conflated.
 */
export function isImageDimensionWithinArtifactBudget(
  width: number,
  height: number,
  frames: number,
): boolean {
  if (width <= 0 || height <= 0 || frames <= 0) return false
  const pixels = width * height
  return (
    width <= MAX_ARTIFACT_IMAGE_DIMENSION &&
    height <= MAX_ARTIFACT_IMAGE_DIMENSION &&
    pixels <= MAX_ARTIFACT_IMAGE_PIXELS &&
    frames <= MAX_ARTIFACT_ANIMATION_FRAMES &&
    pixels * frames <= MAX_ARTIFACT_ANIMATED_PIXEL_WORK
  )
}

export function isImageWithinArtifactDecodeBudget(
  bytes: Buffer,
  mimeType: string,
): boolean {
  const safety = imageSafety(bytes, mimeType)
  if (!safety) return false
  return isImageDimensionWithinArtifactBudget(
    safety.width,
    safety.height,
    safety.frames,
  )
}
