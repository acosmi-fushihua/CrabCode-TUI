/**
 * Pure-JS BMP/TIFF → PNG transcoding lets Windows screenshots and scanner
 * output enter the normal image chain without a native decoder dependency.
 * Unsupported sub-formats fail loudly.
 *
 * 通道序契约（单测锁定）：bmp-js 输出 ABGR，此处转 RGBA；
 * utif2 的 toRGBA8 直出 RGBA。
 */

import { decode as decodeBmp } from 'bmp-js'
import { encode as encodePng } from 'fast-png'
import UTIF from 'utif2'

/**
 * Extensions that the Read tool accepts but must transcode to PNG before the
 * image pipeline (the gateway accepts only jpeg/png/gif/webp). `.ico` is
 * deliberately NOT here (裁决 D8: multi-size container, edge scenario) — it
 * stays rejected, with a convert-to-PNG hint in the rejection message.
 */
export const TRANSCODE_IMAGE_EXTENSIONS: ReadonlySet<string> = new Set([
  'bmp',
  'tiff',
  'tif',
])

/** True if `ext` (with/without dot, any case) needs the PNG transcode step. */
export function isTranscodeImageExtension(ext: string): boolean {
  const normalized = ext.startsWith('.') ? ext.slice(1) : ext
  return TRANSCODE_IMAGE_EXTENSIONS.has(normalized.toLowerCase())
}

/** Input cap — decoded RGBA of a huge scan can be several × the file size. */
export const TRANSCODE_IMAGE_MAX_BYTES = 50 * 1024 * 1024 // 50 MB

function cannotDecodeError(ext: string, detail?: string): Error {
  return new Error(
    `Cannot decode this .${ext.replace(/^\./, '')} image on this platform. ` +
      `Convert it to PNG/JPEG first (e.g. via Bash: \`sips -s format png\` on macOS or ImageMagick \`convert\`).` +
      (detail ? ` (${detail})` : ''),
  )
}

/**
 * Transcode a bmp/tiff/tif file's bytes to a PNG buffer. Throws an honest,
 * actionable error on anything undecodable — never silently returns garbage.
 * 兼容旧签名（丢弃 note）；需要多页标注的调用方用 WithInfo 变体。
 */
export function transcodeImageToPng(buffer: Buffer, ext: string): Buffer {
  return transcodeImageToPngWithInfo(buffer, ext).png
}

/**
 * 同 transcodeImageToPng，另返回诚实性标注（2026-07-04 审计 PR-10）：
 * 多页 TIFF 只解码第 1 页是既有行为，note 把这一点告知用户/模型而非静默。
 */
export function transcodeImageToPngWithInfo(
  buffer: Buffer,
  ext: string,
): { png: Buffer; note: string | null } {
  let multiPageNote: string | null = null
  const normalized = (
    ext.startsWith('.') ? ext.slice(1) : ext
  ).toLowerCase()
  if (buffer.length === 0) {
    throw cannotDecodeError(normalized, 'file is empty')
  }
  if (buffer.length > TRANSCODE_IMAGE_MAX_BYTES) {
    throw new Error(
      `This .${normalized} image is too large to transcode (over ${TRANSCODE_IMAGE_MAX_BYTES / (1024 * 1024)}MB). Convert it to PNG/JPEG first.`,
    )
  }
  try {
    if (normalized === 'bmp') {
      const decoded = decodeBmp(buffer)
      if (!decoded.width || !decoded.height) {
        throw new Error('bmp decoded to zero dimensions')
      }
      // bmp-js emits ABGR per pixel; PNG wants RGBA. 24bpp sources carry no
      // real alpha (the A byte reads 0) → force opaque, else the PNG would be
      // fully transparent.
      const abgr = decoded.data
      const rgba = new Uint8Array(abgr.length)
      for (let i = 0; i < abgr.length; i += 4) {
        rgba[i] = abgr[i + 3]! // R
        rgba[i + 1] = abgr[i + 2]! // G
        rgba[i + 2] = abgr[i + 1]! // B
        rgba[i + 3] = decoded.is_with_alpha ? abgr[i]! : 255 // A
      }
      return {
        png: Buffer.from(
          encodePng({
            width: decoded.width,
            height: decoded.height,
            data: rgba,
            channels: 4,
          }),
        ),
        note: multiPageNote,
      }
    }
    if (normalized === 'tiff' || normalized === 'tif') {
      const ifds = UTIF.decode(buffer)
      const first = ifds[0]
      if (!first) throw new Error('tiff contains no image directory')
      UTIF.decodeImage(buffer, first)
      const rgba = UTIF.toRGBA8(first)
      if (!first.width || !first.height || rgba.length === 0) {
        throw new Error('tiff decoded to empty image')
      }
      // 2026-07-04 审计 PR-10:多页 TIFF 只解首页是既有行为,但此前静默——
      // 调用方经 transcodeImageToPngWithInfo 把该标注带给用户/模型（诚实性）。
      if (ifds.length > 1) {
        multiPageNote = `[TIFF 共 ${ifds.length} 页，仅解码第 1 页]`
      }
      return {
        png: Buffer.from(
          encodePng({
            width: first.width,
            height: first.height,
            data: rgba,
            channels: 4,
          }),
        ),
        note: multiPageNote,
      }
    }
    throw new Error(`unsupported transcode extension .${normalized}`)
  } catch (e) {
    throw cannotDecodeError(
      normalized,
      e instanceof Error ? e.message : undefined,
    )
  }
}
