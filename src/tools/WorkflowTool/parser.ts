import { WORKFLOW_MAX_SOURCE_BYTES } from './constants.js'

const WORKFLOW_NAME_RE = /^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$/

export type WorkflowPhaseMeta = {
  title: string
  detail?: string
}

export type WorkflowMeta = {
  name: string
  description: string
  whenToUse?: string
  phases: WorkflowPhaseMeta[]
}

export type ParsedWorkflowModule = {
  meta: WorkflowMeta
  executableSource: string
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null
}

function validateMeta(value: unknown, filePath: string): WorkflowMeta {
  const record = asRecord(value)
  if (!record) {
    throw new Error(`${filePath}: workflow meta must be an object`)
  }
  const name = typeof record.name === 'string' ? record.name.trim() : ''
  if (!WORKFLOW_NAME_RE.test(name)) {
    throw new Error(
      `${filePath}: workflow meta.name must match ${WORKFLOW_NAME_RE}`,
    )
  }
  const description =
    typeof record.description === 'string'
      ? record.description.trim()
      : ''
  if (!description) {
    throw new Error(`${filePath}: workflow meta.description is required`)
  }
  const whenToUse =
    typeof record.whenToUse === 'string' && record.whenToUse.trim()
      ? record.whenToUse.trim()
      : undefined
  const phasesValue = record.phases
  const phases: WorkflowPhaseMeta[] = []
  if (phasesValue !== undefined) {
    if (!Array.isArray(phasesValue)) {
      throw new Error(`${filePath}: workflow meta.phases must be an array`)
    }
    for (const [index, phaseValue] of phasesValue.entries()) {
      const phase = asRecord(phaseValue)
      const title =
        phase && typeof phase.title === 'string' ? phase.title.trim() : ''
      if (!title) {
        throw new Error(
          `${filePath}: workflow meta.phases[${index}].title is required`,
        )
      }
      const detail =
        typeof phase?.detail === 'string' && phase.detail.trim()
          ? phase.detail.trim()
          : undefined
      phases.push({ title, ...(detail ? { detail } : {}) })
    }
  }
  return {
    name,
    description,
    ...(whenToUse ? { whenToUse } : {}),
    phases,
  }
}

/**
 * Minimal parser for the data-only metadata subset of JavaScript. Keeping
 * this grammar local avoids evaluating plugin code during discovery and
 * avoids shipping the full TypeScript compiler in the CrabCode executable.
 */
class LiteralReader {
  index = 0

  constructor(
    private readonly source: string,
    private readonly filePath: string,
  ) {}

  skipTrivia(): void {
    while (this.index < this.source.length) {
      const char = this.source[this.index]
      if (char === '\ufeff' || /\s/.test(char ?? '')) {
        this.index++
        continue
      }
      if (char === '/' && this.source[this.index + 1] === '/') {
        this.index += 2
        while (
          this.index < this.source.length &&
          this.source[this.index] !== '\n'
        ) {
          this.index++
        }
        continue
      }
      if (char === '/' && this.source[this.index + 1] === '*') {
        const end = this.source.indexOf('*/', this.index + 2)
        if (end < 0) this.fail('unterminated workflow meta comment')
        this.index = end + 2
        continue
      }
      return
    }
  }

  expectIdentifier(expected: string): void {
    this.skipTrivia()
    const match = /^[A-Za-z_$][A-Za-z0-9_$]*/.exec(
      this.source.slice(this.index),
    )
    if (!match || match[0] !== expected) {
      this.fail(`expected "${expected}"`)
    }
    this.index += match[0].length
  }

  expect(char: string): void {
    this.skipTrivia()
    if (this.source[this.index] !== char) {
      this.fail(`expected "${char}"`)
    }
    this.index++
  }

  parseValue(): unknown {
    this.skipTrivia()
    const char = this.source[this.index]
    if (char === '{') return this.parseObject()
    if (char === '[') return this.parseArray()
    if (char === '"' || char === "'" || char === '`') {
      return this.parseString(char)
    }
    if (char === '-' || (char !== undefined && /[0-9]/.test(char))) {
      return this.parseNumber()
    }
    const identifier = /^[A-Za-z_$][A-Za-z0-9_$]*/.exec(
      this.source.slice(this.index),
    )?.[0]
    if (identifier) {
      this.index += identifier.length
      if (identifier === 'true') return true
      if (identifier === 'false') return false
      if (identifier === 'null') return null
    }
    this.fail('workflow meta must contain data literals only')
  }

  private parseObject(): Record<string, unknown> {
    const value: Record<string, unknown> = {}
    this.index++
    this.skipTrivia()
    if (this.source[this.index] === '}') {
      this.index++
      return value
    }
    while (this.index < this.source.length) {
      this.skipTrivia()
      const char = this.source[this.index]
      let key: string
      if (char === '"' || char === "'" || char === '`') {
        key = this.parseString(char)
      } else {
        const identifier = /^[A-Za-z_$][A-Za-z0-9_$]*/.exec(
          this.source.slice(this.index),
        )?.[0]
        if (!identifier) {
          this.fail('workflow meta object keys must be identifiers or strings')
        }
        key = identifier
        this.index += identifier.length
      }
      if (Object.prototype.hasOwnProperty.call(value, key)) {
        this.fail(`duplicate workflow meta key "${key}"`)
      }
      this.expect(':')
      value[key] = this.parseValue()
      this.skipTrivia()
      if (this.source[this.index] === '}') {
        this.index++
        return value
      }
      this.expect(',')
      this.skipTrivia()
      if (this.source[this.index] === '}') {
        this.index++
        return value
      }
    }
    this.fail('unterminated workflow meta object')
  }

