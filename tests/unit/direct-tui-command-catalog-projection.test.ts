import { describe, expect, test } from 'bun:test'

import { projectCommandCatalogEntries } from '../../src/cli/commandCatalogProjection.js'
import type { Command } from '../../src/types/command.js'
import { parseFrontmatter } from '../../src/utils/frontmatterParser.js'

function command(
  name: string,
  options: {
    aliases?: string[]
    argumentHint?: string
    description?: string
    userFacingName?: () => string
    userInvocable?: boolean
  } = {},
): Command {
  return {
    type: 'local',
    name,
    description: options.description ?? `${name} description`,
    aliases: options.aliases,
    argumentHint: options.argumentHint,
    userFacingName: options.userFacingName,
    userInvocable: options.userInvocable,
  } as unknown as Command
}

describe('slash-command control catalog projection', () => {
  test('projects canonical names and aliases in backend first-wins order', () => {
    const entries = projectCommandCatalogEntries(
      [
        command('alpha', {
          aliases: ['shared', 'alpha-alias'],
          argumentHint: '<alpha>',
        }),
        command('shared', {
          aliases: ['beta-alias', 'alpha'],
          argumentHint: '<beta>',
        }),
        command('gamma', {
          aliases: ['beta-alias', 'gamma-alias', 'gamma-alias'],
        }),
      ],
      item => `formatted: ${item.description}`,
    )

    expect(entries).toEqual([
      {
        name: 'alpha',
        description: 'formatted: alpha description',
        argumentHint: '<alpha>',
      },
      {
        name: 'shared',
        description: 'formatted: alpha description',
        argumentHint: '<alpha>',
      },
      {
        name: 'alpha-alias',
        description: 'formatted: alpha description',
        argumentHint: '<alpha>',
      },
      {
        name: 'beta-alias',
        description: 'formatted: shared description',
        argumentHint: '<beta>',
      },
      {
        name: 'gamma',
        description: 'formatted: gamma description',
        argumentHint: '',
      },
      {
        name: 'gamma-alias',
        description: 'formatted: gamma description',
        argumentHint: '',
      },
    ])
  })

  test('filters model-only commands before assigning visible token ownership', () => {
    const entries = projectCommandCatalogEntries(
      [
        command('model-only', {
          aliases: ['shared'],
          userInvocable: false,
        }),
        command('shared', { aliases: ['visible-alias'] }),
      ],
      item => item.description,
    )

    expect(entries.map(entry => entry.name)).toEqual([
      'shared',
      'visible-alias',
    ])
  })

  test('does not invent user-facing display labels as invocation tokens', () => {
    const entries = projectCommandCatalogEntries(
      [
        command('stable-name', {
          aliases: ['documented-alias'],
          userFacingName: () => 'localized-display-name',
        }),
      ],
      item => item.description,
    )

    expect(entries.map(entry => entry.name)).toEqual([
      'stable-name',
      'documented-alias',
    ])
    expect(entries.some(entry => entry.name === 'localized-display-name')).toBe(
      false,
    )
  })

  test('omits description-only rows whose names cannot roundtrip through the existing slash parser', () => {
    const entries = projectCommandCatalogEntries(
      [
        command('', { description: 'blank canonical description' }),
        command('valid', {
          aliases: ['', '   ', 'alias with argument tail', 'valid-alias'],
        }),
        command('mcp:tool (MCP)'),
      ],
      item => item.description,
    )

    expect(entries.map(entry => entry.name)).toEqual([
      'valid',
      'valid-alias',
      'mcp:tool (MCP)',
    ])
    expect(entries.every(entry => entry.name.trim().length > 0)).toBe(true)
  })

  test('retains commands but drops malformed runtime argument hints', () => {
    const malformed = [
      ['array-hint', ['project-name']],
      ['number-hint', 7],
      ['object-hint', { value: 'project-name' }],
    ] as const
    const entries = projectCommandCatalogEntries(
      malformed.map(([name, argumentHint]) =>
        command(name, {
          argumentHint: argumentHint as unknown as string,
        }),
      ),
      item => item.description,
    )

    expect(entries).toEqual(
      malformed.map(([name]) => ({
        name,
        description: `${name} description`,
        argumentHint: '',
      })),
    )
  })

  test('closes the real YAML array-hint path before initialize projection', () => {
    const parsed = parseFrontmatter(
      [
        '---',
        'description: Create an SDK project',
        'argument-hint: [project-name]',
        '---',
        'Create the project.',
      ].join('\n'),
      'commands/new-sdk-app.md',
    )
    expect(parsed.frontmatter['argument-hint']).toEqual(['project-name'])

    const [entry] = projectCommandCatalogEntries(
      [
        command('new-sdk-app', {
          argumentHint: parsed.frontmatter[
            'argument-hint'
          ] as unknown as string,
        }),
      ],
      item => item.description,
    )
    expect(entry).toEqual({
      name: 'new-sdk-app',
      description: 'new-sdk-app description',
      argumentHint: '',
    })
    expect(
      Object.values(entry!).every(value => typeof value === 'string'),
    ).toBe(true)
  })
})
