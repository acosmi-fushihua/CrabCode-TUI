import { drainPendingMemoryRunner } from '../turnEnd.js'

export async function drainPendingExtraction(
  timeoutMs?: number,
): Promise<void> {
  await drainPendingMemoryRunner(timeoutMs)
}
