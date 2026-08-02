import {
  Command,
  InvalidArgumentError,
  Option,
} from '@commander-js/extra-typings'

export type TuiRuntimeOptions = {
  debug?: boolean | string
  debugToStderr?: boolean
  debugFile?: string
  verbose?: boolean
  print?: boolean
  bare?: boolean
  init?: boolean
  initOnly?: boolean
  maintenance?: boolean
  outputFormat?: 'text' | 'json' | 'stream-json'
  inputFormat?: 'text' | 'stream-json'
  mcpDebug?: boolean
  jsonSchema?: string
  includeHookEvents?: boolean
  includePartialMessages?: boolean
  replayUserMessages?: boolean
  enableAuthStatus?: boolean
  dangerouslySkipPermissions?: boolean
  allowDangerouslySkipPermissions?: boolean
  delegatePermissions?: boolean
  dangerouslySkipPermissionsWithClassifiers?: boolean
  afk?: boolean
  enableAutoMode?: boolean
  thinking?: 'enabled' | 'adaptive' | 'disabled'
  maxThinkingTokens?: number
  maxTurns?: number
  maxBudgetUsd?: number
  taskBudget?: number
  allowedTools?: string[]
  tools?: string[]
  disallowedTools?: string[]
  mcpConfig?: string[]
  permissionPromptTool?: string
  systemPrompt?: string
  systemPromptFile?: string
  appendSystemPrompt?: string
  appendSystemPromptFile?: string
  permissionMode?: string
  continue?: boolean
  resume?: string | boolean
  forkSession?: boolean
  sessionPersistence?: boolean
  resumeSessionAt?: string
  rewindFiles?: string
  model?: string
  effort?: 'low' | 'medium' | 'high' | 'max'
  agent?: string
  betas?: string[]
  fallbackModel?: string
  workload?: string
  settings?: string
  addDir?: string[]
  ide?: boolean
  strictMcpConfig?: boolean
  sessionId?: string
  name?: string
  agents?: string
  settingSources?: string
  pluginDir?: string[]
  disableSlashCommands?: boolean
  file?: string[]
  advisor?: string
  agentTeams?: boolean
  goal?: boolean
  coordinator?: boolean
  proactive?: boolean
  brief?: boolean
  messagingSocketPath?: string
  worktree?: string | boolean
  tmux?: boolean
  tasks?: string | boolean
  channels?: string[]
  dangerouslyLoadDevelopmentChannels?: string[]
  agentId?: string
  agentName?: string
  teamName?: string
  agentColor?: string
  planModeRequired?: boolean
  parentSessionId?: string
  teammateMode?: string
  agentType?: string
  hardFail?: boolean
}

function positiveNumber(flag: string): (value: string) => number {
  return value => {
    const parsed = Number(value)
    if (!Number.isFinite(parsed) || parsed <= 0) {
      throw new InvalidArgumentError(`${flag} must be greater than 0`)
    }
    return parsed
  }
}

function positiveInteger(flag: string): (value: string) => number {
  return value => {
    const parsed = Number(value)
    if (!Number.isSafeInteger(parsed) || parsed <= 0) {
      throw new InvalidArgumentError(`${flag} must be a positive integer`)
    }
    return parsed
  }
}

function collect(value: string, previous: string[]): string[] {
  return [...previous, value]
}

/**
 * Parse the backend arguments forwarded by the native CrabCode TUI.
 *
 * The private runtime always uses verbose stream-json in both directions. The
 * renderer owns presentation; callers cannot downgrade the child protocol.
 */