  private parseArray(): unknown[] {
    const value: unknown[] = []
    this.index++
    this.skipTrivia()
    if (this.source[this.index] === ']') {
      this.index++
      return value
    }
    while (this.index < this.source.length) {
      value.push(this.parseValue())
      this.skipTrivia()
      if (this.source[this.index] === ']') {
        this.index++
        return value
      }
      this.expect(',')
      this.skipTrivia()
      if (this.source[this.index] === ']') {
        this.index++
        return value
      }
    }
    this.fail('unterminated workflow meta array')
  }

  private parseNumber(): number {
    const match =
      /^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/.exec(
        this.source.slice(this.index),
      )
    if (!match) this.fail('invalid workflow meta number')
    this.index += match[0].length
    const value = Number(match[0])
    if (!Number.isFinite(value)) {
      this.fail('workflow meta numbers must be finite')
    }
    return value
  }

  private parseString(quote: '"' | "'" | '`'): string {
    this.index++
    let value = ''
    while (this.index < this.source.length) {
      const char = this.source[this.index++]
      if (char === quote) return value
      if (quote === '`' && char === '$' && this.source[this.index] === '{') {
        this.fail('workflow meta template substitutions are not allowed')
      }
      if (char === '\\') {
        value += this.parseEscape()
        continue
      }
      if (
        quote !== '`' &&
        (char === '\n' || char === '\r' || char === undefined)
      ) {
        this.fail('unterminated workflow meta string')
      }
      value += char
    }
    this.fail('unterminated workflow meta string')
  }

  private parseEscape(): string {
    const escaped = this.source[this.index++]
    if (escaped === undefined) this.fail('unterminated workflow meta escape')
    if (escaped === '\n') return ''
    if (escaped === '\r') {
      if (this.source[this.index] === '\n') this.index++
      return ''
    }
    const simple: Record<string, string> = {
      b: '\b',
      f: '\f',
      n: '\n',
      r: '\r',
      t: '\t',
      v: '\v',
      '0': '\0',
      '\\': '\\',
      '"': '"',
      "'": "'",
      '`': '`',
      $: '$',
    }
    if (Object.prototype.hasOwnProperty.call(simple, escaped)) {
      return simple[escaped]!
    }
    if (escaped === 'x') {
      return String.fromCodePoint(this.readHex(2))
    }
    if (escaped === 'u') {
      if (this.source[this.index] === '{') {
        const close = this.source.indexOf('}', this.index + 1)
        const raw =
          close >= 0 ? this.source.slice(this.index + 1, close) : ''
        if (!/^[0-9A-Fa-f]{1,6}$/.test(raw)) {
          this.fail('invalid workflow meta unicode escape')
        }
        this.index = close + 1
        const codePoint = Number.parseInt(raw, 16)
        if (codePoint > 0x10ffff) {
          this.fail('invalid workflow meta unicode code point')
        }
        return String.fromCodePoint(codePoint)
      }
      return String.fromCharCode(this.readHex(4))
    }
    // JavaScript treats an unknown non-octal escape as the escaped character.
    return escaped
  }

  private readHex(length: number): number {
    const raw = this.source.slice(this.index, this.index + length)
    if (
      raw.length !== length ||
      !new RegExp(`^[0-9A-Fa-f]{${length}}$`).test(raw)
    ) {
      this.fail('invalid workflow meta hexadecimal escape')
    }
    this.index += length
    return Number.parseInt(raw, 16)
  }

  private fail(message: string): never {
    throw new Error(`${this.filePath}: ${message} at offset ${this.index}`)
  }
}

export function parseWorkflowModule(
  source: string,
  filePath = '<workflow>',
): ParsedWorkflowModule {
  const sourceBytes = Buffer.byteLength(source, 'utf8')
  if (sourceBytes > WORKFLOW_MAX_SOURCE_BYTES) {
    throw new Error(
      `${filePath}: workflow source is ${sourceBytes} UTF-8 bytes; maximum is ${WORKFLOW_MAX_SOURCE_BYTES}`,
    )
  }
  const reader = new LiteralReader(source, filePath)
  reader.skipTrivia()
  const exportStart = reader.index
  reader.expectIdentifier('export')
  const exportEnd = reader.index
  reader.expectIdentifier('const')
  reader.expectIdentifier('meta')
  reader.expect('=')
  const rawMeta = reader.parseValue()
  const meta = validateMeta(rawMeta, filePath)
  const executableSource =
    source.slice(0, exportStart) +
    source.slice(exportEnd)
  return { meta, executableSource }
}
