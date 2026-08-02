// Renderer-neutral ambient types for runtime assets and optional dependencies.
// Keep these outside the retired TypeScript/Ink tree so backend code can be
// typechecked without reintroducing an Ink renderer or overriding real MCPB
// package exports.

declare module '*.md' {
  const content: string
  export default content
}

declare module 'cacache' {
  // cacache is an optional runtime dependency without bundled declarations.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const cacache: any
  export = cacache
}

declare module 'highlight.js' {
  export function getLanguage(name: string): unknown
}
