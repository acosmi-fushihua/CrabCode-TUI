import type { Theme } from '../utils/theme.js'

export type NotificationPriority =
  | 'low'
  | 'medium'
  | 'high'
  | 'immediate'

type BaseNotification = {
  key: string
  invalidates?: string[]
  priority: NotificationPriority
  timeoutMs?: number
  fold?: (
    accumulator: Notification,
    incoming: Notification,
  ) => Notification
}

type TextNotification = BaseNotification & {
  text: string
  color?: keyof Theme
}

type JSXNotification = BaseNotification & {
  /** Renderer-owned payload; the backend deliberately treats it as opaque. */
  jsx: unknown
}

/**
 * Backend notification payload retained independently of the removed React
 * notification queue implementation.
 */
export type Notification = TextNotification | JSXNotification
