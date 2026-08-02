/**
 * Peer address parsing — kept separate from peerRegistry.ts so that
 * SendMessageTool can import parseAddress without transitively loading the
 * UDS transport at tool-enumeration time.
 */

/** Parse a URI-style address into scheme + target. */
export function parseAddress(to: string): {
  scheme: 'uds' | 'other'
  target: string
} {
  if (to.startsWith('uds:')) return { scheme: 'uds', target: to.slice(4) }
  // Legacy: old-code UDS senders emit bare socket paths in from=; route them
  // through the UDS branch so replies aren't silently dropped into teammate
  // routing.
  if (to.startsWith('/')) return { scheme: 'uds', target: to }
  return { scheme: 'other', target: to }
}
