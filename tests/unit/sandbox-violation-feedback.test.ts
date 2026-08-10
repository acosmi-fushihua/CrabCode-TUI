import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'

import {
  MAX_DENIAL_LINE_CHARS,
  SANDBOX_VIOLATIONS_CLOSE_TAG,
  SANDBOX_VIOLATIONS_OPEN_TAG,
  classifySandboxDenialLine,
  collectSandboxDenials,
  filterIgnoredDenials,
  formatSandboxViolationAnnotation,
} from '../../src/utils/sandbox/violationPatterns.js'
describe('sandbox denial feedback', () => {
  test('keeps the TypeScript source free of literal NUL bytes', () => {
    const source = readFileSync(
      new URL('../../src/utils/sandbox/violationPatterns.ts', import.meta.url),
      'utf8',
    )
    expect(source).not.toContain('\u0000')
    expect(source).toContain('`${kind}\\0${truncated}`')
  })

  test('recognizes isolation evidence without classifying ordinary failures', () => {
    expect(
      classifySandboxDenialLine(
        "touch: cannot touch '/etc/x': Operation not permitted",
      ),
    ).toBe('operation-not-permitted')
    expect(
      classifySandboxDenialLine(
        "cp: cannot create regular file '/etc/x': Permission denied",
      ),
    ).toBe('permission-denied')
    expect(classifySandboxDenialLine('connect: Network is unreachable')).toBe(
      'network-unreachable',
    )
    expect(
      classifySandboxDenialLine(
        "ls: cannot access 'missing': No such file or directory",
      ),
    ).toBeNull()
    expect(
      classifySandboxDenialLine(
        'curl: (7) Failed to connect to localhost: Connection refused',
      ),
    ).toBeNull()
  })

  test('deduplicates repeated lines and bounds adversarially long output', () => {
    const repeated = Array.from(
      { length: 300 },
      () => "cp: cannot create '/etc/x': Permission denied",
    ).join('\n')
    expect(collectSandboxDenials(repeated)).toHaveLength(1)

    const [long] = collectSandboxDenials(
      `${'x'.repeat(5_000)}: Permission denied`,
    )
    expect(long).toBeDefined()
    expect(long!.line.length).toBeLessThanOrEqual(MAX_DENIAL_LINE_CHARS + 1)
  })

  test('ignore rules are type-scoped, case-insensitive and fail open on bad shape', () => {
    const denials = collectSandboxDenials(
      [
        "cp: cannot create '/etc/hosts': Permission denied",
        'curl: Network is unreachable',
      ].join('\n'),
    )

    expect(filterIgnoredDenials(denials, null)).toEqual(denials)
    expect(
      filterIgnoredDenials(denials, {
        'permission-denied': ['/ETC/HOSTS'],
      }).map(item => item.kind),
    ).toEqual(['network-unreachable'])
    expect(filterIgnoredDenials(denials, { '*': ['*'] })).toEqual([])
  })

  test('annotation names the local backend and gives a TUI recovery path', () => {
    const denials = collectSandboxDenials(
      "touch: cannot touch '/etc/x': Operation not permitted",
    )
    const annotation = formatSandboxViolationAnnotation({
      denials,
      backend: 'sandbox-exec-darwin',
      allowUnsandboxedCommands: true,
      command: 'touch /etc/x',
    })

    expect(annotation.startsWith(SANDBOX_VIOLATIONS_OPEN_TAG)).toBe(true)
    expect(annotation.endsWith(SANDBOX_VIOLATIONS_CLOSE_TAG)).toBe(true)
    expect(annotation).toContain('sandbox-exec-darwin')
    expect(annotation).toContain('NOT bugs in the command')
    expect(annotation).toContain('via /sandbox')
    expect(annotation).toContain('last resort, not a first response')
  })

  test('strict annotations never suggest an isolation bypass', () => {
    const annotation = formatSandboxViolationAnnotation({
      denials: collectSandboxDenials('open: Operation not permitted'),
      backend: 'sandbox-exec-linux',
      allowUnsandboxedCommands: false,
      command: 'open file',
    })
    expect(annotation).toContain('disabled by policy')
    expect(annotation).not.toContain('last resort')
  })
})
