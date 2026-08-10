/**
 * sideQuery 显式请求预算 —— 2026-08-06 qwen3.8-max 事故的第二处同形回归。
 *
 * 事故本体是「11min 的预算建了但没传下去，30s 的内层默认值恒先触发」。修复把 chat
 * 链路的真实预算恢复成 11min 之后，**每一个此前靠那个 30s 默认值封顶的裸调用点都会
 * 跟着涨到 11min**。tokenEstimation 的两处已在同一轮修掉；sideQuery 这一处当时漏了，
 * 而它是 11 个调用点的唯一咽喉，其中 permissionExplainer / yoloClassifier 就挂在用户
 * 盯着权限提示等待的交互路径上。
 *
 * 用 sideQuery 既有的 `deps` 注入缝做行为断言（与 sideQuery.test.ts 同一约定，不用
 * mock.module —— 单进程测试里那会泄漏成全局模块替换）。
 *
 * 关于「为什么不直接跑满 60s 证明它超时」：本仓无 fake timer，等 60s 不可接受。改为
 * 拆成两段各自可快速证伪的断言 —— 预算助手本身的超时行为（毫秒级真跑）+ sideQuery
 * 确实用该助手和该常量派生了传下去的 signal。两段合起来才是完整证明，任何一段单独
 * 都不够。
 */
import { describe, test, expect, spyOn } from 'bun:test'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

import { sideQuery, type SideQueryDeps } from '../../src/utils/sideQuery.js'
import { createCombinedAbortSignal } from '../../src/utils/combinedAbortSignal.js'

const SIDE_QUERY_SRC = join(
  import.meta.dir,
  '..',
  '..',
  'src',
  'utils',
  'sideQuery.ts',
)

const okResponse = {
  id: 'msg_1',
  type: 'message',
  role: 'assistant',
  model: 'test-model',
  content: [],
  stop_reason: 'end_turn',
  usage: {
    input_tokens: 1,
    output_tokens: 1,
    cache_creation_input_tokens: 0,
    cache_read_input_tokens: 0,
  },
}

function baseDeps(overrides: Partial<SideQueryDeps> = {}): SideQueryDeps {
  return {
    systemToString: () => 'sys',
    sleep: async () => {},
    isAcosmiSubscriber: () => true,
    isEnterpriseSubscriber: () => false,
    ...overrides,
  }
}

/** 读取源码文本，行尾归一后再扫（本仓混有 CRLF，逐字符偏移会漂）。 */
function readSource(): string {
  return readFileSync(SIDE_QUERY_SRC, 'utf8').replace(/\r\n/g, '\n')
}

/**
 * 从 `needle(` 起做括号配平，取出整段实参文本。
 *
 * 刻意不逐字比对整段代码块：那种断言会因为任何无关的换行或重命名假红（本事故修复的
 * 前一轮就踩过两次）。
 */
function extractCallArgs(source: string, needle: string): string {
  const start = source.indexOf(needle)
  expect(start, `找不到调用点 ${needle}`).toBeGreaterThan(-1)
  let depth = 0
  for (let i = start + needle.length - 1; i < source.length; i++) {
    const ch = source[i]
    if (ch === '(') depth++
    else if (ch === ')') {
      depth--
      if (depth === 0) return source.slice(start, i + 1)
    }
  }
  throw new Error(`${needle} 括号未配平`)
}

describe('createCombinedAbortSignal —— 三处预算共同的地基', () => {
  test('timeoutMs 到期后派生 signal 被 abort', async () => {
    const budget = createCombinedAbortSignal(undefined, { timeoutMs: 5 })
    expect(budget.signal.aborted).toBe(false)
    await new Promise(r => setTimeout(r, 40))
    expect(budget.signal.aborted).toBe(true)
    budget.cleanup()
  })

  test('父 signal 先 abort 时派生 signal 立即 abort（取二者先到者）', () => {
    const parent = new AbortController()
    const budget = createCombinedAbortSignal(parent.signal, {
      timeoutMs: 60_000,
    })
    expect(budget.signal.aborted).toBe(false)
    parent.abort()
    expect(budget.signal.aborted).toBe(true)
    budget.cleanup()
  })

  test('cleanup 之后定时器不再开火', async () => {
    const budget = createCombinedAbortSignal(undefined, { timeoutMs: 5 })
    budget.cleanup()
    await new Promise(r => setTimeout(r, 40))
    expect(budget.signal.aborted).toBe(false)
  })

  test('父 signal 已 abort 时直接返回已 abort 的派生 signal', () => {
    const parent = new AbortController()
    parent.abort()
    const budget = createCombinedAbortSignal(parent.signal, { timeoutMs: 5 })
    expect(budget.signal.aborted).toBe(true)
    budget.cleanup()
  })
})

