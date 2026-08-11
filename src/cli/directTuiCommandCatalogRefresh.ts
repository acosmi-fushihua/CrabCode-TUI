import { z } from 'zod/v4'

import type { CommandCatalogEntry } from './commandCatalogProjection.js'
import { parseSlashCommand } from '../utils/slashCommandParsing.js'

/**
 * Process-private TypeScript -> Rust command-catalog refresh.
 *
 * This intentionally does not enter the public SDK control union. The direct
 * route owns both endpoints and uses it only after the normal initialize
 * response, when a plugin, skill, or account transition changes discovery.
 */
export const DIRECT_TUI_COMMAND_CATALOG_CHANGED_SUBTYPE =
  'crabcode_tui_command_catalog_changed' as const
export const DIRECT_TUI_COMMAND_CATALOG_PROTOCOL_VERSION = 1 as const

const CommandCatalogEntrySchema = z
  .object({
    name: z
      .string()
      .min(1)
      .max(512)
      .refine(name => {
        const parsed = parseSlashCommand('/' + name)
        return (
          parsed !== null &&
          parsed.commandName === name &&
          parsed.args === ''
        )
      }, {
        message: 'command name must roundtrip as one argument-free slash token',
      }),
    description: z.string().max(16_384),
    argumentHint: z.string().max(4_096),
    hidden: z.literal(true).optional(),
    builtin: z.literal(true).optional(),
  })
  .strict()

export const DirectTuiCommandCatalogChangedRequestSchema = z
  .object({
    subtype: z.literal(DIRECT_TUI_COMMAND_CATALOG_CHANGED_SUBTYPE),
    protocol_version: z.literal(DIRECT_TUI_COMMAND_CATALOG_PROTOCOL_VERSION),
    commands: z.array(CommandCatalogEntrySchema).max(4_096),
  })
  .strict()
  .superRefine((request, context) => {
    const names = new Set<string>()
    for (const [index, command] of request.commands.entries()) {
      if (names.has(command.name)) {
        context.addIssue({
          code: 'custom',
          path: ['commands', index, 'name'],
          message: `duplicate command name: ${command.name}`,
        })
      }
      names.add(command.name)
    }
  })

export const DirectTuiCommandCatalogChangedResponseSchema = z
  .object({
    protocol_version: z.literal(DIRECT_TUI_COMMAND_CATALOG_PROTOCOL_VERSION),
    received: z.literal(true),
  })
  .strict()

export type DirectTuiCommandCatalogChangedRequest = z.infer<
  typeof DirectTuiCommandCatalogChangedRequestSchema
>
export type DirectTuiCommandCatalogChangedResponse = z.infer<
  typeof DirectTuiCommandCatalogChangedResponseSchema
>

export function buildDirectTuiCommandCatalogChangedRequest(
  commands: readonly CommandCatalogEntry[],
): DirectTuiCommandCatalogChangedRequest {
  return DirectTuiCommandCatalogChangedRequestSchema.parse({
    subtype: DIRECT_TUI_COMMAND_CATALOG_CHANGED_SUBTYPE,
    protocol_version: DIRECT_TUI_COMMAND_CATALOG_PROTOCOL_VERSION,
    commands,
  })
}

type CommandCatalogSender = (
  commands: readonly CommandCatalogEntry[],
) => Promise<unknown>

type CommandCatalogRefreshTicket<TCommand> = {
  /**
   * Commit the loaded snapshot if no newer refresh has already committed.
   * Returns false after close, after another commit wins, or on a second call.
   */
  commit(commands: readonly TCommand[]): boolean
}

/**
 * Coordinates every producer that can replace the process command registry.
 *
 * Plugin installation, skill watching, manual reload, and authentication can
 * all load commands asynchronously. A slow, older load must not overwrite a
 * newer snapshot that already reached the renderer. Revisions are assigned
 * when work starts and only the newest successfully committed revision wins;
 * a newer failed load does not suppress an older successful result.
 *
 * The class deliberately owns no transport. `onCommit` is the one integration
 * seam that updates the query loop's registry and feeds the publisher.
 */
export class DirectTuiCommandCatalogLifecycle<TCommand> {
  private nextRevision = 0
  private committedRevision = 0
  private closed = false
  private commands: readonly TCommand[]

  constructor(
    initialCommands: readonly TCommand[],
    private readonly onCommit: (commands: readonly TCommand[]) => void,
  ) {
    this.commands = initialCommands.slice()
  }

  snapshot(): readonly TCommand[] {
    return this.commands
  }

  beginRefresh(): CommandCatalogRefreshTicket<TCommand> {
    const revision = ++this.nextRevision
    let settled = false
    return {
      commit: commands => {
        if (settled) return false
        settled = true
        if (this.closed || revision <= this.committedRevision) return false

        const snapshot = commands.slice()
        this.commands = snapshot
        this.committedRevision = revision
        this.onCommit(snapshot)
        return true
      },
    }
  }

  async refresh(
    load: () => Promise<readonly TCommand[]>,
  ): Promise<readonly TCommand[]> {
    const ticket = this.beginRefresh()
    const commands = await load()
    ticket.commit(commands)
    return this.snapshot()
  }

  replace(commands: readonly TCommand[]): boolean {
    return this.beginRefresh().commit(commands)
  }

  close(): void {
    this.closed = true
  }
}

/**
 * Serial, latest-wins publisher for background discovery changes.
 *
 * Discovery can fire before initialize and can fire again while Rust is
 * acknowledging an earlier snapshot. Keeping one in-flight request and one
 * replaceable pending snapshot prevents stale completion from winning while
 * preserving StructuredIO's sole outbound FIFO.
 */
export class DirectTuiCommandCatalogPublisher {
  private ready = false
  private closed = false
  private dirty = false
  private latest: readonly CommandCatalogEntry[] = []
  private drainPromise: Promise<void> | undefined

  constructor(
    private readonly send: CommandCatalogSender,
    private readonly onError: (error: unknown) => void,
  ) {}

  update(commands: readonly CommandCatalogEntry[]): void {
    if (this.closed) return
    this.latest = commands.map(command => ({ ...command }))
    this.dirty = true
    this.startDrain()
  }

  markInitialized(): void {
    if (this.closed) return
    this.ready = true
    this.startDrain()
  }

  close(): void {
    this.closed = true
    this.dirty = false
  }

  async whenIdle(): Promise<void> {
    for (;;) {
      const current = this.drainPromise
      if (!current) return
      await current
    }
  }

  private startDrain(): void {
    if (
      !this.ready ||
      this.closed ||
      !this.dirty ||
      this.drainPromise
    ) {
      return
    }
    // Defer the reverse request by one microtask. Authentication and manual
    // reload handlers commit their registry snapshot and enqueue the owning
    // correlated response in the same stack; that response must reach Rust
    // before the follow-up discovery notification so account/catalog state is
    // never presented as a half-transition.
    const drain = Promise.resolve().then(() => this.drain())
    this.drainPromise = drain
    void drain.finally(() => {
      if (this.drainPromise === drain) this.drainPromise = undefined
      this.startDrain()
    })
  }

  private async drain(): Promise<void> {
    while (this.ready && !this.closed && this.dirty) {
      this.dirty = false
      const snapshot = this.latest
      try {
        await this.send(snapshot)
      } catch (error) {
        try {
          this.onError(error)
        } catch {
          // Diagnostics must never turn a handled transport failure into an
          // unhandled rejection or restart the drain loop.
        }
        // Do not spin on a broken/closed transport. A later discovery change
        // calls update() and gives the channel one fresh attempt.
        return
      }
    }
  }
}
