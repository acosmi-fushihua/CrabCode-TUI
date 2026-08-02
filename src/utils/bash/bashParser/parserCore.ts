/**
 * Parser entry point and top-level parsing: program, statements, and-or lists,
 * pipelines. Also exports the module API (ensureParserInitialized, getParserModule).
 */

import {
  type TsNode,
  type ParserModule,
  type ParseState,
  PARSE_TIMEOUT_MS,
  skipBlanks,
  nextToken,
  mk,
  leaf,
  saveLex,
  restoreLex,
  makeLexer,
  byteLengthUtf8,
  peek,
  skipNewlines,
} from './types.js'
import { parseCommand, scanHeredocBodies } from './commands.js'

const MODULE: ParserModule = { parse: parseSource }

const READY = Promise.resolve()

/** No-op: pure-TS parser needs no async init. Kept for API compatibility. */
export function ensureParserInitialized(): Promise<void> {
  return READY
}

/** Always succeeds — pure-TS needs no init. */
export function getParserModule(): ParserModule | null {
  return MODULE
}

function parseSource(source: string, timeoutMs?: number): TsNode | null {
  const L = makeLexer(source)
  const srcBytes = byteLengthUtf8(source)
  const P: ParseState = {
    L,
    src: source,
    srcBytes,
    isAscii: srcBytes === source.length,
    nodeCount: 0,
    deadline: performance.now() + (timeoutMs ?? PARSE_TIMEOUT_MS),
    aborted: false,
    inBacktick: 0,
    stopToken: null,
  }
  try {
    const program = parseProgram(P)
    if (P.aborted) return null
    return program
  } catch {
    return null
  }
}

function parseProgram(P: ParseState): TsNode {
  const children: TsNode[] = []
  // Skip leading whitespace & newlines — program start is first content byte
  skipBlanks(P.L)
  while (true) {
    const save = saveLex(P.L)
    const t = nextToken(P.L, 'cmd')
    if (t.type === 'NEWLINE') {
      skipBlanks(P.L)
      continue
    }
    restoreLex(P.L, save)
    break
  }
  const progStart = P.L.b
  while (P.L.i < P.L.len) {
    const save = saveLex(P.L)
    const t = nextToken(P.L, 'cmd')
    if (t.type === 'EOF') break
    if (t.type === 'NEWLINE') continue
    if (t.type === 'COMMENT') {
      children.push(leaf(P, 'comment', t))
      continue
    }
    restoreLex(P.L, save)
    const stmts = parseStatements(P, null)
    for (const s of stmts) children.push(s)
    if (stmts.length === 0) {
      // Couldn't parse — emit ERROR and skip one token
      const errTok = nextToken(P.L, 'cmd')
      if (errTok.type === 'EOF') break
      if (
        errTok.type === 'OP' &&
        errTok.value === ';;' &&
        children.length > 0
      ) {
        continue
      }
      children.push(mk(P, 'ERROR', errTok.start, errTok.end, []))
    }
  }
  // tree-sitter includes trailing whitespace in program extent
  const progEnd = children.length > 0 ? P.srcBytes : progStart
  return mk(P, 'program', progStart, progEnd, children)
}

/**
 * Parse a sequence of statements separated by ; & newline. Returns a flat list
 * where ; and & are sibling leaves (NOT wrapped in 'list' — only && || get
 * that). Stops at terminator or EOF.
 */
