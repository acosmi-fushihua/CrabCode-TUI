import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'

const REPO_ROOT = join(import.meta.dir, '..', '..')

function source(path: string): string {
  return readFileSync(join(REPO_ROOT, path), 'utf8')
}

function constArrayBody(
  body: string,
  name: string,
  followingDeclaration: string,
): string {
  const startMarker = `const ${name}: readonly Command[] = [`
  const start = body.indexOf(startMarker)
  const end = body.indexOf(followingDeclaration, start)
  expect(start).toBeGreaterThanOrEqual(0)
  expect(end).toBeGreaterThan(start)
  return body.slice(start + startMarker.length, end)
}

function expectOrdered(body: string, tokens: readonly string[]): void {
  let cursor = -1
  for (const token of tokens) {
    const next = body.indexOf(token, cursor + 1)
    expect(next).toBeGreaterThan(cursor)
    cursor = next
  }
}

/**
 * Evidence pins used to derive this denominator:
 *
 * - fixed historical direct TUI:
 *   2358212c2df2018816058c8a03b1ac3d324e74e0
 * - migration baseline:
 *   a81f848b27bb546b39791e05c77729080582052a
 *
 * The lists below project only prompt commands, explicitly supported local
 * commands, reviewed route-private local adapters, and the exact fixed
 * `/output-style` action that completes without returning JSX. They do not
 * authorize a generic local-jsx direct-TUI protocol.
 */
const PRINT_SDK_BUILTINS = [
  'advisor',
  'compact',
  'compactHistory',
  'context',
  'cost',
  'files',
  'heapDump',
  'init',
  'localModels',
  'prComments',
  'proxy',
  'releaseNotes',
  'review',
  'securityReview',
  'extraUsage',
  'insights',
  '...WORKFLOW_MANAGEMENT_BUILTINS',
  '...ANT_RENDERER_NEUTRAL_BUILTINS',
] as const

const DIRECT_TUI_BUILTINS = [
  'advisor',
  'DIRECT_TUI_CLEAR',
  'compact',
  'compactHistory',
  'cost',
  'files',
  'heapDump',
  'DIRECT_TUI_INSTALL_SLACK_APP',
  'init',
  'localModels',
  'DIRECT_TUI_SMALLMODEL',
  'outputStyle',
  'prComments',
  'proxy',
  'releaseNotes',
  'directTuiStatusline',
  'review',
  'securityReview',
  'DIRECT_TUI_TERMINAL_SETUP',
  'extraUsage',
  'insights',
  'DIRECT_TUI_VISION',
  '...WORKFLOW_MANAGEMENT_BUILTINS',
  "...(feature('PROACTIVE') || feature('KAIROS')",
  '? [DIRECT_TUI_PROACTIVE]',
  '...ANT_RENDERER_NEUTRAL_BUILTINS',
] as const

