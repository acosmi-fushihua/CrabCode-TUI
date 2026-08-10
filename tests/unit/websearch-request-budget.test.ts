/**
 * WebSearch 显式请求预算 —— 嵌套模型流的第三处同形补齐。
 *
 * 2026-08-08 deep-research 事故审计发现 `src/tools/WebSearchTool/` 全目录 0 处 timeout,
 * 而它的 Mode A 是一条**嵌套模型流**(工具内部再调 `queryModelWithStreaming`, 让主循环
 * 模型带 `web_search` server tool 跑最多 8 次搜索)。整个目录里唯一与时间相关的东西只有
 * 一句 `signal: context.abortController.signal` —— 这条流没有任何属于自己的上界。
 * CLAUDE.md §SDK 直调的配套铁律: 「新增裸调 chat 的路径必须同 PR 声明预算 —— 隐式继承
 * 别人的默认值正是本次事故的形态本身」。同门范本 `sideQuery.ts` /`tokenEstimation.ts`,
 * 本文件镜像 `tests/unit/sidequery-request-budget.test.ts` 的写法。
 *
 * 为什么不是「断言常量等于 120000 就收工」: 本仓 2026-07-30 的战训是「测抄件 = 断言不为
 * 零仍零覆盖」。所以本文件的每一条断言都钩在**真源**上 —— 行为半场直接 import 并调用
 * WebSearchTool 真正使用的那几个导出; 接线半场从 WebSearchTool.ts 的源码里按括号配平取
 * 出真实调用点的实参再断言。两半合起来才是完整证明: 单看常量证明不了它被传下去了, 单看
 * 接线证明不了到期后会发生什么。
 *
 * 关于「为什么不真等 120s 证明它超时」: 与 sideQuery 那份同理, 本仓无 fake timer。预算
 * 定时器本身的行为由共享原语 `createCombinedAbortSignal` 负责(已在 sideQuery 那份里以
 * 毫秒级真跑钉死), 这里只证明 WebSearch 确实用了它、用的是本工具自己的常量, 并且到期
 * 之后走的是「报错」而不是「把半截结果冒充成完整结果」。
 */
import { describe, test, expect } from 'bun:test'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

import {
  WEB_SEARCH_BUDGET_ERROR_NAME,
  WEB_SEARCH_REQUEST_TIMEOUT_MS,
  assertWebSearchStreamCompleted,
  isWebSearchBudgetExpiry,
  webSearchBudgetExhaustedError,
} from '../../src/tools/WebSearchTool/WebSearchTool.js'
import { APIUserAbortError } from '../../src/errors/api-errors.js'
import { classifyTerminalToolError } from '../../src/services/tools/terminalToolError.js'

const SRC_ROOT = join(import.meta.dir, '..', '..', 'src')
const WEB_SEARCH_SRC = join(
  SRC_ROOT,
  'tools',
  'WebSearchTool',
  'WebSearchTool.ts',
)

/** 读取源码文本, 行尾归一后再扫(本仓混有 CRLF, 逐字符偏移会漂)。 */
function readSource(path: string = WEB_SEARCH_SRC): string {
  return readFileSync(path, 'utf8').replace(/\r\n/g, '\n')
}

/**
 * 从 `needle(` 起做括号配平, 取出整段实参文本。
 *
 * 刻意不逐字比对整段代码块: 那种断言会因为任何无关的换行或重命名假红(与
 * sidequery-request-budget.test.ts 同一约定)。
 */
function extractCallArgs(
  source: string,
  needle: string,
  fromIndex = 0,
): { text: string; end: number } {
  const start = source.indexOf(needle, fromIndex)
  expect(start, `找不到调用点 ${needle}`).toBeGreaterThan(-1)
  let depth = 0
  for (let i = start + needle.length - 1; i < source.length; i++) {
    const ch = source[i]
    if (ch === '(') depth++
    else if (ch === ')') {
      depth--
      if (depth === 0) return { text: source.slice(start, i + 1), end: i + 1 }
    }
  }
  throw new Error(`${needle} 括号未配平`)
}

/** 取出某个 `const NAME = 12_345` 形态常量的数值(用于跨文件的内外层次序断言)。 */
function readNumericConstant(path: string, name: string): number {
  const match = readSource(path).match(
    new RegExp(`${name}\\s*=\\s*([0-9_]+)`, 'u'),
  )
  expect(match, `${name} 未在 ${path} 找到`).not.toBeNull()
  return Number(match![1]!.replace(/_/g, ''))
}