describe('sideQuery —— 传给 chatComplete 的是派生预算而不是裸 signal', () => {
  test('传下去的 signal 不是调用方那一个（证明中间确实套了预算）', async () => {
    const caller = new AbortController()
    let received: AbortSignal | undefined
    await sideQuery(
      { model: 'm', messages: [], querySource: 'auto_mode', signal: caller.signal },
      baseDeps({
        chatComplete: async (
          _model: unknown,
          _params: unknown,
          signal?: AbortSignal,
        ) => {
          received = signal
          return okResponse
        },
      } as Partial<SideQueryDeps>),
    )
    expect(received).toBeDefined()
    expect(received).not.toBe(caller.signal)
  })

  test('调用方 abort 会传导到派生 signal（预算是叠加不是替换）', async () => {
    const caller = new AbortController()
    let abortedBefore: boolean | undefined
    let abortedAfter: boolean | undefined
    await sideQuery(
      { model: 'm', messages: [], querySource: 'auto_mode', signal: caller.signal },
      baseDeps({
        chatComplete: async (
          _model: unknown,
          _params: unknown,
          signal?: AbortSignal,
        ) => {
          // 必须在**调用进行中**断言传导：cleanup() 在 finally 里摘掉父监听（这正是
          // 不泄漏定时器与监听器的要求），调用结束后再 abort 当然传导不到，而那个
          // 时刻本来也没有任何东西需要被取消。
          abortedBefore = signal!.aborted
          caller.abort()
          abortedAfter = signal!.aborted
          return okResponse
        },
      } as Partial<SideQueryDeps>),
    )
    expect(abortedBefore).toBe(false)
    expect(abortedAfter).toBe(true)
  })

  test('调用方主动取消时原样抛出，不伪装成「网络不可用」的空结果', async () => {
    const caller = new AbortController()
    const warnSpy = spyOn(console, 'warn').mockImplementation(() => {})
    try {
      await expect(
        sideQuery(
          {
            model: 'm',
            messages: [],
            querySource: 'auto_mode',
            signal: caller.signal,
          },
          baseDeps({
            chatComplete: async () => {
              caller.abort()
              const e = new Error('aborted')
              e.name = 'AbortError'
              throw e
            },
          }),
        ),
      ).rejects.toThrow('aborted')
      // 取消不是失败，不该走 fail-soft 告警。
      expect(warnSpy).not.toHaveBeenCalled()
    } finally {
      warnSpy.mockRestore()
    }
  })

  test('SDK 把取消包装成 NetworkError 时仍原样抛出', async () => {
    const caller = new AbortController()
    const warnSpy = spyOn(console, 'warn').mockImplementation(() => {})
    try {
      await expect(
        sideQuery(
          {
            model: 'm',
            messages: [],
            querySource: 'auto_mode',
            signal: caller.signal,
          },
          baseDeps({
            chatComplete: async () => {
              caller.abort()
              const error = new Error('wrapped abort')
              error.name = 'NetworkError'
              throw error
            },
          }),
        ),
      ).rejects.toThrow('wrapped abort')
      expect(warnSpy).not.toHaveBeenCalled()
    } finally {
      warnSpy.mockRestore()
    }
  })

  test('NetworkError 仍走既有 fail-soft（本次改动不得动到它）', async () => {
    const warnSpy = spyOn(console, 'warn').mockImplementation(() => {})
    try {
      const resp = await sideQuery(
        { model: 'm', messages: [], querySource: 'auto_mode' },
        baseDeps({
          chatComplete: async () => {
            const e = new Error('boom')
            e.name = 'NetworkError'
            throw e
          },
        }),
      )
      expect((resp as unknown as { id: string }).id).toBe(
        'sdk_network_unavailable',
      )
      expect(warnSpy.mock.calls.flat().join(' ')).toContain(
        'SDK network unavailable',
      )
    } finally {
      warnSpy.mockRestore()
    }
  })
})

describe('sideQuery —— 源码契约（防止预算被悄悄摘掉或改成共享 deadline）', () => {
  test('预算常量存在且为 60s', () => {
    expect(readSource()).toContain(
      'const SIDE_QUERY_REQUEST_TIMEOUT_MS = 60_000',
    )
  })

  test('chatComplete 收到的是 budget.signal，不是裸 signal', () => {
    const args = extractCallArgs(readSource(), 'chatComplete(')
    expect(args).toContain('budget.signal')
    // 裸 `signal,` 作为独立实参出现即回归（退避睡眠用的是 signal，但那不在本调用里）。
    expect(args).not.toMatch(/\n\s*signal,/)
  })

  test('预算在重试循环体内创建 —— 刻意按尝试计而非共享 deadline', () => {
    const src = readSource()
    const loopStart = src.indexOf('for (let attempt = 0; ; attempt++)')
    const budgetAt = src.indexOf('createCombinedAbortSignal(signal, {')
    expect(loopStart).toBeGreaterThan(-1)
    expect(budgetAt).toBeGreaterThan(loopStart)
    // 只允许一个创建点，防止叠出第二层预算。
    expect(src.split('createCombinedAbortSignal(').length - 1).toBe(1)
  })

  test('cleanup 挂在 finally 上（break/continue/return/throw 四条出口都要过）', () => {
    const src = readSource()
    expect(src).toMatch(/\}\s*finally\s*\{[\s\S]{0,200}budget\.cleanup\(\)/)
  })
})
