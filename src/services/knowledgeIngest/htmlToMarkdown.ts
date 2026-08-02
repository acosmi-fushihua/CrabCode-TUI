/**
 * W-MEMORY-KB-UPLIFT P4 (2026-07-17) — HTML → Markdown conversion for
 * knowledge ingestion. Reuses the repo's existing `turndown` dependency
 * (same engine WebFetchTool uses) with active-content stripping: script /
 * style / iframe / noscript subtrees are REMOVED (not converted) so a
 * fetched page can never smuggle executable text or invisible instructions
 * into a knowledge entry via those elements.
 */

type TurndownCtor = typeof import('turndown')
let servicePromise: Promise<InstanceType<TurndownCtor>> | undefined

function getService(): Promise<InstanceType<TurndownCtor>> {
  return (servicePromise ??= import('turndown').then(m => {
    const Turndown = (m as unknown as { default: TurndownCtor }).default
    const service = new Turndown({
      headingStyle: 'atx',
      codeBlockStyle: 'fenced',
    })
    // Active / invisible content never lands in the knowledge base.
    service.remove(['script', 'style', 'noscript'])
    service.remove('iframe' as never)
    return service
  }))
}

/** Convert an HTML document/fragment to markdown (active content stripped). */
export async function htmlToMarkdown(html: string): Promise<string> {
  const service = await getService()
  return service.turndown(html)
}
