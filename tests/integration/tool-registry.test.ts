/**
 * Tool Registry Integration Tests
 *
 * Validates the core Tool protocol utilities: buildTool defaults, findToolByName,
 * toolMatchesName, and schema contracts.
 *
 * Individual tool modules cannot be imported in isolation without a full
 * `bun install` (transitive deps: react, opentelemetry, etc.). We test the
 * registry infrastructure through Tool.ts, which has minimal dependencies.
 *
 * NOTE: buildTool spreads the definition object, which eagerly invokes getters
 * (including inputSchema). Since zod/v4 is not resolvable from the tests/
 * directory in the current unbundled environment, we use plain objects for
 * testing findToolByName and toolMatchesName, and test buildTool only with
 * a pre-constructed inputSchema value.
 */

import { describe, test, expect, beforeAll, afterAll } from 'bun:test'
import {
  buildTool,
  findToolByName,
  toolMatchesName,
  getEmptyToolPermissionContext,
  filterToolProgressMessages,
  type Tool,
  type Tools,
} from '../../src/Tool.js'

// A stub inputSchema that satisfies the AnyObject constraint (z.ZodType<{[key:string]:unknown}>)
// without requiring zod. buildTool only stores it; the tests never parse with it.
const stubSchema = {
  _def: {},
  _input: {} as { [key: string]: unknown },
  _output: {} as { [key: string]: unknown },
  parse: (v: unknown) => v as { [key: string]: unknown },
  safeParse: (v: unknown) => ({ success: true, data: v }),
} as any

