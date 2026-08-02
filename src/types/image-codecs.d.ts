// ---------------------------------------------------------------------------
// Pure-JS image codec deps for W-DOC-VISION-QUALITY-REMEDIATION PR-5
// (src/utils/imageTranscode.ts). Neither package ships type declarations.
// ---------------------------------------------------------------------------

declare module 'bmp-js' {
  export interface BmpDecodedImage {
    width: number
    height: number
    /** Pixel data in ABGR byte order (library quirk, locked by unit test). */
    data: Buffer
    /** True when the source carried a real alpha channel (32bpp). */
    is_with_alpha: boolean
    bitPP: number
  }
  export function decode(buffer: Buffer): BmpDecodedImage
  const bmp: { decode: typeof decode }
  export default bmp
}

declare module 'utif2' {
  export interface UtifIFD {
    width: number
    height: number
    [tag: string]: unknown
  }
  export function decode(buffer: ArrayBuffer | Uint8Array): UtifIFD[]
  export function decodeImage(
    buffer: ArrayBuffer | Uint8Array,
    ifd: UtifIFD,
  ): void
  export function toRGBA8(ifd: UtifIFD): Uint8Array
  export function encodeImage(
    rgba: ArrayBuffer,
    width: number,
    height: number,
  ): ArrayBuffer
  const UTIF: {
    decode: typeof decode
    decodeImage: typeof decodeImage
    toRGBA8: typeof toRGBA8
    encodeImage: typeof encodeImage
  }
  export default UTIF
}
