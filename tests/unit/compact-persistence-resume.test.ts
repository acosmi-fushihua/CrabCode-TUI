import { describe, expect, test } from 'bun:test'
import { randomUUID } from 'node:crypto'
import {
  mkdir,
  mkdtemp,
  readFile,
  rm,
  stat,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import {
  attachAnalyticsSink,
  type AnalyticsSink,
} from '../../src/services/analytics/index.js'
import { scanCompactBoundaries } from '../../src/commands/compact-history/historyCore.js'
import { loadMessagesFromJsonlPath } from '../../src/utils/conversationRecovery.js'
import {
  loadTranscriptFile,
} from '../../src/utils/sessionStorage-transcript.js'
import type { TranscriptMessage } from '../../src/types/logs.js'
import {
  clearSessionMessagesCache,
  resetProjectForTesting,
} from '../../src/utils/sessionStorage.js'
import { readLastJsonEvidence } from '../helpers/readLastJsonEvidence.js'

const REPO_ROOT = join(import.meta.dir, '..', '..')
const WRITER_FIXTURE = join(
  REPO_ROOT,
  'tests',
  'fixtures',
  'compact-persistence-writer.ts',
)

type WriterEvidence = {
  sessionId: string
  boundaryUuid: string
  summaryUuid: string
  oldUserUuid: string
  oldAssistantUuid: string
  keptUserUuid: string
  keptAssistantUuid: string
  terminalResult?: {
    type?: string
    subtype?: string
    is_error?: boolean
  }
}

type JsonlEntry = {
  type?: string
  subtype?: string
  uuid?: string
  timestamp?: string
  parentUuid?: string | null
  logicalParentUuid?: string | null
  message?: { content?: unknown }
  compactMetadata?: {
    trigger?: string
    preTokens?: number
    messagesSummarized?: number
    preservedSegment?: {
      headUuid: unknown
      anchorUuid: unknown
      tailUuid: unknown
    }
  }
}

type PreservedSegment = NonNullable<
  NonNullable<JsonlEntry['compactMetadata']>['preservedSegment']
>

function parseJsonl(content: string): JsonlEntry[] {
  return content
    .split('\n')
    .filter(line => line.trim().length > 0)
    .map(line => JSON.parse(line) as JsonlEntry)
}

function mutatePreservedSegment(
  content: string,
  mutate: (segment: PreservedSegment) => void,
): string {
  return content
    .split('\n')
    .filter(line => line.trim().length > 0)
    .map(line => {
      const entry = JSON.parse(line) as JsonlEntry
      if (entry.type === 'system' && entry.subtype === 'compact_boundary') {
        const segment = entry.compactMetadata?.preservedSegment
        if (!segment) throw new Error('fixture boundary omitted preservedSegment')
        mutate(segment)
      }
      return JSON.stringify(entry)
    })
    .join('\n')
}

function messageText(message: TranscriptMessage | undefined): string {
  if (!message || !('message' in message)) return ''
  return JSON.stringify(message.message.content)
}

describe('compact transcript persistence and cross-process resume', () => {
  test('flushes the logical compact sequence, resumes summary + preserved segment, retains raw history, and fails soft on every bad relink shape', async () => {
    const root = await mkdtemp(join(tmpdir(), 'crabcode-compact-persist-'))
    const configDir = join(root, 'config')
    const homeDir = join(root, 'home')
    const projectDir = join(root, 'project')
    const sessionId = randomUUID()
    const transcriptPath = join(projectDir, `${sessionId}.jsonl`)
    const evidencePath = join(root, 'writer-evidence.json')
    const errorPath = join(root, 'writer.stderr')
    await Promise.all([
      mkdir(configDir, { recursive: true }),
      mkdir(homeDir, { recursive: true }),
      mkdir(projectDir, { recursive: true }),
    ])
    await Promise.all([
      writeFile(
        join(configDir, '.crabcode.json'),
        JSON.stringify({ theme: 'dark', hasCompletedOnboarding: true }),
      ),
      writeFile(
        join(configDir, 'settings.json'),
        JSON.stringify({ autoMemoryEnabled: false, disableAllHooks: true }),
      ),
    ])

    try {
      const writer = Bun.spawn({
        cmd: [process.execPath, WRITER_FIXTURE],
        cwd: REPO_ROOT,
        env: {
          ...process.env,
          HOME: homeDir,
          COMPACT_CONFIG_DIR: configDir,
          COMPACT_SESSION_ID: sessionId,
          COMPACT_TRANSCRIPT_PATH: transcriptPath,
          CRABCODE_DISABLE_AUTO_MEMORY: '1',
          CRABCODE_DISABLE_TELEMETRY: '1',
          CRABCODE_FEATURE_COORDINATOR_MODE: '0',
          DISABLE_BACKGROUND_TASKS: '1',
        },
        stdout: Bun.file(evidencePath),
        stderr: Bun.file(errorPath),
      })
      const exitCode = await writer.exited
      const stderr = await readFile(errorPath, 'utf8')
      expect(exitCode, stderr).toBe(0)
      const evidence = await readLastJsonEvidence<WriterEvidence>(evidencePath)
      expect(evidence.terminalResult).toMatchObject({
        type: 'result',
        subtype: 'success',
        is_error: false,
      })

      // The child has flushed, reset its Project singleton, and exited. Every
      // assertion below is therefore a real disk/read-process boundary.
      const transcriptStat = await stat(transcriptPath)
      expect(transcriptStat.size).toBeGreaterThan(0)
      expect(transcriptStat.mode & 0o077).toBe(0)
      const raw = await readFile(transcriptPath, 'utf8')
      const entries = parseJsonl(raw)
      expect(entries.length).toBeGreaterThanOrEqual(9)

      const byUuid = new Map(
        entries.flatMap(entry =>
          entry.uuid ? ([[entry.uuid, entry]] as const) : [],
        ),
      )
      const boundary = byUuid.get(evidence.boundaryUuid)
      const summary = byUuid.get(evidence.summaryUuid)
      expect(boundary).toMatchObject({
        type: 'system',
        subtype: 'compact_boundary',
        parentUuid: null,
        logicalParentUuid: evidence.keptAssistantUuid,
        compactMetadata: {
          trigger: 'manual',
          preTokens: 12_345,
          messagesSummarized: 2,
          preservedSegment: {
            headUuid: evidence.keptUserUuid,
            anchorUuid: evidence.summaryUuid,
            tailUuid: evidence.keptAssistantUuid,
          },
        },
      })
      expect(summary?.parentUuid).toBe(evidence.boundaryUuid)
      expect(
        entries.filter(entry => entry.uuid === evidence.keptUserUuid),
      ).toHaveLength(1)
      expect(
        entries.filter(entry => entry.uuid === evidence.keptAssistantUuid),
      ).toHaveLength(1)

      // Original pre-compaction content is physically retained for transcript
      // inspection even though it must no longer enter the resumed model view.
      expect(raw).toContain('ORIGINAL-OLD-USER')
      expect(raw).toContain('ORIGINAL-OLD-ASSISTANT')
      expect(raw).toContain('PRESERVED-USER')
      expect(raw).toContain('PRESERVED-ASSISTANT')

      resetProjectForTesting()
      clearSessionMessagesCache()
      const resumed = await loadMessagesFromJsonlPath(transcriptPath)
      const resumedIds = resumed.messages.map(message => message.uuid)
      expect(resumed.sessionId).toBe(evidence.sessionId)
      expect(resumedIds.slice(0, 4)).toEqual([
        evidence.boundaryUuid,
        evidence.summaryUuid,
        evidence.keptUserUuid,
        evidence.keptAssistantUuid,
      ])
      expect(resumedIds).not.toContain(evidence.oldUserUuid)
      expect(resumedIds).not.toContain(evidence.oldAssistantUuid)
      const resumedText = JSON.stringify(resumed.messages)
      expect(resumedText).toContain('COMPACT-SUMMARY')
      expect(resumedText).toContain('PRESERVED-USER')
      expect(resumedText).toContain('PRESERVED-ASSISTANT')
      expect(resumedText).not.toContain('ORIGINAL-OLD-USER')
      expect(resumedText).not.toContain('ORIGINAL-OLD-ASSISTANT')

      // /compact-history scans the append-only physical transcript, not the
      // pruned resume projection, so the compaction event remains inspectable.
      expect(scanCompactBoundaries(raw)).toEqual([
        {
          timestamp: boundary?.timestamp,
          trigger: 'manual',
          preTokens: 12_345,
          messagesSummarized: 2,
        },
      ])

      const telemetry: Array<{
        eventName: string
        metadata: Record<string, boolean | number | undefined>
      }> = []
      const sink: AnalyticsSink = {
        logEvent: (eventName, metadata) => {
          telemetry.push({ eventName, metadata })
        },
        logEventAsync: async (eventName, metadata) => {
          telemetry.push({ eventName, metadata })
        },
      }
      attachAnalyticsSink(sink)
      // Drain analytics queued by module initialization before isolating the
      // stable relink reason codes emitted by the corrupt variants below.
      await Promise.resolve()
      telemetry.length = 0

      const physicalMessageIds = entries.flatMap(entry =>
        entry.uuid &&
        ['user', 'assistant', 'attachment', 'system'].includes(entry.type ?? '')
          ? [entry.uuid]
          : [],
      )
      const postBoundaryLeaf = entries.findLast(
        entry => entry.uuid && entry.parentUuid !== null,
      )?.uuid
      if (!postBoundaryLeaf) throw new Error('fixture omitted post-boundary leaf')

      const corruptions: Array<{
        reason: number
        mutate: Parameters<typeof mutatePreservedSegment>[1]
      }> = [
        {
          reason: 1,
          mutate: segment => {
            segment.headUuid = ''
          },
        },
        {
          reason: 2,
          mutate: segment => {
            segment.tailUuid = randomUUID()
          },
        },
        {
          reason: 3,
          mutate: segment => {
            segment.anchorUuid = randomUUID()
          },
        },
        {
          reason: 4,
          mutate: segment => {
            segment.anchorUuid = evidence.oldUserUuid
          },
        },
        {
          reason: 5,
          mutate: segment => {
            segment.anchorUuid = evidence.keptUserUuid
          },
        },
        {
          reason: 6,
          mutate: segment => {
            segment.headUuid = evidence.summaryUuid
            segment.anchorUuid = evidence.boundaryUuid
            segment.tailUuid = postBoundaryLeaf
          },
        },
      ]

      for (const corruption of corruptions) {
        const corruptRaw = mutatePreservedSegment(raw, corruption.mutate)
        const eventCountBefore = telemetry.length
        const loaded = await loadTranscriptFile(transcriptPath, {
          sourceBuffer: Buffer.from(corruptRaw),
        })

        // Fail-soft means no partial relink/prune: all physical history stays
        // available and the original parents remain untouched.
        expect([...loaded.messages.keys()].sort()).toEqual(
          [...physicalMessageIds].sort(),
        )
        expect(loaded.messages.get(evidence.keptUserUuid)?.parentUuid).toBe(
          evidence.oldAssistantUuid,
        )
        expect(
          [...loaded.messages.values()].some(
            message => message.parentUuid === evidence.summaryUuid,
          ),
        ).toBe(true)
        expect(messageText(loaded.messages.get(evidence.oldUserUuid))).toContain(
          'ORIGINAL-OLD-USER',
        )

        const emitted = telemetry
          .slice(eventCountBefore)
          .filter(event => event.eventName === 'tengu_relink_walk_broken')
        expect(emitted).toHaveLength(1)
        expect(emitted[0]?.metadata.reason).toBe(corruption.reason)
      }
    } finally {
      resetProjectForTesting()
      clearSessionMessagesCache()
      await rm(root, { recursive: true, force: true })
    }
  }, 30_000)
})
