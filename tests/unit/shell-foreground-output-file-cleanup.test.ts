// W-WORKFLOW-AGENT-LIVENESS 次级发现 F1 (2026-08-08) —— 前台 shell 命令的产物文件残留。
//
// 每次 `Shell.exec` 在 file 模式下都会 `open(taskOutput.path, 'w')`：那个文件**就是**
// 子进程的 stdio fd 本体，不能惰性创建。谁来删它，因此是一条必须钉住的不变量 —— 本仓
// **没有任何**清扫器会回收它（`cleanupTaskOutput` 零调用点，session 临时目录无人 GC，
// 实测本机 807 个 `.output` 遗留）。
//
// 本文件钉三条：
//   1. 前台命令**有输出**时不留文件（内容已内联进 tool result，文件是冗余的）；
//   2. 前台命令**零输出**时不留文件（0 字节文件无检索价值）；
//      —— 1/2 由 `ShellCommand.#handleExit` 的 `outputFileRedundant` 分支承担，删掉它两条同时红；
//   3. **spawn 抛错**时不留文件 —— 这条是本轮补的：`open()` 已经建好文件、`spawn` 抛错、
//      于是 `ShellCommandImpl` 从未构造、`#handleExit` 永不运行，第 1/2 条的删除者
//      整条都不在场。这是 exec 唯一一条会漏文件的返回路径。
//
// 隔离靠**每个测试进程自己的随机 sessionId**（产物目录 =
// `<projectTempDir>/<sessionId>/tasks`），不是靠改 HOME —— 硬约束 #4 明令不得 set
// `process.env.HOME`（Bun 的 `os.homedir()` 首调即缓存）。afterAll 再把本进程建过的
// 目录清掉，免得本测试自己成为它所描述的那种遗留。

import { afterAll, describe, expect, test } from 'bun:test'
import { spawn } from 'child_process'
import { constants as fsConstants } from 'fs'
import { mkdir, open, readdir, rm, stat } from 'fs/promises'
import { dirname, join } from 'path'
import { exec } from '../../src/utils/Shell.js'
import { wrapSpawn } from '../../src/utils/ShellCommand.js'
import {
  bindTaskOutputFileIdentity,
  getTaskOutputDir,
} from '../../src/utils/task/diskOutput.js'
import { TaskOutput } from '../../src/utils/task/TaskOutput.js'

/** `.output` files currently sitting in this process's task directory. */
async function outputFiles(): Promise<string[]> {
  try {
    return (await readdir(getTaskOutputDir()))
      .filter(name => name.endsWith('.output'))
      .sort()
  } catch {
    // Directory not created yet — indistinguishable from "no leftovers", and
    // that is the answer every assertion here wants.
    return []
  }
}

/** Absolute paths + sizes, for a failure message that says *what* leaked. */
async function describeLeftovers(names: string[]): Promise<string[]> {
  const dir = getTaskOutputDir()
  return Promise.all(
    names.map(async name => {
      try {
        return `${name} (${(await stat(join(dir, name))).size} bytes)`
      } catch {
        return `${name} (vanished)`
      }
    }),
  )
}

/**
 * Let the exit path's fire-and-forget `unlink` land.
 *
 * `#handleExit` deletes with `void this.taskOutput.deleteOutputFile()` — the
 * result promise resolves without awaiting it. Polling rather than sleeping a
 * fixed amount keeps the test honest on a slow machine without making it slow
 * on a fast one.
 */
async function settledOutputFiles(baseline: string[]): Promise<string[]> {
  for (let i = 0; i < 50; i++) {
    const now = (await outputFiles()).filter(n => !baseline.includes(n))
    if (now.length === 0) return now
    await new Promise(resolve => setTimeout(resolve, 20))
  }
  return (await outputFiles()).filter(n => !baseline.includes(n))
}

afterAll(async () => {
  await rm(getTaskOutputDir(), { recursive: true, force: true })
})