describe('WEB_SEARCH_REQUEST_TIMEOUT_MS —— 取值与内外层次序', () => {
  test('预算存在且为 120s', () => {
    expect(WEB_SEARCH_REQUEST_TIMEOUT_MS).toBe(120_000)
  })

  test('比 sideQuery / tokenEstimation 的 60s 宽 —— 它不是一问一答的辅助小请求', () => {
    // 本工具是一条会跑多次搜索、带 thinking、可能触发非流式降级的完整模型流,
    // 与那两处的量级不同。这条断言把「为什么没照抄 60s」钉成契约而不是注释。
    expect(WEB_SEARCH_REQUEST_TIMEOUT_MS).toBeGreaterThan(60_000)
  })

  test('内层预算严格小于本预算(CLAUDE.md §硬约束 #17 条 2)', () => {
    // Mode B 的两个 provider 各自带 30s 内层预算; 内层必须先开火, 否则内层看门狗是
    // 死代码, 且把「可解释失败」降级成一个笼统的预算到期。
    const ali = readNumericConstant(
      join(SRC_ROOT, 'services', 'search', 'ali.ts'),
      'ALI_TIMEOUT_MS',
    )
    const bocha = readNumericConstant(
      join(SRC_ROOT, 'services', 'search', 'bocha.ts'),
      'BOCHA_TIMEOUT_MS',
    )
    expect(ali).toBeLessThan(WEB_SEARCH_REQUEST_TIMEOUT_MS)
    expect(bocha).toBeLessThan(WEB_SEARCH_REQUEST_TIMEOUT_MS)
  })
})

describe('预算到期的判定与报错 —— 真源函数, 不是抄件', () => {
  test('预算到期 ≠ 调用方取消(四种组合全覆盖)', () => {
    const aborted = () => {
      const c = new AbortController()
      c.abort()
      return c.signal
    }
    const live = () => new AbortController().signal

    // 只有「预算 aborted 且调用方没 abort」才算预算到期。
    expect(isWebSearchBudgetExpiry(aborted(), live())).toBe(true)
    // 用户按了停止 —— 是取消不是失败, 既有取消路径原样保留。
    expect(isWebSearchBudgetExpiry(aborted(), aborted())).toBe(false)
    expect(isWebSearchBudgetExpiry(live(), live())).toBe(false)
    expect(isWebSearchBudgetExpiry(live(), aborted())).toBe(false)
  })

  test('错误可辨识: 带工具名 + 预算值 + 稳定的 Error.name', () => {
    const err = webSearchBudgetExhaustedError()
    expect(err).toBeInstanceOf(Error)
    expect(err.name).toBe(WEB_SEARCH_BUDGET_ERROR_NAME)
    expect(err.message).toContain('WebSearch')
    // 预算值从真源常量派生再比对 —— 写死 '120000' 会变成常量派生文案漂移
    // (改了常量而文案不动, 测试照绿)。
    expect(err.message).toContain(String(WEB_SEARCH_REQUEST_TIMEOUT_MS))
  })

  test('保留 cause, 底层失败不被吞掉', () => {
    const cause = new Error('underlying abort')
    expect(webSearchBudgetExhaustedError(cause).cause).toBe(cause)
    expect(webSearchBudgetExhaustedError().cause).toBeUndefined()
  })

  test('不得被 #16 判成 terminal —— 超时是瞬态, 重试一次搜索是合理的', () => {
    const err = webSearchBudgetExhaustedError()
    expect(classifyTerminalToolError(err.message)).toBeNull()
    expect(classifyTerminalToolError(String(err))).toBeNull()
    // 也不能撞上「Exit code N」那条回音室排除前缀。
    expect(err.message.startsWith('Exit code')).toBe(false)
  })

  test('流静默结束后优先恢复 caller abort，不把半截 blocks 当成成功', () => {
    const caller = new AbortController()
    const budget = new AbortController()

    caller.abort()
    expect(() =>
      assertWebSearchStreamCompleted(budget.signal, caller.signal),
    ).toThrow(APIUserAbortError)

    // 即使两条 signal 同时 aborted，用户取消仍是真实终态。
    budget.abort()
    expect(() =>
      assertWebSearchStreamCompleted(budget.signal, caller.signal),
    ).toThrow(APIUserAbortError)
  })

  test('只有预算到期时仍返回可辨识的预算错误', () => {
    const caller = new AbortController()
    const budget = new AbortController()
    budget.abort()

    let thrown: unknown
    try {
      assertWebSearchStreamCompleted(budget.signal, caller.signal)
    } catch (error) {
      thrown = error
    }
    expect(thrown).toBeInstanceOf(Error)
    expect((thrown as Error).name).toBe(WEB_SEARCH_BUDGET_ERROR_NAME)
  })
})

