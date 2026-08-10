/**
 * nonstreaming-fallback-budget.test.ts — 2026-08-06
 *
 * 事故: qwen3.8-max 长链路跑到第 11 个工具调用时报「网络连接中断」。链条是
 *   首字节 > 90s (期间客户端方向零字节) → 空闲看门狗掐流 → 回退非流式
 *   → 撞 SDK 内层 30s 隐式超时 → withRetry 重放 3 次、每次必然再死一遍。
 *
 * 客户端这一侧有两个缺陷:
 *   1. `getNonstreamingFallbackTimeoutMs()` 算出的 300s 只被写进埋点字段
 *      `timeout_ms`, **从未传给 SDK** —— 真正生效的是 SDK 内层 30s 默认值。
 *      回退路径的预算比它要救的流式路径**更短**, 是反向降级。
 *   2. `chatComplete` 连 `signal` 都没拿到, 用户按 ESC 取消不了非流式回退。
 *
 * 本文件钉的不变量:
 *   A. 预算与 signal 真的传到了 SDK 边界 (chatComplete → client.chatMessages)
 *   B. 预算是**整条回退链路共享一条 deadline**, 不是每次尝试各给一份
 *      —— 按尝试计会突破 getNonstreamingFallbackTimeoutMs 自己要守的容器
 *      idle-kill 线, 也会撞 CLAUDE.md §17 的轮次墙钟
 *   C. 流式看门狗接上了 SDK 的上游活性信号 (否则网关补的心跳被中间层吞掉)
 */
import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

import { chatComplete } from '../../src/services/acosmi/client.js'
import { withRetry } from '../../src/services/api/withRetry.js'
import { createCombinedAbortSignal } from '../../src/utils/combinedAbortSignal.js'

const QUERY_MODEL_SRC = readFileSync(
  join(import.meta.dir, '../../src/services/api/queryModel.ts'),
  'utf8',
)
const ACOSMI_CLIENT_SRC = readFileSync(
  join(import.meta.dir, '../../src/services/acosmi/client.ts'),
  'utf8',
)

// ---------------------------------------------------------------------------
// A. 边界行为: signal 真的走到 SDK
// ---------------------------------------------------------------------------

describe('chatComplete 把 signal 交给 SDK', () => {
  test('调用方传的 signal 原样到达 client.chatMessages', async () => {
    const controller = new AbortController()
    let seenSignal: AbortSignal | undefined = undefined

    const fakeClient = {
      isAuthorized: () => true,
      chatMessages: async (_id: string, _req: unknown, signal?: AbortSignal) => {
        seenSignal = signal
        return { id: 'msg_1', content: [] }
      },
    }

    await chatComplete(
      'some-gateway-model',
      { max_tokens: 1, messages: [{ role: 'user', content: 'hi' }] } as never,
      controller.signal,
      undefined,
      undefined,
      { getClient: async () => fakeClient as never },
    )

    expect(seenSignal).toBe(controller.signal)
  })

  test('不传 signal 时到达的是 undefined (旧行为, 即事故形态)', async () => {
    let seenSignal: AbortSignal | undefined | 'unset' = 'unset'
    const fakeClient = {
      isAuthorized: () => true,
      chatMessages: async (_id: string, _req: unknown, signal?: AbortSignal) => {
        seenSignal = signal
        return { id: 'msg_1', content: [] }
      },
    }

    await chatComplete(
      'some-gateway-model',
      { max_tokens: 1, messages: [{ role: 'user', content: 'hi' }] } as never,
      undefined,
      undefined,
      undefined,
      { getClient: async () => fakeClient as never },
    )

    // 负向对照: 证明上一条不是"随便传什么都等于 controller.signal"的假绿。
    expect(seenSignal).toBeUndefined()
  })
})

// ---------------------------------------------------------------------------
// B. 预算语义: 共享 deadline, 不是每次尝试各一份
// ---------------------------------------------------------------------------

