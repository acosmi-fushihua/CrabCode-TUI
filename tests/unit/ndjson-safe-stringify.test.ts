import { describe, expect, test } from 'bun:test'

import { ndjsonSafeStringify } from '../../src/cli/ndjsonSafeStringify.js'

const HIGH_SURROGATE = String.fromCharCode(0xd800)
const LOW_SURROGATE = String.fromCharCode(0xdc00)
const REPLACEMENT_CHARACTER = '\ufffd'

describe('ndjsonSafeStringify Unicode projection', () => {
  test('projects lone surrogates in scalar, array, nested, and key strings', () => {
    const highKey = `high-${HIGH_SURROGATE}-key`
    const lowKey = `low-${LOW_SURROGATE}-key`
    const input = {
      scalar: `before-${HIGH_SURROGATE}-middle-${LOW_SURROGATE}-after`,
      array: [HIGH_SURROGATE, { [lowKey]: LOW_SURROGATE }],
      nested: {
        [highKey]: {
          value: `nested-${HIGH_SURROGATE}`,
        },
      },
    }

    const encoded = ndjsonSafeStringify(input)
    const decoded = JSON.parse(encoded) as Record<string, unknown>

    expect(decoded).toEqual({
      scalar: `before-${REPLACEMENT_CHARACTER}-middle-${REPLACEMENT_CHARACTER}-after`,
      array: [
        REPLACEMENT_CHARACTER,
        {
          [`low-${REPLACEMENT_CHARACTER}-key`]: REPLACEMENT_CHARACTER,
        },
      ],
      nested: {
        [`high-${REPLACEMENT_CHARACTER}-key`]: {
          value: `nested-${REPLACEMENT_CHARACTER}`,
        },
      },
    })
    expectAllStringsWellFormed(decoded)
  })

  test('preserves valid pairs, emoji, and literal backslash-u text', () => {
    const literalHighEscape = String.raw`\ud800`
    const literalLowEscape = String.raw`\udfff`
    const literalPairEscape = String.raw`\ud83d\ude00`
    const literalKey = String.raw`key-\ud800`
    const value = {
      emoji: '😀',
      explicitPair: `${String.fromCharCode(0xd83d)}${String.fromCharCode(0xde00)}`,
      literalHighEscape,
      literalLowEscape,
      literalPairEscape,
      [literalKey]: literalHighEscape,
    }

    expect(JSON.parse(ndjsonSafeStringify(value))).toEqual(value)
  })

  test('fails closed when projection would alias keys in one object scope', () => {
    expect(() =>
      ndjsonSafeStringify({
        [HIGH_SURROGATE]: 'lone surrogate key',
        [REPLACEMENT_CHARACTER]: 'replacement-character key',
      }),
    ).toThrow('NDJSON Unicode projection produced duplicate object keys')
  })

  test('keeps projected keys independent across nested object scopes', () => {
    const encoded = ndjsonSafeStringify({
      left: { [HIGH_SURROGATE]: 'left' },
      right: { [REPLACEMENT_CHARACTER]: 'right' },
    })

    expect(JSON.parse(encoded)).toEqual({
      left: { [REPLACEMENT_CHARACTER]: 'left' },
      right: { [REPLACEMENT_CHARACTER]: 'right' },
    })
  })

  test('allows repeated projected values and keys in separate array elements', () => {
    const encoded = ndjsonSafeStringify({
      repeated: [HIGH_SURROGATE, HIGH_SURROGATE],
      objects: [
        { [HIGH_SURROGATE]: HIGH_SURROGATE },
        { [HIGH_SURROGATE]: HIGH_SURROGATE },
      ],
    })

    expect(JSON.parse(encoded)).toEqual({
      repeated: [REPLACEMENT_CHARACTER, REPLACEMENT_CHARACTER],
      objects: [
        { [REPLACEMENT_CHARACTER]: REPLACEMENT_CHARACTER },
        { [REPLACEMENT_CHARACTER]: REPLACEMENT_CHARACTER },
      ],
    })
  })

  test('stringifies toJSON, getters, undefined, and array holes exactly once', () => {
    let toJsonCalls = 0
    let getterCalls = 0
    const input = {
      toJSON() {
        toJsonCalls++
        return {
          get projected() {
            getterCalls++
            return HIGH_SURROGATE
          },
          omitted: undefined,
          array: [HIGH_SURROGATE, , undefined],
        }
      },
    }

    expect(JSON.parse(ndjsonSafeStringify(input))).toEqual({
      projected: REPLACEMENT_CHARACTER,
      array: [REPLACEMENT_CHARACTER, null, null],
    })
    expect(toJsonCalls).toBe(1)
    expect(getterCalls).toBe(1)
  })

  test('retains NDJSON line-separator escaping after Unicode projection', () => {
    const value = {
      text: `before\u2028middle\u2029after-${HIGH_SURROGATE}`,
    }

    const encoded = ndjsonSafeStringify(value)

    expect(encoded).toContain('\\u2028')
    expect(encoded).toContain('\\u2029')
    expect(encoded).not.toContain('\u2028')
    expect(encoded).not.toContain('\u2029')
    expect(JSON.parse(encoded)).toEqual({
      text: `before\u2028middle\u2029after-${REPLACEMENT_CHARACTER}`,
    })
  })
})

function expectAllStringsWellFormed(value: unknown): void {
  if (typeof value === 'string') {
    expect(hasLoneSurrogate(value)).toBe(false)
    return
  }

  if (Array.isArray(value)) {
    for (const item of value) {
      expectAllStringsWellFormed(item)
    }
    return
  }

  if (typeof value === 'object' && value !== null) {
    for (const [key, item] of Object.entries(value)) {
      expect(hasLoneSurrogate(key)).toBe(false)
      expectAllStringsWellFormed(item)
    }
  }
}

function hasLoneSurrogate(value: string): boolean {
  for (let index = 0; index < value.length; index++) {
    const codeUnit = value.charCodeAt(index)
    if (codeUnit >= 0xd800 && codeUnit <= 0xdbff) {
      const followingCodeUnit = value.charCodeAt(index + 1)
      if (followingCodeUnit < 0xdc00 || followingCodeUnit > 0xdfff) {
        return true
      }
      index++
    } else if (codeUnit >= 0xdc00 && codeUnit <= 0xdfff) {
      return true
    }
  }
  return false
}
