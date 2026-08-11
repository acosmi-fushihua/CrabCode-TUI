import { jsonParse, jsonStringify } from '../utils/slowOperations.js'

// JSON.stringify emits U+2028/U+2029 raw (valid per ECMA-404). When the
// output is a single NDJSON line, any receiver that uses JavaScript
// line-terminator semantics (ECMA-262 §11.3 — \n \r U+2028 U+2029) to
// split the stream will cut the JSON mid-string. ProcessTransport now
// silently skips non-JSON lines rather than crashing (gh-28405), but
// the truncated fragment is still lost — the message is silently dropped.
//
// The \uXXXX form is equivalent JSON (parses to the same string) but
// can never be mistaken for a line terminator by ANY receiver. This is
// what ES2019's "Subsume JSON" proposal and Node's util.inspect do.
//
// Single regex with alternation: the callback's one dispatch per match
// is cheaper than two full-string scans.
const JS_LINE_TERMINATORS = /\u2028|\u2029/g

const REPLACEMENT_CHARACTER_ESCAPE = '\\ufffd'
const DUPLICATE_PROJECTED_KEY_ERROR =
  'NDJSON Unicode projection produced duplicate object keys'
const INVALID_PROJECTED_JSON_ERROR =
  'NDJSON Unicode projection produced invalid JSON'

function isHexDigit(code: number): boolean {
  return (
    (code >= 0x30 && code <= 0x39) ||
    (code >= 0x41 && code <= 0x46) ||
    (code >= 0x61 && code <= 0x66)
  )
}

function parseUnicodeEscape(json: string, index: number): number | undefined {
  if (
    json.charCodeAt(index) !== 0x5c ||
    json.charCodeAt(index + 1) !== 0x75
  ) {
    return undefined
  }

  for (let offset = 2; offset < 6; offset++) {
    if (!isHexDigit(json.charCodeAt(index + offset))) {
      return undefined
    }
  }

  return Number.parseInt(json.slice(index + 2, index + 6), 16)
}

/**
 * Reject key aliasing introduced by lone-surrogate projection.
 *
 * This parses only the already-stringified JSON text. It deliberately does
 * not walk the caller's value, so JSON.stringify remains the sole authority
 * for toJSON, getters, undefined values, and array holes. Keys are decoded per
 * object scope so an escaped `\\ufffd` and a literal U+FFFD compare equal,
 * while equal keys in separate nested objects remain independent.
 */
function assertNoDuplicateProjectedObjectKeys(json: string): void {
  let index = 0

  const failInvalidJson = (): never => {
    throw new TypeError(INVALID_PROJECTED_JSON_ERROR)
  }

  const skipWhitespace = (): void => {
    while (index < json.length) {
      const codeUnit = json.charCodeAt(index)
      if (
        codeUnit !== 0x20 &&
        codeUnit !== 0x09 &&
        codeUnit !== 0x0a &&
        codeUnit !== 0x0d
      ) {
        return
      }
      index++
    }
  }

  const parseString = (decode: boolean): string | undefined => {
    if (json.charCodeAt(index) !== 0x22) failInvalidJson()
    const start = index
    index++
    while (index < json.length) {
      const codeUnit = json.charCodeAt(index)
      if (codeUnit === 0x22) {
        index++
        return decode
          ? (jsonParse(json.slice(start, index)) as string)
          : undefined
      }
      if (codeUnit === 0x5c) {
        index += 2
      } else {
        index++
      }
    }
    return failInvalidJson()
  }

  const parseValue = (): void => {
    skipWhitespace()
    const codeUnit = json.charCodeAt(index)
    if (codeUnit === 0x22) {
      parseString(false)
      return
    }
    if (codeUnit === 0x7b) {
      parseObject()
      return
    }
    if (codeUnit === 0x5b) {
      parseArray()
      return
    }

    const start = index
    while (index < json.length) {
      const current = json.charCodeAt(index)
      if (
        current === 0x2c ||
        current === 0x5d ||
        current === 0x7d ||
        current === 0x20 ||
        current === 0x09 ||
        current === 0x0a ||
        current === 0x0d
      ) {
        break
      }
      index++
    }
    if (index === start) failInvalidJson()
  }

  const parseObject = (): void => {
    index++
    skipWhitespace()
    if (json.charCodeAt(index) === 0x7d) {
      index++
      return
    }

    const keys = new Set<string>()
    for (;;) {
      skipWhitespace()
      const key = parseString(true) ?? failInvalidJson()
      if (keys.has(key)) {
        throw new TypeError(DUPLICATE_PROJECTED_KEY_ERROR)
      }
      keys.add(key)

      skipWhitespace()
      if (json.charCodeAt(index) !== 0x3a) failInvalidJson()
      index++
      parseValue()
      skipWhitespace()
      const delimiter = json.charCodeAt(index)
      index++
      if (delimiter === 0x7d) return
      if (delimiter !== 0x2c) failInvalidJson()
    }
  }

  const parseArray = (): void => {
    index++
    skipWhitespace()
    if (json.charCodeAt(index) === 0x5d) {
      index++
      return
    }

    for (;;) {
      parseValue()
      skipWhitespace()
      const delimiter = json.charCodeAt(index)
      index++
      if (delimiter === 0x5d) return
      if (delimiter !== 0x2c) failInvalidJson()
    }
  }

  parseValue()
  skipWhitespace()
  if (index !== json.length) failInvalidJson()
}

