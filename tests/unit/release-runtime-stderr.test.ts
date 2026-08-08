import { describe, expect, test } from 'bun:test'
import {
  bunNoAvxBaselineWarning,
  classifyRuntimeStderr,
} from '../../scripts/release-runtime-stderr.mjs'

const baselineMaterials = {
  schemaVersion: 1,
  product: 'CrabCode TUI',
  platform: 'x64-darwin',
  runtime: {
    bun: {
      version: '1.3.14',
      url: 'https://github.com/oven-sh/bun/releases/download/bun-v1.3.14/bun-darwin-x64-baseline.zip',
      sha256: '3e35ad6f53971a9834bf9e6786e2adf72b5f1921cc9a9c5fde073d2972944076',
    },
  },
}

describe('release runtime stderr classification', () => {
  test('accepts silence for source and packaged runtime smoke', () => {
    expect(classifyRuntimeStderr('', null)).toBe('empty')
  })

  test('accepts only the exact no-AVX warning from the pinned baseline package', () => {
    for (const suffix of ['', '\n', '\r\n']) {
      expect(
        classifyRuntimeStderr(
          `${bunNoAvxBaselineWarning}${suffix}`,
          baselineMaterials,
        ),
      ).toBe('classified-bun-baseline-no-avx-warning')
    }
  })

  test('rejects the warning without complete baseline package authority', () => {
    for (const materials of [
      null,
      { ...baselineMaterials, platform: 'arm64-darwin' },
      {
        ...baselineMaterials,
        runtime: {
          bun: { ...baselineMaterials.runtime.bun, sha256: '0'.repeat(64) },
        },
      },
      {
        ...baselineMaterials,
        runtime: {
          bun: { ...baselineMaterials.runtime.bun, version: '1.3.11' },
        },
      },
      {
        ...baselineMaterials,
        runtime: {
          bun: {
            ...baselineMaterials.runtime.bun,
            url: 'https://example.invalid/bun-darwin-x64-baseline.zip',
          },
        },
      },
    ]) {
      expect(classifyRuntimeStderr(`${bunNoAvxBaselineWarning}\n`, materials)).toBe(
        'unexpected',
      )
    }
  })

  test('rejects all additional or mutated stderr', () => {
    for (const stderr of [
      `prefix\n${bunNoAvxBaselineWarning}\n`,
      `${bunNoAvxBaselineWarning}\nextra\n`,
      `${bunNoAvxBaselineWarning} `,
      'warning: unrelated runtime failure\n',
    ]) {
      expect(classifyRuntimeStderr(stderr, baselineMaterials)).toBe('unexpected')
    }
  })
})