describe('WebSearchTool 源码契约 —— 预算真的被传到了每一个调用点', () => {
  test('唯一创建点, 且用的是本工具自己的常量', () => {
    const src = readSource()
    // 多于一个创建点 = 叠了第二层预算, 语义立刻含糊。
    expect(src.split('createCombinedAbortSignal(').length - 1).toBe(1)
    const args = extractCallArgs(src, 'createCombinedAbortSignal(').text
    expect(args).toContain('timeoutMs: WEB_SEARCH_REQUEST_TIMEOUT_MS')
    // 派生自调用方 signal —— 预算是叠加不是替换, 用户按停止必须依旧立刻生效。
    expect(args).toContain('callerSignal')
  })

  test('Mode A: queryModelWithStreaming 收到的是预算 signal, 不是裸 abortController', () => {
    const args = extractCallArgs(readSource(), 'queryModelWithStreaming(').text
    expect(args).toContain('signal: budgetSignal')
    // 回归指纹: 事故前这里就是这一行。
    expect(args).not.toContain('context.abortController.signal')
  })

  test('Mode B: callLocalSearch 的每个调用点都把 budget.signal 传进去', () => {
    const src = readSource()
    // 只数调用点, 不数函数定义(定义是 `async function callLocalSearch(`)。
    const callSites: string[] = []
    let cursor = 0
    for (;;) {
      const at = src.indexOf('await callLocalSearch(', cursor)
      if (at < 0) break
      const { text, end } = extractCallArgs(src, 'callLocalSearch(', at)
      callSites.push(text)
      cursor = end
    }
    expect(callSites.length).toBeGreaterThan(0)
    for (const args of callSites) {
      expect(args).toContain('budget.signal')
    }
  })

  test('Mode B: executeSearch 真的收下了那个 signal(它一直有形参, 此前一个都没传)', () => {
    const args = extractCallArgs(readSource(), 'executeSearch(').text
    expect(args).toContain('budgetSignal')
  })

  test('`return await` 而不是 `return` —— 否则 cleanup 早于 Promise 落定, 预算等于没生效', () => {
    const src = readSource()
    expect(src).not.toMatch(/\n\s*return callLocalSearch\(/)
    expect(src).not.toMatch(/\n\s*return runUpstreamServerToolSearch\(/)
    expect(src).toMatch(/return await callLocalSearch\(/)
    expect(src).toMatch(/return await runUpstreamServerToolSearch\(/)
  })

  test('cleanup 挂在 finally 上(return / throw 两条出口都要过)', () => {
    expect(readSource()).toMatch(
      /\}\s*finally\s*\{[\s\S]{0,300}budget\.cleanup\(\)/,
    )
  })

  test('流消费结束后显式判定预算, 防止「半截结果冒充完整结果」回归', () => {
    // queryModel 在 abort 时 catch 掉 APIUserAbortError 后**静默 return**, 不抛。
    // 少了这一步, 被预算腰斩的搜索会带着空/半截 allContentBlocks 正常组装返回。
    const src = readSource()
    const checkAt = src.indexOf(
      'assertWebSearchStreamCompleted(\n    budgetSignal,\n    context.abortController.signal,',
    )
    expect(checkAt, '流消费后的取消/预算判定缺失').toBeGreaterThan(-1)
    const loopAt = src.indexOf('for await (const event of queryStream)')
    // 取**调用点**而不是函数定义(定义在文件上方, 位置比较会恒真)。
    const assembleAt = src.indexOf(
      'const data = makeOutputFromSearchResponse(',
    )
    expect(loopAt).toBeGreaterThan(-1)
    expect(assembleAt).toBeGreaterThan(-1)
    // 判定必须夹在「流跑完」与「组装输出」之间, 否则组装照样会发生。
    expect(checkAt).toBeGreaterThan(loopAt)
    expect(checkAt).toBeLessThan(assembleAt)
  })

  test('catch 分支把 Mode B 的裸 AbortError 翻成可辨识错误, 且不重复包装自己', () => {
    const src = readSource()
    // 已经是我们自己的错误时原样放行 —— 否则 catch 会把它再包一层。
    expect(src).toMatch(
      /err\.name === WEB_SEARCH_BUDGET_ERROR_NAME[\s\S]{0,80}throw err/,
    )
    expect(src).toMatch(
      /isWebSearchBudgetExpiry\(budget\.signal, callerSignal\)[\s\S]{0,120}throw webSearchBudgetExhaustedError\(err\)/,
    )
  })
})
