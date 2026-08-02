/**
 * Pure, import-free chunk-format helpers for macOsKeychainStorage.
 *
 * A credentials payload that
 * would overflow the `security -i` stdin line buffer is stored as N chunked
 * keychain entries so EVERY write goes through `security -i` stdin and the
 * `-X <hexValue>` argv branch (which leaked recoverable hex to any `ps`
 * observer is deleted.
 *
 * Layout:
 *   - The base entry (`<service>`) holds a manifest value
 *     `v2:<nHex>:<lenHex>:<headHex>` — version tag, total chunk count, total
 *     payload-hex length, and the first hex slice.
 *   - Entries `<service>#1` .. `<service>#(n-1)` hold the remaining raw hex
 *     slices.
 *   - A small payload (the common case — account OAuth tokens) fits entirely
 *     in `head` ⇒ n = 1 ⇒ exactly one keychain entry, one spawn, no behavior
 *     difference from before this change.
 *
 * Every stored value is escape-safe by construction (pure hex, or `v2:` plus
 * hex/colons) so it can be written through `security -i` with `-w "<value>"`
 * — no `-X` hex argv, no double encoding.
 *
 * This module MUST stay import-free (no execa, no fs, no JSON helpers): it is
 * pulled in by macOsKeychainHelpers.ts, which keychainPrefetch.ts loads at the
 * very top of startup. Pure string/number math only — see keychainPrefetch.ts
 * and macOsKeychainHelpers.ts file headers for the import-minimalism contract.
 *
 * Tested directly by tests/unit/keychain-chunking.test.ts (no `security`
 * subprocess required).
 */

// `security -i` reads stdin with a 4096-byte fgets() buffer (BUFSIZ on darwin).
// A command line longer than this is truncated mid-argument: the first 4096
// bytes are consumed as one command (unterminated quote → fails), the overflow
// is interpreted as a second unknown command. Net: non-zero exit with NO data
// written, but the *previous* keychain entry is left intact — which fallback
// storage then reads as stale. See #30337. The chunk scheme keeps every
// command strictly under this limit. Headroom of 64B below the raw 4096 guards
// against edge-case line-terminator accounting differences.
export const SECURITY_STDIN_LINE_LIMIT = 4096 - 64

// Fixed character cost of the write command frame
//   `add-generic-password -U -a "<u>" -s "<s>" -w "<v>"\n`
// excluding the <u>/<s>/<v> contents: 28 + 6 + 6 + 2 = 42.
const COMMAND_FRAME_COST = 42

// Manifest entry value layout: `v2:<nHex>:<lenHex>:<headHex>`.
export const MANIFEST_PREFIX = 'v2:'

// Reserves used when budgeting head/chunk sizes so the actual nHex / chunk
// index / lenHex digit counts can never push a command past the line limit.
const N_HEX_RESERVE = 4 // up to 0xffff chunks
const LEN_HEX_RESERVE = 8 // up to 0xffffffff hex chars
const INDEX_DIGITS_RESERVE = 4 // `#<i>` index up to 4 digits

// Hard cap on chunk count — a corrupt manifest claiming millions of chunks
// must not trigger an unbounded read loop. 8192 chunks ≈ 32 MB payload, far
// beyond any real credentials record.
export const MAX_CHUNKS = 8192

export interface ChunkPlan {
  /** Total entry count (manifest + chunks); always >= 1. */
  n: number
  /** Hex slice stored inside the manifest (base) entry. */
  head: string
  /** Hex slices for entries #1 .. #(n-1); length === n - 1. */
  chunks: string[]
}

/** Character length of the `security -i` add command for one entry. */
export function commandLength(
  username: string,
  service: string,
  value: string,
): number {
  return COMMAND_FRAME_COST + username.length + service.length + value.length
}

/**
 * Largest hex slice that fits in the manifest entry (`maxHead`) and in a chunk
 * entry (`maxChunk`), given the runtime username / base service name. Both are
 * even so every hex slice ends on a byte boundary.
 */
