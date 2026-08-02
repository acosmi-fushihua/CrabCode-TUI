import type { ContentBlockParam, TextBlockParam } from '../types/api-types.js'
import type { Message, UserMessage } from '../types/message.js'
import {
  BASH_STDERR_TAG,
  BASH_STDOUT_TAG,
  LOCAL_COMMAND_STDERR_TAG,
  LOCAL_COMMAND_STDOUT_TAG,
  TASK_NOTIFICATION_TAG,
  TEAMMATE_MESSAGE_TAG,
  TICK_TAG,
} from '../constants/xml.js'
import { isSyntheticMessage } from './messages.js'

export function isTextBlock(
  block: ContentBlockParam,
): block is TextBlockParam {
  return block.type === 'text'
}

/**
 * Backend-owned predicate shared by the legacy message picker and QueryEngine.
 * Keeping it outside the React component prevents the direct runtime from
 * importing the old renderer merely to classify transcript messages.
 */
export function selectableUserMessagesFilter(
  message: Message,
): message is UserMessage {
  if (message.type !== 'user') return false
  if (
    Array.isArray(message.message.content) &&
    message.message.content[0]?.type === 'tool_result'
  ) {
    return false
  }
  if (
    isSyntheticMessage(message) ||
    message.isMeta ||
    message.isCompactSummary ||
    message.isVisibleInTranscriptOnly
  ) {
    return false
  }

  const content = message.message.content
  const lastBlock =
    typeof content === 'string' ? null : content[content.length - 1]
  const messageText =
    typeof content === 'string'
      ? content.trim()
      : lastBlock && isTextBlock(lastBlock)
        ? lastBlock.text.trim()
        : ''

  return ![
    `<${LOCAL_COMMAND_STDOUT_TAG}>`,
    `<${LOCAL_COMMAND_STDERR_TAG}>`,
    `<${BASH_STDOUT_TAG}>`,
    `<${BASH_STDERR_TAG}>`,
    `<${TASK_NOTIFICATION_TAG}>`,
    `<${TICK_TAG}>`,
    `<${TEAMMATE_MESSAGE_TAG}`,
  ].some(tag => messageText.includes(tag))
}