export function parseStatements(P: ParseState, terminator: string | null): TsNode[] {
  const out: TsNode[] = []
  while (true) {
    skipBlanks(P.L)
    const save = saveLex(P.L)
    const t = nextToken(P.L, 'cmd')
    if (t.type === 'EOF') {
      restoreLex(P.L, save)
      break
    }
    if (t.type === 'NEWLINE') {
      // Process pending heredocs
      if (P.L.heredocs.length > 0) {
        scanHeredocBodies(P)
      }
      continue
    }
    if (t.type === 'COMMENT') {
      out.push(leaf(P, 'comment', t))
      continue
    }
    if (terminator && t.type === 'OP' && t.value === terminator) {
      restoreLex(P.L, save)
      break
    }
    if (
      t.type === 'OP' &&
      (t.value === ')' ||
        t.value === '}' ||
        t.value === ';;' ||
        t.value === ';&' ||
        t.value === ';;&' ||
        t.value === '))' ||
        t.value === ']]' ||
        t.value === ']')
    ) {
      restoreLex(P.L, save)
      break
    }
    if (t.type === 'BACKTICK' && P.inBacktick > 0) {
      restoreLex(P.L, save)
      break
    }
    if (
      t.type === 'WORD' &&
      (t.value === 'then' ||
        t.value === 'elif' ||
        t.value === 'else' ||
        t.value === 'fi' ||
        t.value === 'do' ||
        t.value === 'done' ||
        t.value === 'esac')
    ) {
      restoreLex(P.L, save)
      break
    }
    restoreLex(P.L, save)
    const stmt = parseAndOr(P)
    if (!stmt) break
    out.push(stmt)
    // Look for separator
    skipBlanks(P.L)
    const save2 = saveLex(P.L)
    const sep = nextToken(P.L, 'cmd')
    if (sep.type === 'OP' && (sep.value === ';' || sep.value === '&')) {
      // Check if terminator follows
      const save3 = saveLex(P.L)
      const after = nextToken(P.L, 'cmd')
      restoreLex(P.L, save3)
      out.push(leaf(P, sep.value, sep))
      if (
        after.type === 'EOF' ||
        (after.type === 'OP' &&
          (after.value === ')' ||
            after.value === '}' ||
            after.value === ';;' ||
            after.value === ';&' ||
            after.value === ';;&')) ||
        (after.type === 'WORD' &&
          (after.value === 'then' ||
            after.value === 'elif' ||
            after.value === 'else' ||
            after.value === 'fi' ||
            after.value === 'do' ||
            after.value === 'done' ||
            after.value === 'esac'))
      ) {
        continue
      }
    } else if (sep.type === 'NEWLINE') {
      if (P.L.heredocs.length > 0) {
        scanHeredocBodies(P)
      }
      continue
    } else {
      restoreLex(P.L, save2)
    }
  }
  return out
}

/**
 * Parse pipeline chains joined by && ||. Left-associative nesting.
 */
export function parseAndOr(P: ParseState): TsNode | null {
  let left = parsePipeline(P)
  if (!left) return null
  while (true) {
    const save = saveLex(P.L)
    const t = nextToken(P.L, 'cmd')
    if (t.type === 'OP' && (t.value === '&&' || t.value === '||')) {
      const op = leaf(P, t.value, t)
      skipNewlines(P)
      const right = parsePipeline(P)
      if (!right) {
        left = mk(P, 'list', left.startIndex, op.endIndex, [left, op])
        break
      }
      // If right is a redirected_statement, hoist its redirects to wrap the list.
      if (right.type === 'redirected_statement' && right.children.length >= 2) {
        const inner = right.children[0]!
        const redirs = right.children.slice(1)
        const listNode = mk(P, 'list', left.startIndex, inner.endIndex, [
          left,
          op,
          inner,
        ])
        const lastR = redirs[redirs.length - 1]!
        left = mk(
          P,
          'redirected_statement',
          listNode.startIndex,
          lastR.endIndex,
          [listNode, ...redirs],
        )
      } else {
        left = mk(P, 'list', left.startIndex, right.endIndex, [left, op, right])
      }
    } else {
      restoreLex(P.L, save)
      break
    }
  }
  return left
}

/**
 * Parse commands joined by | or |&.
 */
function parsePipeline(P: ParseState): TsNode | null {
  let first = parseCommand(P)
  if (!first) return null
  const parts: TsNode[] = [first]
  while (true) {
    const save = saveLex(P.L)
    const t = nextToken(P.L, 'cmd')
    if (t.type === 'OP' && (t.value === '|' || t.value === '|&')) {
      const op = leaf(P, t.value, t)
      skipNewlines(P)
      const next = parseCommand(P)
      if (!next) {
        parts.push(op)
        break
      }
      // Hoist trailing redirect on `next` to wrap current pipeline fragment
      if (
        next.type === 'redirected_statement' &&
        next.children.length >= 2 &&
        parts.length >= 1
      ) {
        const inner = next.children[0]!
        const redirs = next.children.slice(1)
        const pipeKids = [...parts, op, inner]
        const pipeNode = mk(
          P,
          'pipeline',
          pipeKids[0]!.startIndex,
          inner.endIndex,
          pipeKids,
        )
        const lastR = redirs[redirs.length - 1]!
        const wrapped = mk(
          P,
          'redirected_statement',
          pipeNode.startIndex,
          lastR.endIndex,
          [pipeNode, ...redirs],
        )
        parts.length = 0
        parts.push(wrapped)
        first = wrapped
        continue
      }
      parts.push(op, next)
    } else {
      restoreLex(P.L, save)
      break
    }
  }
  if (parts.length === 1) return parts[0]!
  const last = parts[parts.length - 1]!
  return mk(P, 'pipeline', parts[0]!.startIndex, last.endIndex, parts)
}
