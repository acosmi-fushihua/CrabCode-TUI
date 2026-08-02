/**
 * Shared types, constants, lexer, and core utilities for the bash parser.
 *
 * Every other sub-module imports from this file. This file has zero
 * intra-package dependencies.
 */

// ───────────────────────────── AST Node ─────────────────────────────

export type TsNode = {
  type: string
  text: string
  startIndex: number
  endIndex: number
  children: TsNode[]
}

export type ParserModule = {
  parse: (source: string, timeoutMs?: number) => TsNode | null
}

// ───────────────────────────── Constants ─────────────────────────────

/**
 * 50ms wall-clock cap — bails out on pathological/adversarial input.
 * Pass `Infinity` via `parse(src, Infinity)` to disable (e.g. correctness
 * tests, where CI jitter would otherwise cause spurious null returns).
 */
export const PARSE_TIMEOUT_MS = 50

/** Node budget cap — bails out before OOM on deeply nested input. */
export const MAX_NODES = 50_000

export const SPECIAL_VARS = new Set(['?', '$', '@', '*', '#', '-', '!', '_'])

export const DECL_KEYWORDS = new Set([
  'export',
  'declare',
  'typeset',
  'readonly',
  'local',
])

export const SHELL_KEYWORDS = new Set([
  'if',
  'then',
  'elif',
  'else',
  'fi',
  'while',
  'until',
  'for',
  'in',
  'do',
  'done',
  'case',
  'esac',
  'function',
  'select',
])

/** Operator precedence table (higher = tighter binding). */
export const ARITH_PREC: Record<string, number> = {
  '=': 2,
  '+=': 2,
  '-=': 2,
  '*=': 2,
  '/=': 2,
  '%=': 2,
  '<<=': 2,
  '>>=': 2,
  '&=': 2,
  '^=': 2,
  '|=': 2,
  '||': 4,
  '&&': 5,
  '|': 6,
  '^': 7,
  '&': 8,
  '==': 9,
  '!=': 9,
  '<': 10,
  '>': 10,
  '<=': 10,
  '>=': 10,
  '<<': 11,
  '>>': 11,
  '+': 12,
  '-': 12,
  '*': 13,
  '/': 13,
  '%': 13,
  '**': 14,
}

/** Right-associative operators (assignment and exponent). */
export const ARITH_RIGHT_ASSOC = new Set([
  '=',
  '+=',
  '-=',
  '*=',
  '/=',
  '%=',
  '<<=',
  '>>=',
  '&=',
  '^=',
  '|=',
  '**',
])

// ───────────────────────────── Tokenizer Types ─────────────────────────────

export type TokenType =
  | 'WORD'
  | 'NUMBER'
  | 'OP'
  | 'NEWLINE'
  | 'COMMENT'
  | 'DQUOTE'
  | 'SQUOTE'
  | 'ANSI_C'
  | 'DOLLAR'
  | 'DOLLAR_PAREN'
  | 'DOLLAR_BRACE'
  | 'DOLLAR_DPAREN'
  | 'BACKTICK'
  | 'LT_PAREN'
  | 'GT_PAREN'
  | 'EOF'

export type Token = {
  type: TokenType
  value: string
  /** UTF-8 byte offset of first char */
  start: number
  /** UTF-8 byte offset one past last char */
  end: number
}

/**
 * Lexer state. Tracks both JS-string index (for charAt) and UTF-8 byte offset
 * (for TsNode positions). ASCII fast path: byte == char index. Non-ASCII
 * advances byte count per-codepoint.
 */
export type Lexer = {
  src: string
  len: number
  /** JS string index */
  i: number
  /** UTF-8 byte offset */
  b: number
  /** Pending heredoc delimiters awaiting body scan at next newline */
  heredocs: HeredocPending[]
  /** Precomputed byte offset for each char index (lazy for non-ASCII) */
  byteTable: Uint32Array | null
}

export type HeredocPending = {
  delim: string
  stripTabs: boolean
  quoted: boolean
  /** Filled after body scan */
  bodyStart: number
  bodyEnd: number
  endStart: number
  endEnd: number
}

// ───────────────────────────── Parser Types ─────────────────────────────

