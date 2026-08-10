import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { createDirectTuiQueryEventSink } from '../../src/cli/directTuiQueryEvents.js'
import type { Message } from '../../src/types/message.js'
import { createAssistantMessage } from '../../src/utils/messages/factory.js'
import {
  clearSessionMessagesCache,
  flushSessionStorage,
  recordTranscript,
  removeTranscriptMessage,
  resetProjectForTesting,
  setSessionFileForTesting,
} from '../../src/utils/sessionStorage.js'

const ENGINE_SOURCE = join(import.meta.dir, '..', '..', 'src', 'QueryEngine.ts')

function tombstoneCaseSource(): string {
  const source = readFileSync(ENGINE_SOURCE, 'utf8').replace(/\r\n/g, '\n')
  const start = source.indexOf("case 'tombstone':")
  const end = source.indexOf("case 'assistant':", start)
  expect(start).toBeGreaterThan(-1)
  expect(end).toBeGreaterThan(start)
  return source.slice(start, end)
}

describe('tombstone cleanup on the direct route', () => {
  test('forwards the original tombstone object without reshaping it', () => {
    const delivered: Message[] = []
    const sink = createDirectTuiQueryEventSink(event => delivered.push(event))
    const tombstone = {
      type: 'tombstone',
      message: { uuid: '11111111-1111-4111-8111-111111111111' },
      opaque: { retain: true },
    } as unknown as Message

    sink(tombstone)

    expect(delivered).toEqual([tombstone])
    expect(delivered[0]).toBe(tombstone)
  })

  test('removes the target from both in-memory collections and the persisted transcript', () => {
    const source = tombstoneCaseSource()

    expect(source).toContain('this.mutableMessages.findLastIndex')
    expect(source).toContain('this.mutableMessages.splice(mutableIndex, 1)')
    expect(source).toContain('messages.findLastIndex')
    expect(source).toContain('messages.splice(bufferIndex, 1)')
    expect(source).toContain('await removeTranscriptMessage(targetUuid as UUID)')
  })

  test('orders a tombstone after an in-flight append and permits a same-UUID replacement', async () => {
    const root = await mkdtemp(join(tmpdir(), 'crabcode-tombstone-order-'))
    const transcriptPath = join(root, 'session.jsonl')
    const previousPersistenceOverride =
      process.env.TEST_ENABLE_SESSION_PERSISTENCE

    process.env.TEST_ENABLE_SESSION_PERSISTENCE = '1'
    resetProjectForTesting()
    clearSessionMessagesCache()
    await writeFile(transcriptPath, '', { mode: 0o600 })
    setSessionFileForTesting(transcriptPath)

    try {
      const orphan = createAssistantMessage({ content: 'orphaned attempt' })

      // Deliberately mirror QueryEngine's fire-and-forget assistant recording:
      // the tombstone starts before recordTranscript has reached its lazy 100ms
      // append drain.
      const inFlightRecording = recordTranscript([orphan])
      const removal = removeTranscriptMessage(orphan.uuid)

      // Start the retry immediately, without waiting for either earlier call.
      // Invocation order must still be append(orphan) -> remove ->
      // append(replacement), both in preparation and in the lazy disk queue.
      const replacement = {
        ...createAssistantMessage({ content: 'replacement attempt' }),
        uuid: orphan.uuid,
      }
      const replacementRecording = recordTranscript([replacement])
      await Promise.all([inFlightRecording, removal, replacementRecording])
      await flushSessionStorage()

      const persisted = await readFile(transcriptPath, 'utf8')
      expect(persisted).toContain(orphan.uuid)
      expect(persisted).toContain('replacement attempt')
      expect(persisted).not.toContain('orphaned attempt')
    } finally {
      if (previousPersistenceOverride === undefined) {
        delete process.env.TEST_ENABLE_SESSION_PERSISTENCE
      } else {
        process.env.TEST_ENABLE_SESSION_PERSISTENCE = previousPersistenceOverride
      }
      resetProjectForTesting()
      clearSessionMessagesCache()
      await rm(root, { recursive: true, force: true })
    }
  })
})
