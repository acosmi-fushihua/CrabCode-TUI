import type { SDKMessage } from '../entrypoints/agentSdkTypes.js'
import type { Message } from '../types/message.js'

/**
 * The current top-level event denominator yielded by the direct `query()`
 * route. This list is a drift baseline for contract generation and tests; it
 * is deliberately not a runtime allowlist. Additive presentation events must
 * reach the renderer so its compatibility projection can preserve them.
 *
 * This module does not widen the public backend or SDK protocol and does not
 * transform backend messages. It declares the existing process-private
 * renderer transport projection, selects the source-proven renderer-active
 * events, and passes each original object to the private TUI output queue.
 */
export const DIRECT_TUI_RENDERER_EVENT_TYPES = [
  'assistant',
  'user',
  'progress',
  'attachment',
  'system',
  'stream_event',
  'stream_request_start',
  'tombstone',
  'tool_use_summary',
] as const

export type DirectTuiRendererEventType =
  (typeof DIRECT_TUI_RENDERER_EVENT_TYPES)[number]

export type DirectTuiRendererEvent = Message

export type DirectTuiQueryEventSink = (event: Message) => void

type DirectTuiRendererEventEnqueue = (event: DirectTuiRendererEvent) => void
const directTuiRendererEvents = new WeakSet<object>()

/**
 * Publish the exact messages accepted from one composer submission to the
 * process-private renderer observer.
 *
 * The historical interactive renderer appended these `processUserInput`
 * messages to its transcript before starting `query()`. The split-process
 * renderer must observe the same source objects at that boundary because the
 * normal query generator does not yield its initial user message. No observer
 * means the ordinary SDK/headless path remains untouched.
 */
export function publishDirectTuiInputEvents(
  messages: readonly Message[],
  observer: DirectTuiQueryEventSink | undefined,
): void {
  if (!observer) return
  for (const message of messages) observer(message)
}

/**
 * Distinguish renderer events by their observed query() origin, never by
 * guessing from overlapping SDK/internal message shapes. The WeakSet adds no
 * serialized field and preserves the exact source object identity.
 */
export function isDirectTuiRendererEvent(
  event: unknown,
): event is DirectTuiRendererEvent {
  return (
    typeof event === 'object' &&
    event !== null &&
    directTuiRendererEvents.has(event)
  )
}

/**
 * Create the observer installed directly at the `query()` generator boundary.
 * No clone, normalization, SDK envelope, generated UUID, or field filtering is
 * allowed here: identity preservation is deliberate and test-covered. The
 * boundary validates only transport integrity. Capability/presentation
 * compatibility belongs to the Rust projection, not to a duplicated
 * TypeScript event allowlist.
 */
export function createDirectTuiQueryEventSink(
  enqueue: DirectTuiRendererEventEnqueue,
): DirectTuiQueryEventSink {
  return event => {
    if (
      typeof event !== 'object' ||
      event === null ||
      typeof (event as { type?: unknown }).type !== 'string'
    ) {
      throw new Error('Malformed direct query event: missing string type')
    }

    directTuiRendererEvents.add(event)
    enqueue(event)
  }
}

/**
 * QueryEngine still produces two established SDK envelopes needed by the
 * split-process runtime. They are control-plane records, not historical query
 * renderer events:
 *
 * - `system:init` carries authoritative runtime/session metadata.
 * - `result` closes the turn and carries metrics/error state.
 *
 * Every SDK rendering projection is rejected on the direct route so the same
 * assistant/user/stream/tool-summary data cannot be rendered a second time.
 */
export function isDirectTuiControlPlaneSdkMessage(
  message: SDKMessage,
): boolean {
  return (
    message.type === 'result' ||
    (message.type === 'system' && message.subtype === 'init')
  )
}