export type ParseState = {
  L: Lexer
  src: string
  srcBytes: number
  /** True when byte offsets == char indices (no multi-byte UTF-8) */
  isAscii: boolean
  nodeCount: number
  deadline: number
  aborted: boolean
  /** Depth of backtick nesting — inside `...`, ` terminates words */
  inBacktick: number
  /** When set, parseSimpleCommand stops at this token (for `[` backtrack) */
  stopToken: string | null
}

/** Packed as (b << 16) | i — avoids heap alloc on every backtrack. */
export type LexSave = number

/**
 * Arithmetic context modes:
 * - 'var': bare identifiers → variable_name (default, used in $((..)), ((..)))
 * - 'word': bare identifiers → word (c-style for head condition/update clauses)
 * - 'assign': identifiers with = → variable_assignment (c-style for init clause)
 */
export type ArithMode = 'var' | 'word' | 'assign'

// ───────────────────────────── Lexer Functions ─────────────────────────────

export function makeLexer(src: string): Lexer {
  return {
    src,
    len: src.length,
    i: 0,
    b: 0,
    heredocs: [],
    byteTable: null,
  }
}

/** Advance one JS char, updating byte offset for UTF-8. */
export function advance(L: Lexer): void {
  const c = L.src.charCodeAt(L.i)
  L.i++
  if (c < 0x80) {
    L.b++
  } else if (c < 0x800) {
    L.b += 2
  } else if (c >= 0xd800 && c <= 0xdbff) {
    // High surrogate — next char completes the pair, total 4 UTF-8 bytes
    L.b += 4
    L.i++
  } else {
    L.b += 3
  }
}

export function peek(L: Lexer, off = 0): string {
  return L.i + off < L.len ? L.src[L.i + off]! : ''
}

export function byteAt(L: Lexer, charIdx: number): number {
  // Fast path: ASCII-only prefix means char idx == byte idx
  if (L.byteTable) return L.byteTable[charIdx]!
  // Build table on first non-trivial lookup
  const t = new Uint32Array(L.len + 1)
  let b = 0
  let i = 0
  while (i < L.len) {
    t[i] = b
    const c = L.src.charCodeAt(i)
    if (c < 0x80) {
      b++
      i++
    } else if (c < 0x800) {
      b += 2
      i++
    } else if (c >= 0xd800 && c <= 0xdbff) {
      t[i + 1] = b + 2
      b += 4
      i += 2
    } else {
      b += 3
      i++
    }
  }
  t[L.len] = b
  L.byteTable = t
  return t[charIdx]!
}

// ───────────────────────────── Character Predicates ─────────────────────────────

export function isWordChar(c: string): boolean {
  // Bash word chars: alphanumeric + various punctuation that doesn't start operators
  return (
    (c >= 'a' && c <= 'z') ||
    (c >= 'A' && c <= 'Z') ||
    (c >= '0' && c <= '9') ||
    c === '_' ||
    c === '/' ||
    c === '.' ||
    c === '-' ||
    c === '+' ||
    c === ':' ||
    c === '@' ||
    c === '%' ||
    c === ',' ||
    c === '~' ||
    c === '^' ||
    c === '?' ||
    c === '*' ||
    c === '!' ||
    c === '=' ||
    c === '[' ||
    c === ']'
  )
}

export function isWordStart(c: string): boolean {
  return isWordChar(c) || c === '\\'
}

export function isIdentStart(c: string): boolean {
  return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || c === '_'
}

export function isIdentChar(c: string): boolean {
  return isIdentStart(c) || (c >= '0' && c <= '9')
}

export function isDigit(c: string): boolean {
  return c >= '0' && c <= '9'
}

export function isHexDigit(c: string): boolean {
  return isDigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F')
}

export function isBaseDigit(c: string): boolean {
  // Bash BASE#DIGITS: digits, letters, @ and _ (up to base 64)
  return isIdentChar(c) || c === '@'
}

/**
 * Unquoted heredoc delimiter chars. Bash accepts most non-metacharacters —
 * not just identifiers. Stop at whitespace, redirects, pipe/list operators,
 * and structural tokens. Allows !, -, ., +, etc. (e.g. <<!HEREDOC!).
 */