describe('F1 — 前台 shell 命令不留产物文件', () => {
  test('有输出的前台命令：内容进 stdout，文件被删', async () => {
    const baseline = await outputFiles()
    const command = await exec(
      'echo f1-with-output',
      new AbortController().signal,
      'bash',
    )
    const result = await command.result

    // 先证明这条命令真的跑通了 —— 否则「没留文件」可能只是因为它根本没执行。
    expect(result.code).toBe(0)
    expect(result.stdout).toContain('f1-with-output')
    // 小输出走的是 `outputFileRedundant` 分支：内容已完整内联，文件因此是冗余的。
    // 大输出**不**走这条（`result.outputFilePath` 会被填上，文件刻意保留），所以这里
    // 一并断言我们确实在小输出这条路径上。
    expect(result.outputFilePath).toBeUndefined()

    expect(await describeLeftovers(await settledOutputFiles(baseline))).toEqual(
      [],
    )
  }, 60_000)

  test('零输出的前台命令：0 字节文件同样被删', async () => {
    const baseline = await outputFiles()
    const command = await exec('true', new AbortController().signal, 'bash')
    const result = await command.result

    expect(result.code).toBe(0)
    expect(result.stdout).toBe('')

    expect(await describeLeftovers(await settledOutputFiles(baseline))).toEqual(
      [],
    )
  }, 60_000)

  test('spawn 抛错的前台命令：文件已建但从未有子进程，exec 自己收尾', async () => {
    const baseline = await outputFiles()
    // 命令串里的 NUL 字节让 Node 在 `spawn()` **同步**抛
    // `ERR_INVALID_ARG_VALUE`（args 不得含 null bytes）。这是从真实入口触发那条
    // catch 分支的方式 —— 不 mock `child_process`，因为 Bun 的 `mock.module` 是
    // 进程级的，会泄漏进同一进程后续每个测试文件。
    const command = await exec(
      'echo f1-\0-nul',
      new AbortController().signal,
      'bash',
    )
    const result = await command.result

    // 126 = `createAbortedCommand` 在 catch 里给的码值。断言它，是为了证明这条
    // 用例真的走进了 catch 分支：换个不抛错的命令这里会是 0，整条测试就变成空转。
    expect(result.code).toBe(126)

    const leftovers = await settledOutputFiles(baseline)
    // 去掉 exec catch 分支里的 `deleteOutputFile()` 这行，这里就会出现一个 0 字节
    // 的 `b*.output` —— 这正是本条要防的形态。
    expect(await describeLeftovers(leftovers)).toEqual([])
  }, 60_000)
})

describe('host-owned shell output containment', () => {
  test('unlinking the published path cannot bypass the byte cap', async () => {
    if (process.platform === 'win32') return

    const taskId = `output-cap-${process.pid}-${Date.now()}`
    const taskOutput = new TaskOutput(taskId, null, true)
    await mkdir(dirname(taskOutput.path), { recursive: true })
    const outputHandle = await open(
      taskOutput.path,
      fsConstants.O_RDWR |
        fsConstants.O_CREAT |
        fsConstants.O_EXCL |
        fsConstants.O_APPEND |
        fsConstants.O_NONBLOCK |
        (fsConstants.O_NOFOLLOW ?? 0),
    )
    const outputStats = await outputHandle.stat({ bigint: true })
    bindTaskOutputFileIdentity(taskId, {
      dev: outputStats.dev,
      ino: outputStats.ino,
    })

    const child = spawn(
      '/bin/bash',
      [
        '-c',
        'rm -f "$1"; head -c 1048576 /dev/zero; sleep 5',
        '--',
        taskOutput.path,
      ],
      { detached: true, stdio: ['ignore', 'pipe', 'pipe'] },
    )
    const command = wrapSpawn(
      child,
      new AbortController().signal,
      10_000,
      taskOutput,
      false,
      outputHandle,
      16 * 1024,
    )
    const result = await command.result

    expect(result.code).toBe(137)
    expect(result.interrupted).toBe(true)
    expect(result.stderr).toContain('output file exceeded')
    expect(result.stdout.length).toBeGreaterThan(0)
    await taskOutput.deleteOutputFile()
  }, 15_000)

  test('a writer inherited by a grandchild is cut off after shell exit', async () => {
    if (process.platform === 'win32') return

    const taskId = `output-grandchild-${process.pid}-${Date.now()}`
    const taskOutput = new TaskOutput(taskId, null, true)
    await mkdir(dirname(taskOutput.path), { recursive: true })
    const outputHandle = await open(
      taskOutput.path,
      fsConstants.O_RDWR |
        fsConstants.O_CREAT |
        fsConstants.O_EXCL |
        fsConstants.O_APPEND |
        fsConstants.O_NONBLOCK |
        (fsConstants.O_NOFOLLOW ?? 0),
    )
    const outputStats = await outputHandle.stat({ bigint: true })
    bindTaskOutputFileIdentity(taskId, {
      dev: outputStats.dev,
      ino: outputStats.ino,
    })

    const child = spawn(
      '/bin/bash',
      [
        '-c',
        'head -c 100000 /dev/zero; (while printf x; do sleep 0.01; done) &',
      ],
      { detached: true, stdio: ['ignore', 'pipe', 'pipe'] },
    )
    const command = wrapSpawn(
      child,
      new AbortController().signal,
      10_000,
      taskOutput,
      false,
      outputHandle,
      1024 * 1024,
    )
    const result = await Promise.race([
      command.result,
      new Promise<never>((_, reject) =>
        setTimeout(
          () => reject(new Error('shell result stayed attached to grandchild')),
          2_000,
        ),
      ),
    ])

    expect(result.code).toBe(0)
    expect(result.outputFilePath).toBe(taskOutput.path)
    const sizeAtResult = (await stat(taskOutput.path)).size
    await new Promise(resolve => setTimeout(resolve, 300))
    expect((await stat(taskOutput.path)).size).toBe(sizeAtResult)
    await taskOutput.deleteOutputFile()
  }, 15_000)
})