describe('direct TUI command catalog denominator', () => {
  const catalog = source('src/cli/headlessCommands.ts')
  const printSdk = constArrayBody(
    catalog,
    'HEADLESS_BUILTINS',
    '\n\n/**\n * Interactive direct-TUI projection.',
  )
  const directTui = constArrayBody(
    catalog,
    'DIRECT_TUI_BUILTINS',
    '\n\nconst HEADLESS_BUILTIN_NAMES',
  )

  test('preserves the exact renderer-neutral print/SDK order', () => {
    expectOrdered(printSdk, PRINT_SDK_BUILTINS)
    expect(printSdk).not.toContain('directTuiStatusline')
    expect(printSdk).not.toContain('[logout]')
    expect(printSdk).not.toContain('ultraplan')
  })

  test('adds only reviewed direct-TUI renderer-neutral adapters', () => {
    expectOrdered(directTui, DIRECT_TUI_BUILTINS)
    expect(directTui).not.toContain('context,')
    expect(directTui).not.toContain('ultraplan')
    expect(directTui).not.toContain('login')
    expect(directTui.match(/directTuiStatusline/g)).toHaveLength(1)
    expect(directTui.match(/DIRECT_TUI_INSTALL_SLACK_APP/g)).toHaveLength(1)
    expect(directTui.match(/DIRECT_TUI_PROACTIVE/g)).toHaveLength(1)
    expect(directTui).not.toContain('DIRECT_TUI_RELOAD_PLUGINS')
    expect(directTui.match(/DIRECT_TUI_CLEAR/g)).toHaveLength(1)
    expect(directTui.match(/DIRECT_TUI_SMALLMODEL/g)).toHaveLength(1)
    expect(directTui.match(/DIRECT_TUI_TERMINAL_SETUP/g)).toHaveLength(1)
    expect(directTui.match(/DIRECT_TUI_VISION/g)).toHaveLength(1)
    expect(directTui.match(/outputStyle/g)).toHaveLength(1)
    expect(directTui).not.toContain('[logout]')
    expect(catalog).toContain("'logout',\n  'reload-plugins'")
    expect(catalog).toContain('claimsDirectTuiRendererOwnedInvocation')
    expect(catalog).toContain(
      'DIRECT_TUI_RENDERER_NEUTRAL_LOCAL_JSX.has(command)',
    )
    expect(catalog).toContain(
      'includeRendererNeutralLocalJsx(command)',
    )
    expect(catalog).toContain(
      'Object.getOwnPropertyDescriptors(command)',
    )
    expect(catalog).toContain(
      "Object.defineProperty(projected, 'supportsNonInteractive'",
    )
    expect(catalog).not.toContain(
      "command.type === 'local-jsx' && !command.disableNonInteractive",
    )
    expect(catalog).not.toContain('DIRECT_TUI_EFFORT')
    expect(catalog).not.toContain('src/commands/effort/direct.js')
  })

  test('keeps context renderer-owned on its pre-existing direct control', () => {
    const app = source('crates/crabcode-tui/src/tui_app.rs')
    const runtimeCatalogPrecedence = app.indexOf(
      'self.runtime_catalog_contains(name)',
    )
    const nativeContextBranch = app.indexOf(
      '"/context" if rest.is_empty()',
      runtimeCatalogPrecedence,
    )
    expect(runtimeCatalogPrecedence).toBeGreaterThanOrEqual(0)
    expect(nativeContextBranch).toBeGreaterThan(runtimeCatalogPrecedence)
    expect(app).toContain('"/context" if rest.is_empty()')
    expect(app).toContain(
      'request: json!({"subtype": "get_context_usage"})',
    )
    expect(app).toContain('purpose: OutboundPurpose::ContextUsage')
  })

  test('keeps statusline print metadata and direct-TUI metadata separate', () => {
    const statusline = source('src/commands/statusline.ts')
    expectOrdered(statusline, [
      'const statusline = createStatuslineCommand(true)',
      'export const directTuiStatusline = createStatuslineCommand(false)',
    ])
    expect(statusline).toContain(
      "...(disableNonInteractive ? { disableNonInteractive: true } : {})",
    )
  })

  test('routes catalog refreshes through the selected surface loader', () => {
    const core = source('src/cli/print/queryExecutionCore.ts')
    expect(core).toContain('installDirectTuiCommandSurface()')
    expect(core).toContain('installHeadlessCommandSurface()')
    expect(core).toContain('commandLoader: getDirectTuiCommands')
    expect(core).toContain('commandLoader: getHeadlessCommands')
    expect(core).toContain(
      'commandCatalogLifecycle.refresh(() =>\n      routePolicy.commandLoader(cwd()),',
    )
    expect(core.match(/routePolicy\.commandLoader\(cwd\(\)\)/g)).toHaveLength(
      3,
    )
    expect(core).toContain(
      'refreshControlAuthCatalog(\n                  routePolicy.commandLoader,\n                  cwd(),',
    )
  })

  test('uses the direct catalog for initial and normal TUI setup', () => {
    const bootstrap = source('src/cli/tuiRuntimeBootstrap.ts')
    expect(bootstrap).toContain('installDirectTuiCommandSurface()')
    expect(bootstrap.match(/getDirectTuiCommands\(/g)).toHaveLength(1)
    expect(bootstrap).not.toContain('getHeadlessCommands(')
  })

  test('uses one canonical-plus-alias projector for every catalog producer', () => {
    const handlers = source('src/cli/print/sdkControlHandlers.ts')
    expectOrdered(handlers, [
      'export async function refreshControlAuthCatalog(',
      'commands: projectCommandCatalogEntries(',
      'async function refreshSignedOutControlAuthCatalog(',
      'commands: projectCommandCatalogEntries(',
      'export async function handleInitializeRequest(',
      'commands: projectCommandCatalogEntries(',
    ])
    expect(handlers).not.toContain('commands: commands.map(')

    const query = source('src/cli/print/queryExecutionCore.ts')
    expectOrdered(query, [
      'const commandCatalogLifecycle = new DirectTuiCommandCatalogLifecycle(',
      'projectCommandCatalogEntries(',
      "message.request.subtype === 'reload_plugins'",
      'commands: projectCommandCatalogEntries(',
    ])
    expect(query).not.toContain('commands: currentCommands.map(')

    const projection = source('src/cli/commandCatalogProjection.ts')
    expectOrdered(projection, [
      'if (command.userInvocable === false) continue',
      'for (const name of [command.name, ...(command.aliases ?? [])])',
      'if (claimedInvocationNames.has(name)) continue',
      'claimedInvocationNames.add(name)',
      'entries.push({',
      '...(command.isHidden === true',
      '...(builtInNames.has(name)',
    ])
    expect(projection).not.toContain('.userFacingName')
  })

  test('preserves name-based built-in skill telemetry after command shadowing', () => {
    const skillTool = source('src/tools/SkillTool/SkillTool.ts')
    expect(skillTool).toContain(
      "from '../../utils/builtInCommandNamesProvider.js'",
    )
    expect(
      skillTool.match(
        /getActiveBuiltInCommandNames\(\)\.has\(commandName\)/g,
      ),
    ).toHaveLength(2)
    expect(skillTool).not.toContain("command.source === 'builtin'")
  })

})