/**
 * Make every JSON string token a Unicode scalar-value sequence.
 *
 * Well-formed JSON.stringify deliberately serializes lone UTF-16 surrogates
 * as `\\uXXXX`. JavaScript can parse those escapes back into a lone code unit,
 * but serde_json rejects them. Rewriting the completed JSON instead of cloning
 * the input preserves JSON.stringify semantics (toJSON, getters, array holes,
 * undefined values) and covers both string values and object keys.
 *
 * The scanner understands JSON escapes, so a literal six-character `\\ud800`
 * remains untouched. A valid high+low pair is also retained exactly.
 */
function projectJsonStringsToWellFormedUnicode(json: string): string {
  let inString = false
  let segmentStart = 0
  let projectedParts: string[] | undefined

  const replaceRange = (start: number, end: number): void => {
    projectedParts ??= []
    projectedParts.push(
      json.slice(segmentStart, start),
      REPLACEMENT_CHARACTER_ESCAPE,
    )
    segmentStart = end
  }

  for (let index = 0; index < json.length; index++) {
    const codeUnit = json.charCodeAt(index)

    if (!inString) {
      if (codeUnit === 0x22) {
        inString = true
      }
      continue
    }

    if (codeUnit === 0x22) {
      inString = false
      continue
    }

    if (codeUnit === 0x5c) {
      const escapedCodeUnit = parseUnicodeEscape(json, index)
      if (escapedCodeUnit === undefined) {
        // JSON.stringify only emits two-character escapes here. Copy both so
        // the second backslash in `\\\\ud800` is never mistaken for the start
        // of a Unicode escape.
        index++
        continue
      }

      if (escapedCodeUnit >= 0xd800 && escapedCodeUnit <= 0xdbff) {
        const followingCodeUnit = parseUnicodeEscape(json, index + 6)
        if (
          followingCodeUnit !== undefined &&
          followingCodeUnit >= 0xdc00 &&
          followingCodeUnit <= 0xdfff
        ) {
          index += 11
          continue
        }

        replaceRange(index, index + 6)
        index += 5
        continue
      }

      if (escapedCodeUnit >= 0xdc00 && escapedCodeUnit <= 0xdfff) {
        replaceRange(index, index + 6)
        index += 5
        continue
      }

      index += 5
      continue
    }

    // Modern JSON.stringify escapes lone surrogates, but handle raw code units
    // defensively for older/custom runtimes while preserving valid pairs.
    if (codeUnit >= 0xd800 && codeUnit <= 0xdbff) {
      const followingCodeUnit = json.charCodeAt(index + 1)
      if (followingCodeUnit >= 0xdc00 && followingCodeUnit <= 0xdfff) {
        index++
      } else {
        replaceRange(index, index + 1)
      }
      continue
    }

    if (codeUnit >= 0xdc00 && codeUnit <= 0xdfff) {
      replaceRange(index, index + 1)
      continue
    }
  }

  if (!projectedParts) {
    return json
  }
  projectedParts.push(json.slice(segmentStart))
  const projected = projectedParts.join('')
  assertNoDuplicateProjectedObjectKeys(projected)
  return projected
}

function escapeJsLineTerminators(json: string): string {
  return json.replace(JS_LINE_TERMINATORS, c =>
    c === '\u2028' ? '\\u2028' : '\\u2029',
  )
}

/**
 * JSON.stringify for one-message-per-line transports. Projects lone UTF-16
 * surrogates in all string values and object keys to U+FFFD for strict UTF-8
 * JSON decoders, then escapes U+2028 LINE SEPARATOR and U+2029 PARAGRAPH
 * SEPARATOR so line-splitting receivers cannot split a message mid-string.
 */
export function ndjsonSafeStringify(value: unknown): string {
  return escapeJsLineTerminators(
    projectJsonStringsToWellFormedUnicode(jsonStringify(value)),
  )
}