export function isHeredocDelimChar(c: string): boolean {
  return (
    c !== '' &&
    c !== ' ' &&
    c !== '\t' &&
    c !== '\n' &&
    c !== '<' &&
    c !== '>' &&
    c !== '|' &&
    c !== '&' &&
    c !== ';' &&
    c !== '(' &&
    c !== ')' &&
    c !== "'" &&
    c !== '"' &&
    c !== '`' &&
    c !== '\\'
  )
}

export function skipBlanks(L: Lexer): void {
  while (L.i < L.len) {
    const c = L.src[L.i]!
    if (c === ' ' || c === '\t' || c === '\r') {
      // \r is whitespace per tree-sitter-bash extras /\s/ — handles CRLF inputs
      advance(L)
    } else if (c === '\\') {
      const nx = L.src[L.i + 1]
      if (nx === '\n' || (nx === '\r' && L.src[L.i + 2] === '\n')) {
        // Line continuation — tree-sitter extras: /\\\r?\n/
        advance(L)
        advance(L)
        if (nx === '\r') advance(L)
      } else if (nx === ' ' || nx === '\t') {
        // \<space> or \<tab> — tree-sitter's _whitespace is /\\?[ \t\v]+/
        advance(L)
        advance(L)
      } else {
        break
      }
    } else {
      break
    }
  }
}

// ───────────────────────────── Tokenizer ─────────────────────────────

/**
 * Scan next token. Context-sensitive: `cmd` mode treats [ as operator (test
 * command start), `arg` mode treats [ as word char (glob/subscript).
 */
