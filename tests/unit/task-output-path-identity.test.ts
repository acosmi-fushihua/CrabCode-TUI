import { afterEach, describe, expect, test } from 'bun:test'
import {
  mkdir,
  mkdtemp,
  readFile,
  rm,
  stat,
  symlink,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { getEmptyToolPermissionContext, type ToolUseContext } from '../../src/Tool.js'
import { FileReadTool } from '../../src/tools/FileReadTool/FileReadTool.js'
import { createFileStateCacheWithSizeLimit } from '../../src/utils/fileStateCache.js'
import { getProjectTempDir } from '../../src/utils/permissions/filesystem.js'
import {
  _resetTaskOutputDirForTest,
  getTaskOutput,
  initTaskOutput,
  initTaskOutputAsSymlink,
  persistTaskOutputFile,
} from '../../src/utils/task/diskOutput.js'
import { TaskOutput } from '../../src/utils/task/TaskOutput.js'

const cleanupRoots: string[] = []

afterEach(async () => {
  _resetTaskOutputDirForTest()
  await Promise.all(
    cleanupRoots.splice(0).map(path => rm(path, { recursive: true, force: true })),
  )
})

describe('task output path identity', () => {
  test('an initially absent transcript target is created and bound', async () => {
    const root = await mkdtemp(join(tmpdir(), 'crabcode-output-new-target-'))
    cleanupRoots.push(root)
    const taskId = `new-target-${process.pid}-${Date.now()}`
    const target = join(root, 'nested', 'agent.jsonl')

    const outputPath = await initTaskOutputAsSymlink(taskId, target)
    await expect(Bun.file(target).exists()).resolves.toBe(false)
    await mkdir(join(root, 'nested'), { recursive: true })
    await writeFile(target, 'created after registration')
    await expect(getTaskOutput(taskId)).resolves.toBe(
      'created after registration',
    )

    await rm(outputPath, { force: true })
    _resetTaskOutputDirForTest()
  })

  test('a host-owned session relink deliberately rebinds the task inode', async () => {
    const root = await mkdtemp(join(tmpdir(), 'crabcode-output-relink-'))
    cleanupRoots.push(root)
    const taskId = `relink-${process.pid}-${Date.now()}`
    const first = join(root, 'first.jsonl')
    const second = join(root, 'second.jsonl')
    await writeFile(first, 'first transcript')
    await writeFile(second, 'second transcript')

    const outputPath = await initTaskOutputAsSymlink(taskId, first)
    await expect(getTaskOutput(taskId)).resolves.toBe('first transcript')

    await initTaskOutputAsSymlink(taskId, second)
    await expect(getTaskOutput(taskId)).resolves.toBe('second transcript')

    await rm(outputPath, { force: true })
    _resetTaskOutputDirForTest()
  })

  test('persistence copies and truncates only the bound inode', async () => {
    const root = await mkdtemp(join(tmpdir(), 'crabcode-output-persist-'))
    cleanupRoots.push(root)
    const taskId = `persist-${process.pid}-${Date.now()}`
    const output = new TaskOutput(taskId, null, true)
    const destination = join(root, 'persisted.txt')

    await initTaskOutput(taskId)
    await writeFile(output.path, '0123456789')

    await expect(persistTaskOutputFile(taskId, destination, 5)).resolves.toBe(10)
    await expect(readFile(destination, 'utf8')).resolves.toBe('01234')
    await expect(stat(output.path).then(value => value.size)).resolves.toBe(5)

    await output.deleteOutputFile()
  })

  test('host reads and persistence reject a post-spawn symlink replacement', async () => {
    const root = await mkdtemp(join(tmpdir(), 'crabcode-output-identity-'))
    cleanupRoots.push(root)
    const taskId = `identity-${process.pid}-${Date.now()}`
    const output = new TaskOutput(taskId, null, true)

    await initTaskOutput(taskId)
    await writeFile(output.path, 'original output')

    const secret = join(root, 'secret.txt')
    const destination = join(root, 'persisted.txt')
    await writeFile(secret, 'HOST-SECRET-MUST-NOT-LEAK')
    await rm(output.path)
    await symlink(secret, output.path)

    const stdout = await output.getStdout()
    expect(stdout).toContain('<bash output unavailable:')
    expect(stdout).not.toContain('HOST-SECRET-MUST-NOT-LEAK')

    await expect(
      persistTaskOutputFile(taskId, destination, 64 * 1024 * 1024),
    ).rejects.toThrow()
    await expect(Bun.file(destination).exists()).resolves.toBe(false)

    await output.deleteOutputFile()
  })

  test('a FIFO replacement cannot block the host reader', async () => {
    if (process.platform === 'win32') return

    const root = await mkdtemp(join(tmpdir(), 'crabcode-output-fifo-'))
    cleanupRoots.push(root)
    const taskId = `fifo-${process.pid}-${Date.now()}`
    const output = new TaskOutput(taskId, null, true)
    const fifo = join(root, 'replacement.fifo')

    await initTaskOutput(taskId)
    const mkfifo = Bun.spawn(['mkfifo', fifo], {
      stdout: 'ignore',
      stderr: 'pipe',
    })
    const mkfifoExit = await mkfifo.exited
    expect(mkfifoExit, await new Response(mkfifo.stderr).text()).toBe(0)
    await rm(output.path)
    await symlink(fifo, output.path)

    const result = await Promise.race([
      output.getStdout(),
      new Promise<string>(resolve =>
        setTimeout(() => resolve('TIMED-OUT'), 1_000),
      ),
    ])
    expect(result).not.toBe('TIMED-OUT')
    expect(result).toContain('<bash output unavailable:')

    await output.deleteOutputFile()
  })

  test('FileRead auto-allows only the exact host-bound task path and rejects replacement', async () => {
    const root = await mkdtemp(join(tmpdir(), 'crabcode-file-read-identity-'))
    cleanupRoots.push(root)
    const projectTempCase = join(
      getProjectTempDir(),
      `untrusted-${process.pid}-${Date.now()}`,
    )
    cleanupRoots.push(projectTempCase)
    await writeFile(join(root, 'secret.txt'), 'HOST-SECRET-MUST-NOT-LEAK')
    await mkdir(projectTempCase, { recursive: true })
    await writeFile(join(projectTempCase, 'placeholder'), 'ordinary temp file')
    const tempAlias = join(projectTempCase, 'alias.txt')
    await symlink(join(root, 'secret.txt'), tempAlias)

    const toolPermissionContext = getEmptyToolPermissionContext()
    const permissionContext = {
      getAppState: () => ({ toolPermissionContext }),
    } as unknown as ToolUseContext

    const ordinaryDecision = await FileReadTool.checkPermissions!(
      { file_path: tempAlias },
      permissionContext,
    )
    expect(ordinaryDecision.behavior).toBe('ask')

    const taskId = `file-read-${process.pid}-${Date.now()}`
    const output = new TaskOutput(taskId, null, true)
    await initTaskOutput(taskId)
    await writeFile(output.path, 'trusted task output')
    const boundDecision = await FileReadTool.checkPermissions!(
      { file_path: output.path },
      permissionContext,
    )
    expect(boundDecision.behavior).toBe('allow')

    await rm(output.path)
    await symlink(join(root, 'secret.txt'), output.path)
    const callContext = {
      ...permissionContext,
      abortController: new AbortController(),
      readFileState: createFileStateCacheWithSizeLimit(8),
    } as unknown as ToolUseContext
    let readError = ''
    try {
      await FileReadTool.call(
        { file_path: output.path, offset: 1 },
        callContext,
      )
    } catch (error) {
      readError = String(error)
    }
    expect(readError).toContain('identity changed')
    expect(readError).not.toContain('HOST-SECRET-MUST-NOT-LEAK')

    await output.deleteOutputFile()

    const agentTaskId = `file-read-backing-${process.pid}-${Date.now()}`
    const backingPath = join(root, 'agent-backing.jsonl')
    await writeFile(backingPath, 'agent transcript')
    const publishedPath = await initTaskOutputAsSymlink(
      agentTaskId,
      backingPath,
    )
    // Simulate native Windows, where no physical alias is published.
    await rm(publishedPath, { force: true })
    const deniedContext = {
      getAppState: () => ({
        toolPermissionContext: {
          ...getEmptyToolPermissionContext(),
          alwaysDenyRules: {
            session: [`Read(/${backingPath})`],
          },
        },
      }),
    } as unknown as ToolUseContext
    const deniedBackingDecision = await FileReadTool.checkPermissions!(
      { file_path: publishedPath },
      deniedContext,
    )
    expect(deniedBackingDecision.behavior).toBe('deny')
  })
})