describe('Tool Registry', () => {

  describe('buildTool defaults', () => {
    const minimalTool = buildTool({
      name: 'TestTool',
      maxResultSizeChars: 1000,
      inputSchema: stubSchema,
      async description() { return 'A test tool' },
      async prompt() { return 'Test prompt' },
      async call(args: any) { return { data: { result: args.input } } },
      renderToolUseMessage() { return null },
      mapToolResultToToolResultBlockParam(data: any, toolUseID: string) {
        return { tool_use_id: toolUseID, type: 'tool_result' as const, content: String(data.result) }
      },
    })

    test('has the given name', () => {
      expect(minimalTool.name).toBe('TestTool')
    })

    test('isEnabled defaults to () => true', () => {
      expect(minimalTool.isEnabled()).toBe(true)
    })

    test('isConcurrencySafe defaults to () => false', () => {
      expect(minimalTool.isConcurrencySafe({})).toBe(false)
    })

    test('isReadOnly defaults to () => false', () => {
      expect(minimalTool.isReadOnly({})).toBe(false)
    })

    test('isDestructive defaults to () => false', () => {
      expect(minimalTool.isDestructive!({})).toBe(false)
    })

    test('checkPermissions defaults to allow', async () => {
      const input = { input: 'test' }
      const result = await minimalTool.checkPermissions(input, {} as any)
      expect(result.behavior).toBe('allow')
    })

    test('userFacingName defaults to tool name', () => {
      expect(minimalTool.userFacingName(undefined)).toBe('TestTool')
    })

    test('maxResultSizeChars is preserved', () => {
      expect(minimalTool.maxResultSizeChars).toBe(1000)
    })

    test('has description function', () => {
      expect(typeof minimalTool.description).toBe('function')
    })

    test('has call function', () => {
      expect(typeof minimalTool.call).toBe('function')
    })

    test('has prompt function', () => {
      expect(typeof minimalTool.prompt).toBe('function')
    })

    test('has inputSchema', () => {
      expect(minimalTool.inputSchema).toBeTruthy()
    })

    test('has mapToolResultToToolResultBlockParam', () => {
      expect(typeof minimalTool.mapToolResultToToolResultBlockParam).toBe('function')
    })

    test('has renderToolUseMessage', () => {
      expect(typeof minimalTool.renderToolUseMessage).toBe('function')
    })
  })

  describe('buildTool with overrides', () => {
    const customTool = buildTool({
      name: 'CustomTool',
      maxResultSizeChars: 5000,
      inputSchema: stubSchema,
      async description() { return 'Custom tool' },
      async prompt() { return 'Custom prompt' },
      async call() { return { data: {} } },
      isEnabled() { return false },
      isConcurrencySafe() { return true },
      isReadOnly() { return true },
      userFacingName() { return 'Custom' },
      renderToolUseMessage() { return null },
      mapToolResultToToolResultBlockParam(_data: any, toolUseID: string) {
        return { tool_use_id: toolUseID, type: 'tool_result' as const, content: '' }
      },
    })

    test('isEnabled override works', () => {
      expect(customTool.isEnabled()).toBe(false)
    })

    test('isConcurrencySafe override works', () => {
      expect(customTool.isConcurrencySafe({})).toBe(true)
    })

    test('isReadOnly override works', () => {
      expect(customTool.isReadOnly({})).toBe(true)
    })

    test('userFacingName override works', () => {
      expect(customTool.userFacingName(undefined)).toBe('Custom')
    })
  })

  describe('findToolByName and toolMatchesName', () => {
    // Create tool-like objects for testing the lookup utilities.
    // These don't need to be full Tool instances — just need name and aliases.
    const toolA = buildTool({
      name: 'ToolA',
      aliases: ['AliasA1', 'AliasA2'],
      maxResultSizeChars: 100,
      inputSchema: stubSchema,
      async description() { return '' },
      async prompt() { return '' },
      async call() { return { data: {} } },
      renderToolUseMessage() { return null },
      mapToolResultToToolResultBlockParam(_: any, id: string) {
        return { tool_use_id: id, type: 'tool_result' as const, content: '' }
      },
    })
    const toolB = buildTool({
      name: 'ToolB',
      maxResultSizeChars: 100,
      inputSchema: stubSchema,
      async description() { return '' },
      async prompt() { return '' },
      async call() { return { data: {} } },
      renderToolUseMessage() { return null },
      mapToolResultToToolResultBlockParam(_: any, id: string) {
        return { tool_use_id: id, type: 'tool_result' as const, content: '' }
      },
    })
    const tools: Tools = [toolA, toolB]

    test('findToolByName finds by primary name', () => {
      const found = findToolByName(tools, 'ToolA')
      expect(found).toBeTruthy()
      expect(found!.name).toBe('ToolA')
    })

    test('findToolByName finds by alias', () => {
      const found = findToolByName(tools, 'AliasA1')
      expect(found).toBeTruthy()
      expect(found!.name).toBe('ToolA')
    })

    test('findToolByName finds second alias', () => {
      const found = findToolByName(tools, 'AliasA2')
      expect(found).toBeTruthy()
      expect(found!.name).toBe('ToolA')
    })

    test('findToolByName returns undefined for unknown name', () => {
      expect(findToolByName(tools, 'Unknown')).toBeUndefined()
    })

    test('toolMatchesName matches primary name', () => {
      expect(toolMatchesName(toolA, 'ToolA')).toBe(true)
    })

    test('toolMatchesName matches alias', () => {
      expect(toolMatchesName(toolA, 'AliasA2')).toBe(true)
    })

    test('toolMatchesName returns false for non-match', () => {
      expect(toolMatchesName(toolA, 'ToolB')).toBe(false)
    })

    test('tool without aliases does not match random name', () => {
      expect(toolMatchesName(toolB, 'SomethingElse')).toBe(false)
    })
  })

  describe('tool name uniqueness validation', () => {
    test('detects duplicate primary names', () => {
      const tools = [
        { name: 'Duplicate' },
        { name: 'Unique' },
        { name: 'Duplicate' },
      ]
      const names = tools.map(t => t.name)
      const uniqueNames = new Set(names)
      expect(uniqueNames.size).toBeLessThan(names.length)
    })

    test('detects alias collision with primary name', () => {
      const tools = [
        { name: 'Read', aliases: [] },
        { name: 'FileRead', aliases: ['Read'] },
      ]
      const primaryNames = new Set(tools.map(t => t.name))
      const collisions: string[] = []
      for (const tool of tools) {
        for (const alias of tool.aliases ?? []) {
          if (primaryNames.has(alias)) {
            collisions.push(`${tool.name} alias "${alias}" collides`)
          }
        }
      }
      expect(collisions.length).toBeGreaterThan(0)
    })

    test('no collision when names and aliases are disjoint', () => {
      const tools = [
        { name: 'Read', aliases: ['FileRead'] },
        { name: 'Edit', aliases: ['FileEdit'] },
      ]
      const primaryNames = new Set(tools.map(t => t.name))
      const allAliases = new Set<string>()
      let hasCollision = false
      for (const tool of tools) {
        for (const alias of tool.aliases ?? []) {
          if (primaryNames.has(alias) || allAliases.has(alias)) {
            hasCollision = true
          }
          allAliases.add(alias)
        }
      }
      expect(hasCollision).toBe(false)
    })
  })

  describe('getEmptyToolPermissionContext', () => {
    test('returns a valid permission context with defaults', () => {
      const ctx = getEmptyToolPermissionContext()
      expect(ctx.mode).toBe('default')
      expect(ctx.additionalWorkingDirectories).toBeInstanceOf(Map)
      expect(ctx.isBypassPermissionsModeAvailable).toBe(false)
    })

    test('alwaysAllowRules is empty', () => {
      const ctx = getEmptyToolPermissionContext()
      expect(Object.keys(ctx.alwaysAllowRules).length).toBe(0)
    })

    test('alwaysDenyRules is empty', () => {
      const ctx = getEmptyToolPermissionContext()
      expect(Object.keys(ctx.alwaysDenyRules).length).toBe(0)
    })
  })

  describe('filterToolProgressMessages', () => {
    test('filters out hook_progress messages', () => {
      const messages = [
        { data: { type: 'bash' } },
        { data: { type: 'hook_progress' } },
        { data: { type: 'bash' } },
      ]
      const filtered = filterToolProgressMessages(messages as any)
      expect(filtered.length).toBe(2)
    })

    test('keeps all non-hook messages', () => {
      const messages = [
        { data: { type: 'agent_tool' } },
        { data: { type: 'bash' } },
      ]
      const filtered = filterToolProgressMessages(messages as any)
      expect(filtered.length).toBe(2)
    })

    test('handles empty array', () => {
      const filtered = filterToolProgressMessages([])
      expect(filtered.length).toBe(0)
    })
  })
})