export function nextToken(L: Lexer, ctx: 'cmd' | 'arg' = 'arg'): Token {
  skipBlanks(L)
  const start = L.b
  if (L.i >= L.len) return { type: 'EOF', value: '', start, end: start }

  const c = L.src[L.i]!
  const c1 = peek(L, 1)
  const c2 = peek(L, 2)

  if (c === '\n') {
    advance(L)
    return { type: 'NEWLINE', value: '\n', start, end: L.b }
  }

  if (c === '#') {
    const si = L.i
    while (L.i < L.len && L.src[L.i] !== '\n') advance(L)
    return {
      type: 'COMMENT',
      value: L.src.slice(si, L.i),
      start,
      end: L.b,
    }
  }

  // Multi-char operators (longest match first)
  if (c === '&' && c1 === '&') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '&&', start, end: L.b }
  }
  if (c === '|' && c1 === '|') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '||', start, end: L.b }
  }
  if (c === '|' && c1 === '&') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '|&', start, end: L.b }
  }
  if (c === ';' && c1 === ';' && c2 === '&') {
    advance(L)
    advance(L)
    advance(L)
    return { type: 'OP', value: ';;&', start, end: L.b }
  }
  if (c === ';' && c1 === ';') {
    advance(L)
    advance(L)
    return { type: 'OP', value: ';;', start, end: L.b }
  }
  if (c === ';' && c1 === '&') {
    advance(L)
    advance(L)
    return { type: 'OP', value: ';&', start, end: L.b }
  }
  if (c === '>' && c1 === '>') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '>>', start, end: L.b }
  }
  if (c === '>' && c1 === '&' && c2 === '-') {
    advance(L)
    advance(L)
    advance(L)
    return { type: 'OP', value: '>&-', start, end: L.b }
  }
  if (c === '>' && c1 === '&') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '>&', start, end: L.b }
  }
  if (c === '>' && c1 === '|') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '>|', start, end: L.b }
  }
  if (c === '&' && c1 === '>' && c2 === '>') {
    advance(L)
    advance(L)
    advance(L)
    return { type: 'OP', value: '&>>', start, end: L.b }
  }
  if (c === '&' && c1 === '>') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '&>', start, end: L.b }
  }
  if (c === '<' && c1 === '<' && c2 === '<') {
    advance(L)
    advance(L)
    advance(L)
    return { type: 'OP', value: '<<<', start, end: L.b }
  }
  if (c === '<' && c1 === '<' && c2 === '-') {
    advance(L)
    advance(L)
    advance(L)
    return { type: 'OP', value: '<<-', start, end: L.b }
  }
  if (c === '<' && c1 === '<') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '<<', start, end: L.b }
  }
  if (c === '<' && c1 === '&' && c2 === '-') {
    advance(L)
    advance(L)
    advance(L)
    return { type: 'OP', value: '<&-', start, end: L.b }
  }
  if (c === '<' && c1 === '&') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '<&', start, end: L.b }
  }
  if (c === '<' && c1 === '(') {
    advance(L)
    advance(L)
    return { type: 'LT_PAREN', value: '<(', start, end: L.b }
  }
  if (c === '>' && c1 === '(') {
    advance(L)
    advance(L)
    return { type: 'GT_PAREN', value: '>(', start, end: L.b }
  }
  if (c === '(' && c1 === '(') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '((', start, end: L.b }
  }
  if (c === ')' && c1 === ')') {
    advance(L)
    advance(L)
    return { type: 'OP', value: '))', start, end: L.b }
  }

  if (c === '|' || c === '&' || c === ';' || c === '>' || c === '<') {
    advance(L)
    return { type: 'OP', value: c, start, end: L.b }
  }
  if (c === '(' || c === ')') {
    advance(L)
    return { type: 'OP', value: c, start, end: L.b }
  }

  // In cmd position, [ [[ { start test/group; in arg position they're word chars
  if (ctx === 'cmd') {
    if (c === '[' && c1 === '[') {
      advance(L)
      advance(L)
      return { type: 'OP', value: '[[', start, end: L.b }
    }
    if (c === '[') {
      advance(L)
      return { type: 'OP', value: '[', start, end: L.b }
    }
    if (c === '{' && (c1 === ' ' || c1 === '\t' || c1 === '\n')) {
      advance(L)
      return { type: 'OP', value: '{', start, end: L.b }
    }
    if (c === '}') {
      advance(L)
      return { type: 'OP', value: '}', start, end: L.b }
    }
    if (c === '!' && (c1 === ' ' || c1 === '\t')) {
      advance(L)
      return { type: 'OP', value: '!', start, end: L.b }
    }
  }

  if (c === '"') {
    advance(L)
    return { type: 'DQUOTE', value: '"', start, end: L.b }
  }
  if (c === "'") {
    const si = L.i
    advance(L)
    while (L.i < L.len && L.src[L.i] !== "'") advance(L)
    if (L.i < L.len) advance(L)
    return {
      type: 'SQUOTE',
      value: L.src.slice(si, L.i),
      start,
      end: L.b,
    }
  }

  if (c === '$') {
    if (c1 === '(' && c2 === '(') {
      advance(L)
      advance(L)
      advance(L)
      return { type: 'DOLLAR_DPAREN', value: '$((', start, end: L.b }
    }
    if (c1 === '(') {
      advance(L)
      advance(L)
      return { type: 'DOLLAR_PAREN', value: '$(', start, end: L.b }
    }
    if (c1 === '{') {
      advance(L)
      advance(L)
      return { type: 'DOLLAR_BRACE', value: '${', start, end: L.b }
    }
    if (c1 === "'") {
      // ANSI-C string $'...'
      const si = L.i
      advance(L)
      advance(L)
      while (L.i < L.len && L.src[L.i] !== "'") {
        if (L.src[L.i] === '\\' && L.i + 1 < L.len) advance(L)
        advance(L)
      }
      if (L.i < L.len) advance(L)
      return {
        type: 'ANSI_C',
        value: L.src.slice(si, L.i),
        start,
        end: L.b,
      }
    }
    advance(L)
    return { type: 'DOLLAR', value: '$', start, end: L.b }
  }

  if (c === '`') {
    advance(L)
    return { type: 'BACKTICK', value: '`', start, end: L.b }
  }

  // File descriptor before redirect: digit+ immediately followed by > or <
  if (isDigit(c)) {
    let j = L.i
    while (j < L.len && isDigit(L.src[j]!)) j++
    const after = j < L.len ? L.src[j]! : ''
    if (after === '>' || after === '<') {
      const si = L.i
      while (L.i < j) advance(L)
      return {
        type: 'WORD',
        value: L.src.slice(si, L.i),
        start,
        end: L.b,
      }
    }
  }

  // Word / number
  if (isWordStart(c) || c === '{' || c === '}') {
    const si = L.i
    while (L.i < L.len) {
      const ch = L.src[L.i]!
      if (ch === '\\') {
        if (L.i + 1 >= L.len) {
          // Trailing `\` at EOF — tree-sitter excludes it from the word and
          // emits a sibling ERROR. Stop here so the word ends before `\`.
          break
        }
        // Escape next char (including \n for line continuation mid-word)
        if (L.src[L.i + 1] === '\n') {
          advance(L)
          advance(L)
          continue
        }
        advance(L)
        advance(L)
        continue
      }
      if (!isWordChar(ch) && ch !== '{' && ch !== '}') {
        break
      }
      advance(L)
    }
    if (L.i > si) {
      const v = L.src.slice(si, L.i)
      // Number: optional sign then digits only
      if (/^-?\d+$/.test(v)) {
        return { type: 'NUMBER', value: v, start, end: L.b }
      }
      return { type: 'WORD', value: v, start, end: L.b }
    }
    // Empty word (lone `\` at EOF) — fall through to single-char consumer
  }

  // Unknown char — consume as single-char word
  advance(L)
  return { type: 'WORD', value: c, start, end: L.b }
}