describe('回退预算是整条链路共享的一条 deadline', () => {
  test('组合 signal 在超时后 abort, 且同一个对象被复用', async () => {
    const caller = new AbortController()
    const budget = createCombinedAbortSignal(caller.signal, { timeoutMs: 20 })
    try {
      expect(budget.signal.aborted).toBe(false)
      await new Promise(r => setTimeout(r, 60))
      expect(budget.signal.aborted).toBe(true)
    } finally {
      budget.cleanup()
    }
  })

  test('调用方 abort 会立刻传导到组合 signal (ESC 能取消回退)', () => {
    const caller = new AbortController()
    const budget = createCombinedAbortSignal(caller.signal, {
      timeoutMs: 60_000,
    })
    try {
      expect(budget.signal.aborted).toBe(false)
      caller.abort()
      expect(budget.signal.aborted).toBe(true)
    } finally {
      budget.cleanup()
    }
  })

  test('源码契约: 预算在 withRetry 之外创建一次, 不在每次尝试里重建', () => {
    const budgetIdx = QUERY_MODEL_SRC.indexOf(
      'const fallbackBudget = createCombinedAbortSignal(',
    )
    expect(budgetIdx).toBeGreaterThan(-1)

    // 只能有一处创建点 —— 出现两次就意味着有人在重试回调里又建了一个。
    const occurrences = QUERY_MODEL_SRC.split(
      'createCombinedAbortSignal(retryOptions.signal',
    ).length - 1
    expect(occurrences).toBe(1)

    // 且它必须早于承载重试循环的 runNonStreamingRetryLoop 定义, 即位于
    // 重试回调之外 —— 否则每次尝试都会拿到一份新的 300s。
    const loopIdx = QUERY_MODEL_SRC.indexOf(
      'async function* runNonStreamingRetryLoop(',
    )
    expect(loopIdx).toBeGreaterThan(-1)
    expect(budgetIdx).toBeLessThan(loopIdx)
  })

  test('源码契约: chatComplete 调用点确实带上了 budgetSignal', () => {
    // 事故形态是 chatComplete(model, req) 两实参; 修复后必须带第三个。
    //
    // 判的是「那个调用的实参里有没有 budgetSignal」, 不是逐字符比对一整段代码 ——
    // 后者会随任何无关的换行或重命名假红 (本文件的另一条断言就这么假红过一次)。
    const callIdx = QUERY_MODEL_SRC.indexOf('await chatComplete(')
    expect(callIdx).toBeGreaterThan(-1)

    const open = QUERY_MODEL_SRC.indexOf('(', callIdx)
    let depth = 0
    let close = -1
    for (let i = open; i < QUERY_MODEL_SRC.length; i++) {
      const ch = QUERY_MODEL_SRC[i]
      if (ch === '(') depth++
      else if (ch === ')') {
        depth--
        if (depth === 0) {
          close = i
          break
        }
      }
    }
    expect(close).toBeGreaterThan(open)
    expect(QUERY_MODEL_SRC.slice(open + 1, close)).toContain('budgetSignal')
  })

  test('源码契约: 同一预算也中断 retry backoff, 且不冒充用户取消', () => {
    expect(QUERY_MODEL_SRC).toContain('retrySignal: budgetSignal')
    expect(QUERY_MODEL_SRC).toContain('retryOptions.signal.aborted')
    expect(QUERY_MODEL_SRC).toContain('new APIConnectionTimeoutError({')
  })

  test('预算在一次 NetworkError 后到期时不再退避或重试', async () => {
    const caller = new AbortController()
    const deadline = new AbortController()
    const timeout = new Error('fallback deadline elapsed')
    let attempts = 0
    const generator = withRetry(
      async () => ({}) as never,
      async () => {
        attempts += 1
        deadline.abort()
        const networkError = new Error('wrapped abort')
        networkError.name = 'NetworkError'
        throw networkError
      },
      {
        model: 'test-model',
        thinkingConfig: {} as never,
        signal: caller.signal,
        retrySignal: deadline.signal,
        retryAbortError: () => timeout,
      },
    )

    await expect(generator.next()).rejects.toBe(timeout)
    expect(attempts).toBe(1)
  })

  test('源码契约: 预算必须被 cleanup (否则 Bun 下计时器悬挂)', () => {
    expect(QUERY_MODEL_SRC).toContain('fallbackBudget.cleanup()')
  })
})

// ---------------------------------------------------------------------------
// C. 流式看门狗接上上游活性信号
// ---------------------------------------------------------------------------

describe('流式空闲看门狗能看见保活注释行', () => {
  test('源码契约: chatStreamAdapter 把活性回调交给 SDK', () => {
    // 回调必须在**调用时**从 adapter 上读, 不能构造期快照 —— 消费方是在开始
    // 迭代之前才装配看门狗并赋值的, 快照会恒为 undefined。
    expect(ACOSMI_CLIENT_SRC).toContain('() => adapter.onUpstreamActivity?.()')
    expect(ACOSMI_CLIENT_SRC).toContain('onUpstreamActivity?: () => void')
  })

  test('源码契约: queryModel 把看门狗的复位函数挂上去', () => {
    // 能力探测 + 赋值必须都在。断言写成两条独立子串而不是一整段代码块 ——
    // 后者会随任何无关的换行/重命名假红 (首版就因为我改了变量名而假红过)。
    expect(QUERY_MODEL_SRC).toContain("'onUpstreamActivity' in activityHookTarget")
    expect(QUERY_MODEL_SRC).toContain(
      'activityHookTarget.onUpstreamActivity = resetStreamIdleTimer',
    )
  })

  test('源码契约: 能力探测不得直接对可能为 undefined 的 stream 用 in', () => {
    // `stream` 是闭包变量, releaseStreamResources 会把它置 undefined,
    // 而 `'x' in undefined` 抛 TypeError 而非返回 false —— 主查询路径不能赌
    // "中间没人加 await"。必须先落局部变量并判真。
    expect(QUERY_MODEL_SRC).not.toContain("'onUpstreamActivity' in stream")
  })

  test('adapter 上 onUpstreamActivity 这个键必须真实存在 (否则 in 探测恒 false)', async () => {
    const { chatStreamAdapter } = await import(
      '../../src/services/acosmi/client.js'
    )
    const adapter = chatStreamAdapter('some-gateway-model', {
      max_tokens: 1,
      messages: [{ role: 'user', content: 'hi' }],
    } as never)

    // 决定性判据。TS 的可选属性不赋值时运行时**没有这个键**, 而消费方正是用
    // `'onUpstreamActivity' in stream` 做能力探测 —— 只断言取值为 undefined 是
    // 分不出"键存在但未设"和"键压根不存在"的, 两种情况下取值都是 undefined,
    // 而后者会让整个钩子静默永不生效。
    expect('onUpstreamActivity' in adapter).toBe(true)
    expect(adapter.onUpstreamActivity).toBeUndefined()

    let fired = 0
    adapter.onUpstreamActivity = () => {
      fired++
    }
    adapter.onUpstreamActivity()
    expect(fired).toBe(1)
  })
})