export function computeBudgets(
  username: string,
  baseService: string,
): { maxHead: number; maxChunk: number } {
  const maxManifestValue =
    SECURITY_STDIN_LINE_LIMIT -
    COMMAND_FRAME_COST -
    username.length -
    baseService.length
  // manifest value = 'v2:' + nHex + ':' + lenHex + ':' + head
  let maxHead =
    maxManifestValue -
    MANIFEST_PREFIX.length -
    N_HEX_RESERVE -
    1 -
    LEN_HEX_RESERVE -
    1
  // chunk service = `${baseService}#${i}`
  let maxChunk =
    SECURITY_STDIN_LINE_LIMIT -
    COMMAND_FRAME_COST -
    username.length -
    baseService.length -
    1 -
    INDEX_DIGITS_RESERVE
  // even-align so every hex slice ends on a byte boundary
  maxHead -= maxHead % 2
  maxChunk -= maxChunk % 2
  return { maxHead: Math.max(0, maxHead), maxChunk: Math.max(2, maxChunk) }
}

/** Split a payload hex string into a manifest head + chunk slices. */
export function splitPayloadHex(
  payloadHex: string,
  maxHead: number,
  maxChunk: number,
): ChunkPlan {
  if (payloadHex.length <= maxHead) {
    return { n: 1, head: payloadHex, chunks: [] }
  }
  const head = payloadHex.slice(0, maxHead)
  const rest = payloadHex.slice(maxHead)
  const chunks: string[] = []
  for (let i = 0; i < rest.length; i += maxChunk) {
    chunks.push(rest.slice(i, i + maxChunk))
  }
  return { n: 1 + chunks.length, head, chunks }
}

/** Build the manifest (base) entry value. */
export function encodeManifestValue(
  n: number,
  totalHexLen: number,
  head: string,
): string {
  return `${MANIFEST_PREFIX}${n.toString(16)}:${totalHexLen.toString(16)}:${head}`
}

export type ParsedManifest =
  | { kind: 'v2'; n: number; totalLen: number; head: string }
  | { kind: 'legacy' }
  | { kind: 'corrupt' }

/**
 * Classify a base-entry value read back from the keychain.
 *  - `v2`     — a chunk manifest written by this code.
 *  - `legacy` — the value IS the credentials JSON string (written by the
 *               pre-chunking code as `-X hex(json)`; `security find -w`
 *               returns the decoded JSON). JSON always starts with '{'.
 *  - `corrupt`— a `v2:`-prefixed value that does not parse.
 */
export function parseManifestValue(value: string): ParsedManifest {
  if (!value.startsWith(MANIFEST_PREFIX)) {
    return { kind: 'legacy' }
  }
  const rest = value.slice(MANIFEST_PREFIX.length)
  const c1 = rest.indexOf(':')
  if (c1 < 0) return { kind: 'corrupt' }
  const nHex = rest.slice(0, c1)
  const rest2 = rest.slice(c1 + 1)
  const c2 = rest2.indexOf(':')
  if (c2 < 0) return { kind: 'corrupt' }
  const lenHex = rest2.slice(0, c2)
  const head = rest2.slice(c2 + 1)
  if (!/^[0-9a-f]+$/.test(nHex) || !/^[0-9a-f]+$/.test(lenHex)) {
    return { kind: 'corrupt' }
  }
  if (!/^[0-9a-f]*$/.test(head)) return { kind: 'corrupt' }
  const n = parseInt(nHex, 16)
  const totalLen = parseInt(lenHex, 16)
  if (!Number.isInteger(n) || n < 1 || n > MAX_CHUNKS) return { kind: 'corrupt' }
  if (!Number.isInteger(totalLen) || totalLen < 0) return { kind: 'corrupt' }
  return { kind: 'v2', n, totalLen, head }
}

/**
 * Reassemble the payload hex from a manifest head + chunk slices, validating
 * the total length committed by the manifest. Returns null if the reassembled
 * hex does not match the committed length, is odd-length, or is not hex — i.e.
 * a torn multi-chunk write degrades to "primary absent" (RFC §4).
 */
export function assemblePayloadHex(
  head: string,
  chunks: string[],
  totalLen: number,
): string | null {
  const hex = head + chunks.join('')
  if (hex.length !== totalLen) return null
  if (hex.length % 2 !== 0) return null
  if (!/^[0-9a-f]*$/.test(hex)) return null
  return hex
}

/**
 * True when `value` is safe to embed in a `security -i` command as `-w
 * "<value>"`: pure hex (a chunk slice) or a `v2:` manifest (hex + colons).
 * Anything else could carry a quote / backslash / newline — writers fail
 * closed rather than emit an unsafe command line.
 */
export function isWriteSafeValue(value: string): boolean {
  return (
    /^[0-9a-f]*$/.test(value) ||
    /^v2:[0-9a-f]+:[0-9a-f]+:[0-9a-f]*$/.test(value)
  )
}
