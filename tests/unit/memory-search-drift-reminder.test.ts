import { describe, expect, test } from 'bun:test'
import { renderMemorySearchResultsForModel } from '../../src/tools/MemorySearchTool/MemorySearchTool.js'

describe('MemorySearch current-state caveat', () => {
  test('keeps historical environment feedback from masquerading as a live capability check', () => {
    const rendered = renderMemorySearchResultsForModel([
      {
        path: '/memory/feedback.md',
        name: 'Old network feedback',
        score: 0.1,
        snippet: 'WebFetch was unavailable',
        scope: 'project',
        type: 'feedback',
      },
    ])

    expect(rendered).toContain('<system-reminder>')
    expect(rendered).toContain('historical snapshots')
    expect(rendered).toContain('Verify current tool availability')
    expect(rendered).toContain('Old network feedback')
  })
})