export function parseTuiRuntimeOptions(
  argv: readonly string[] = process.argv,
): TuiRuntimeOptions {
  const program = new Command()
    .name('crabcode-tui-runtime')
    .allowExcessArguments(false)
    .enablePositionalOptions()
    .option('-d, --debug [filter]')
    .addOption(new Option('--debug-to-stderr').argParser(Boolean).hideHelp())
    .option('--debug-file <path>')
    .option('--verbose')
    .option('-p, --print')
    .option('--bare')
    .addOption(new Option('--init').hideHelp())
    .addOption(new Option('--init-only').hideHelp())
    .addOption(new Option('--maintenance').hideHelp())
    .addOption(
      new Option('--output-format <format>')
        .choices(['text', 'json', 'stream-json'])
        .hideHelp(),
    )
    .addOption(
      new Option('--input-format <format>')
        .choices(['text', 'stream-json'])
        .hideHelp(),
    )
    .option('--mcp-debug')
    .option('--json-schema <schema>')
    .option('--include-hook-events')
    .option('--include-partial-messages')
    .option('--replay-user-messages')
    .addOption(new Option('--enable-auth-status').hideHelp())
    .option('--dangerously-skip-permissions')
    .option('--allow-dangerously-skip-permissions')
    .addOption(
      new Option('--delegate-permissions').implies({
        permissionMode: 'auto',
      }),
    )
    .addOption(
      new Option('--dangerously-skip-permissions-with-classifiers')
        .implies({ permissionMode: 'auto' })
        .hideHelp(),
    )
    .addOption(
      new Option('--afk')
        .implies({ permissionMode: 'auto' })
        .hideHelp(),
    )
    .addOption(new Option('--enable-auto-mode').hideHelp())
    .addOption(
      new Option('--thinking <mode>')
        .choices(['enabled', 'adaptive', 'disabled'])
        .hideHelp(),
    )
    .addOption(
      new Option('--max-thinking-tokens <tokens>')
        .argParser(Number)
        .hideHelp(),
    )
    .addOption(
      new Option('--max-turns <turns>')
        .argParser(positiveInteger('--max-turns'))
        .hideHelp(),
    )
    .addOption(
      new Option('--max-budget-usd <amount>')
        .argParser(positiveNumber('--max-budget-usd'))
        .hideHelp(),
    )
    .addOption(
      new Option('--task-budget <tokens>')
        .argParser(positiveInteger('--task-budget'))
        .hideHelp(),
    )
    .option('--allowedTools, --allowed-tools <tools...>')
    .option('--tools <tools...>')
    .option('--disallowedTools, --disallowed-tools <tools...>')
    .option('--mcp-config <configs...>')
    .addOption(new Option('--permission-prompt-tool <tool>').hideHelp())
    .option('--system-prompt <prompt>')
    .addOption(new Option('--system-prompt-file <file>').hideHelp())
    .option('--append-system-prompt <prompt>')
    .addOption(new Option('--append-system-prompt-file <file>').hideHelp())
    .option('--permission-mode <mode>')
    .option('-c, --continue')
    .option('-r, --resume [value]', 'Resume a session', value => value || true)
    .option('--fork-session')
    .option('--no-session-persistence')
    .addOption(new Option('--resume-session-at <message-id>').hideHelp())
    .addOption(new Option('--rewind-files <user-message-id>').hideHelp())
    .option('--model <model>')
    .addOption(
      new Option('--effort <level>').choices([
        'low',
        'medium',
        'high',
        'max',
      ]),
    )
    .option('--agent <agent>')
    .option('--betas <betas...>')
    .option('--fallback-model <model>')
    .addOption(new Option('--workload <tag>').hideHelp())
    .option('--settings <file-or-json>')
    .option('--add-dir <directories...>')
    .option('--ide')
    .option('--strict-mcp-config')
    .option('--session-id <uuid>')
    .option('-n, --name <name>')
    .option('--agents <json>')
    .option('--setting-sources <sources>')
    .option('--plugin-dir <path>', 'Load an inline plugin directory', collect, [])
    .option('--disable-slash-commands')
    .option('--file <specs...>')
    .option('--advisor <model>')
    .addOption(new Option('--agent-teams').hideHelp())
    .addOption(new Option('--goal').hideHelp())
    .addOption(new Option('--coordinator').hideHelp())
    .addOption(new Option('--proactive').hideHelp())
    .addOption(new Option('--brief').hideHelp())
    .addOption(new Option('--messaging-socket-path <path>').hideHelp())
    .option('-w, --worktree [name]')
    .option('--tmux')
    .addOption(
      new Option('--tasks [id]')
        .argParser(value => value || true)
        .hideHelp(),
    )
    .option('--channels <channels...>')
    .option(
      '--dangerously-load-development-channels <channels...>',
    )
    .addOption(new Option('--agent-id <id>').hideHelp())
    .addOption(new Option('--agent-name <name>').hideHelp())
    .addOption(new Option('--team-name <name>').hideHelp())
    .addOption(new Option('--agent-color <color>').hideHelp())
    .addOption(new Option('--plan-mode-required').hideHelp())
    .addOption(new Option('--parent-session-id <id>').hideHelp())
    .addOption(
      new Option('--teammate-mode <mode>')
        .choices(['auto', 'tmux', 'in-process'])
        .hideHelp(),
    )
    .addOption(new Option('--agent-type <type>').hideHelp())
    .addOption(new Option('--hard-fail').hideHelp())

  program.parse([...argv], { from: 'node' })
  const options = program.opts() as TuiRuntimeOptions
  return options
}
