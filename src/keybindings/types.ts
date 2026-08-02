/**
 * Core TypeScript types for the keybindings system.
 */

import {
  KEYBINDING_ACTIONS,
  KEYBINDING_CONTEXTS,
} from './schema.js'

/**
 * All valid keybinding action identifiers (e.g. 'app:exit', 'chat:submit').
 */
export type KeybindingAction = (typeof KEYBINDING_ACTIONS)[number]

/**
 * All valid keybinding context names (e.g. 'Global', 'Chat', 'Confirmation').
 */
export type KeybindingContextName = (typeof KEYBINDING_CONTEXTS)[number]

/**
 * A chord represents a sequence of one or more parsed keystrokes.
 */
export type Chord = ParsedKeystroke[]

/**
 * A single parsed keystroke (one key event with optional modifiers).
 */
export interface ParsedKeystroke {
  key: string
  ctrl?: boolean
  meta?: boolean
  shift?: boolean
  alt?: boolean
  super?: boolean
}

/**
 * A fully parsed keybinding: a sequence of keystrokes mapped to an action.
 */
export interface ParsedBinding {
  context: KeybindingContextName
  chord: ParsedKeystroke[]
  action: string
}

/**
 * A raw (unparsed) block of keybindings for a given context,
 * as stored in keybindings.json or the default bindings array.
 */
export interface KeybindingBlock {
  context: KeybindingContextName
  bindings: Record<string, KeybindingAction | string | null>
}