// ───────────────────────────── Core Parser Utilities ─────────────────────────────

export function byteLengthUtf8(s: string): number {
  let b = 0
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i)
    if (c < 0x80) b++
    else if (c < 0x800) b += 2
    else if (c >= 0xd800 && c <= 0xdbff) {
      b += 4
      i++
    } else b += 3
  }
  return b
}

export function checkBudget(P: ParseState): void {
  P.nodeCount++
  if (P.nodeCount > MAX_NODES) {
    P.aborted = true
    throw new Error('budget')
  }
  if ((P.nodeCount & 0x7f) === 0 && performance.now() > P.deadline) {
    P.aborted = true
    throw new Error('timeout')
  }
}

/** Build a node. Slices text from source by byte range via char-index lookup. */
export function mk(
  P: ParseState,
  type: string,
  start: number,
  end: number,
  children: TsNode[],
): TsNode {
  checkBudget(P)
  return {
    type,
    text: sliceBytes(P, start, end),
    startIndex: start,
    endIndex: end,
    children,
  }
}

export function sliceBytes(P: ParseState, startByte: number, endByte: number): string {
  if (P.isAscii) return P.src.slice(startByte, endByte)
  // Find char indices for byte offsets. Build byte table if needed.
  const L = P.L
  if (!L.byteTable) byteAt(L, 0)
  const t = L.byteTable!
  // Binary search for char index where byte offset matches
  let lo = 0
  let hi = P.src.length
  while (lo < hi) {
    const m = (lo + hi) >>> 1
    if (t[m]! < startByte) lo = m + 1
    else hi = m
  }
  const sc = lo
  lo = sc
  hi = P.src.length
  while (lo < hi) {
    const m = (lo + hi) >>> 1
    if (t[m]! < endByte) lo = m + 1
    else hi = m
  }
  return P.src.slice(sc, lo)
}

export function leaf(P: ParseState, type: string, tok: Token): TsNode {
  return mk(P, type, tok.start, tok.end, [])
}

export function saveLex(L: Lexer): LexSave {
  return L.b * 0x10000 + L.i
}

export function restoreLex(L: Lexer, s: LexSave): void {
  L.i = s & 0xffff
  L.b = s >>> 16
}

export function restoreLexToByte(P: ParseState, targetByte: number): void {
  if (!P.L.byteTable) byteAt(P.L, 0)
  const t = P.L.byteTable!
  let lo = 0
  let hi = P.src.length
  while (lo < hi) {
    const m = (lo + hi) >>> 1
    if (t[m]! < targetByte) lo = m + 1
    else hi = m
  }
  P.L.i = lo
  P.L.b = targetByte
}

export function skipNewlines(P: ParseState): void {
  while (true) {
    const save = saveLex(P.L)
    const t = nextToken(P.L, 'cmd')
    if (t.type !== 'NEWLINE') {
      restoreLex(P.L, save)
      break
    }
  }
}
