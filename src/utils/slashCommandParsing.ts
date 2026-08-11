/**
 * Centralized utilities for parsing slash commands
 */

export type ParsedSlashCommand = {
  commandName: string
  args: string
  isMcp: boolean
}

/**
 * Parses a slash command input string into its component parts
 *
 * @param input - The raw input string (should start with '/')
 * @returns Parsed command name, args, and MCP flag, or null if invalid
 *
 * @example
 * parseSlashCommand('/search foo bar')
 * // => { commandName: 'search', args: 'foo bar', isMcp: false }
 *
 * @example
 * parseSlashCommand('/mcp:tool (MCP) arg1 arg2')
 * // => { commandName: 'mcp:tool (MCP)', args: 'arg1 arg2', isMcp: true }
 */
export function parseSlashCommand(input: string): ParsedSlashCommand | null {
  const trimmedInput = input.trim()

  // Check if input starts with '/'
  if (!trimmedInput.startsWith('/')) {
    return null
  }

  // Remove the leading slash and split at the first ECMAScript whitespace
  // code point. The Rust direct-TUI router uses the same whitespace family;
  // accepting only ASCII space here made `/command\targ` and Unicode-space
  // variants route to the runtime and then fail as an unknown command.
  const withoutSlash = trimmedInput.slice(1)
  const separatorIndex = withoutSlash.search(/\s/u)
  const firstWord =
    separatorIndex === -1
      ? withoutSlash
      : withoutSlash.slice(0, separatorIndex)

  if (!firstWord) {
    return null
  }

  let commandName = firstWord
  let isMcp = false
  let args =
    separatorIndex === -1 ? '' : withoutSlash.slice(separatorIndex + 1)

  // Check for MCP commands (second token is '(MCP)'). Preserve the historical
  // argument tail after consuming one separator while accepting every
  // whitespace separator supported by the direct renderer.
  const mcpRest = args.replace(/^\s+/u, '')
  if (mcpRest === '(MCP)' || /^\(MCP\)\s/u.test(mcpRest)) {
    commandName = commandName + ' (MCP)'
    isMcp = true
    args = mcpRest.slice('(MCP)'.length).replace(/^\s/u, '')
  }

  return {
    commandName,
    args,
    isMcp,
  }
}
