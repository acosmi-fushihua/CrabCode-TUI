import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import {
  isDirectTuiBashContentBlocks,
  routeDirectTuiInput,
} from '../../src/cli/directTuiInput.js'
import type { ContentBlockParam } from '../../src/types/api-types.js'

const ROOT = resolve(import.meta.dir, '../..')

describe('native direct TUI bash input routing', () => {
  test('does not turn the fixed standalone mode marker into an empty bash submission', () => {
    expect(routeDirectTuiInput('!')).toBeNull()
    expect(routeDirectTuiInput('! \t')).toBeNull()
    expect(
      routeDirectTuiInput([{ type: 'text', text: '!' }]),
    ).toBeNull()
  })

  test('removes exactly one leading mode character', () => {
    expect(routeDirectTuiInput("!printf 'ok'")).toEqual({
      mode: 'bash',
      value: "printf 'ok'",
    })
    expect(routeDirectTuiInput('!!literal-command')).toEqual({
      mode: 'bash',
      value: '!literal-command',
    })
  })

  test('does not trim or infer a mode from a non-leading exclamation mark', () => {
    const leadingSpace = ' !printf must-stay-a-prompt'
    const embedded = 'explain !important'

    expect(routeDirectTuiInput(leadingSpace)).toEqual({
      mode: 'prompt',
      value: leadingSpace,
    })
    expect(routeDirectTuiInput(embedded)).toEqual({
      mode: 'prompt',
      value: embedded,
    })
  })

  test('preserves native image blocks and places the stripped command last', () => {
    const image: ContentBlockParam = {
      type: 'image',
      source: {
        type: 'base64',
        media_type: 'image/png',
        data: 'AA==',
      },
    }
    const input: ContentBlockParam[] = [
      { type: 'text', text: '!file image.png' },
      image,
    ]

    expect(routeDirectTuiInput(input)).toEqual({
      mode: 'bash',
      value: [image, { type: 'text', text: 'file image.png' }],
    })
    const routed = routeDirectTuiInput(input)
    expect(routed?.mode).toBe('bash')
    if (routed?.mode !== 'bash' || typeof routed.value === 'string') {
      throw new Error('expected direct bash content blocks')
    }
    expect(isDirectTuiBashContentBlocks(routed.value)).toBe(true)
  })

  test('injects content-block permission only from the explicit direct route policy', () => {
    const queryEngine = readFileSync(
      resolve(ROOT, 'src/QueryEngine.ts'),
      'utf8',
    )
    const executionCore = readFileSync(
      resolve(ROOT, 'src/cli/print/queryExecutionCore.ts'),
      'utf8',
    )

    expect(queryEngine).toContain(
      'this.config.allowDirectTuiBashContentBlocks === true',
    )
    expect(queryEngine).toContain(
      'isDirectTuiBashContentBlocks(prompt)',
    )
    expect(executionCore).toContain(
      'allowDirectTuiBashContentBlocks: true',
    )
    expect(executionCore).toContain(
      'allowDirectTuiBashContentBlocks: false',
    )
    expect(
      executionCore.match(/allowDirectTuiBashContentBlocks: true/g),
    ).toHaveLength(1)
  })

  test('places an attachment-bearing runtime slash command last for the unchanged parser', () => {
    const image: ContentBlockParam = {
      type: 'image',
      source: {
        type: 'base64',
        media_type: 'image/png',
        data: 'AA==',
      },
    }
    const input: ContentBlockParam[] = [
      { type: 'text', text: '/compact' },
      image,
    ]

    expect(routeDirectTuiInput(input)).toEqual({
      mode: 'prompt',
      value: [image, { type: 'text', text: '/compact' }],
    })
  })

  test('does not reorder an ordinary attachment-bearing prompt', () => {
    const input: ContentBlockParam[] = [
      { type: 'text', text: 'describe this image' },
      {
        type: 'image',
        source: {
          type: 'base64',
          media_type: 'image/png',
          data: 'AA==',
        },
      },
    ]

    const routed = routeDirectTuiInput(input)
    expect(routed.mode).toBe('prompt')
    expect(routed.value).toBe(input)
  })

  test('fails closed to prompt mode for ambiguous multi-text payloads', () => {
    const input: ContentBlockParam[] = [
      { type: 'text', text: '!first' },
      { type: 'text', text: 'second' },
    ]

    const routed = routeDirectTuiInput(input)
    expect(routed.mode).toBe('prompt')
    expect(routed.value).toBe(input)
  })
})
