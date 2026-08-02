import { afterAll, describe, expect, test } from 'bun:test'

const previousDeepResearch = process.env.CRABCODE_FEATURE_DEEP_RESEARCH
const previousWorkflowScripts = process.env.CRABCODE_FEATURE_WORKFLOW_SCRIPTS
process.env.CRABCODE_FEATURE_DEEP_RESEARCH = '1'
process.env.CRABCODE_FEATURE_WORKFLOW_SCRIPTS = '0'

const featureFlags = await import('../../src/utils/featurePolyfill.js')
featureFlags._resetFeatureCacheForTests()
const catalog = await import('../../src/cli/headlessCommands.js')

afterAll(() => {
  if (previousDeepResearch === undefined) {
    delete process.env.CRABCODE_FEATURE_DEEP_RESEARCH
  } else {
    process.env.CRABCODE_FEATURE_DEEP_RESEARCH = previousDeepResearch
  }
  if (previousWorkflowScripts === undefined) {
    delete process.env.CRABCODE_FEATURE_WORKFLOW_SCRIPTS
  } else {
    process.env.CRABCODE_FEATURE_WORKFLOW_SCRIPTS = previousWorkflowScripts
  }
  featureFlags._resetFeatureCacheForTests()
})

describe('direct TUI current-backend command fusion', () => {
  test('projects proxy to both process surfaces and vision only to interactive TUI', async () => {
    catalog.clearHeadlessCommandMemoizationCaches()
    const [headless, direct] = await Promise.all([
      catalog.getHeadlessCommands(process.cwd()),
      catalog.getDirectTuiCommands(process.cwd()),
    ])
    const headlessNames = new Set(headless.map(command => command.name))
    const directNames = new Set(direct.map(command => command.name))

    expect(headlessNames.has('proxy')).toBe(true)
    expect(directNames.has('proxy')).toBe(true)
    expect(headlessNames.has('vision')).toBe(false)
    expect(directNames.has('vision')).toBe(true)
  })

  test('projects bundled deep-research without opening plugin workflow management', async () => {
    catalog.clearHeadlessCommandMemoizationCaches()
    const [headless, direct] = await Promise.all([
      catalog.getHeadlessCommands(process.cwd()),
      catalog.getDirectTuiCommands(process.cwd()),
    ])
    for (const commands of [headless, direct]) {
      const names = new Set(commands.map(command => command.name))
      expect(names.has('deep-research')).toBe(true)
      expect(names.has('workflows')).toBe(false)
    }
  })
})
