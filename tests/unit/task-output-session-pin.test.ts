import { describe, expect, test } from 'bun:test'
import { stat, writeFile } from 'node:fs/promises'
import { dirname, join } from 'node:path'

import { runWithSessionOverride } from '../../src/bootstrap/state.js'
import type { SessionId } from '../../src/types/ids.js'
import {
  _resetTaskOutputDirForTest,
  ensureTaskOutputDir,
  getTaskOutputPath,
} from '../../src/utils/task/diskOutput.js'
import { TaskOutput } from '../../src/utils/task/TaskOutput.js'

function withinSession<T>(sessionId: string, fn: () => T): T {
  return runWithSessionOverride({ sessionId: sessionId as SessionId }, fn)
}

describe('task output session pinning', () => {
  test('one task retains its original path while a new task uses the new session', async () => {
    const suffix = `${process.pid}-${Date.now()}`
    const retainedTask = `retained-${suffix}`
    const freshTask = `fresh-${suffix}`

    _resetTaskOutputDirForTest()
    try {
      const originalPath = withinSession('session-alpha', () =>
        getTaskOutputPath(retainedTask),
      )
      await withinSession('session-alpha', () => ensureTaskOutputDir(retainedTask))

      const retainedAfterRotation = withinSession('session-beta', () =>
        getTaskOutputPath(retainedTask),
      )
      const freshPath = withinSession('session-beta', () =>
        getTaskOutputPath(freshTask),
      )

      expect(originalPath).toContain(
        join('session-alpha', 'tasks', `${retainedTask}.output`),
      )
      expect(retainedAfterRotation).toBe(originalPath)
      expect(freshPath).toContain(join('session-beta', 'tasks', `${freshTask}.output`))
      const directory = await stat(dirname(originalPath))
      expect(directory.isDirectory()).toBe(true)
    } finally {
      _resetTaskOutputDirForTest()
    }
  })

  test('deleting a terminal artifact releases the task id pin', async () => {
    const taskId = `released-${process.pid}-${Date.now()}`
    _resetTaskOutputDirForTest()
    try {
      const output = withinSession(
        'session-alpha',
        () => new TaskOutput(taskId, null, true),
      )
      await withinSession('session-alpha', () => ensureTaskOutputDir(taskId))
      await writeFile(output.path, 'done')
      await output.deleteOutputFile()

      const reusedPath = withinSession('session-beta', () =>
        getTaskOutputPath(taskId),
      )
      expect(reusedPath).toContain(
        join('session-beta', 'tasks', `${taskId}.output`),
      )
      expect(reusedPath).not.toBe(output.path)
    } finally {
      _resetTaskOutputDirForTest()
    }
  })
})
