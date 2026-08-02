// W-TRANSCRIPT-DUALWRITE RC-1c (E10) — single source of truth for the bundled
// build version.
//
// Root cause of the transcript `version:"unknown"` regression: the three
// historical call sites guarded with `typeof MACRO !== 'undefined' ? MACRO.VERSION
// : 'unknown'`. Bun's bundler `define` (scripts/build-ts.ts) only substitutes
// the *member access* keys (`MACRO.VERSION`, `MACRO.BUILD_ID`, …); it never
// defines the bare `MACRO` identifier. So in an installed/bundled build
// `typeof MACRO` is `'undefined'` at runtime and the ternary short-circuits to
// `'unknown'` — even though the `MACRO.VERSION` inside it was already replaced
// with the real version literal. (Direct `MACRO.VERSION` / `MACRO.BUILD_ID`
// access works fine, which is why daemon handshake + updater were unaffected.)
//
// Fix: access `MACRO.VERSION` DIRECTLY (bundled → the literal) inside a
// try/catch (dev / bare-bun / test with no `define` → `MACRO` is an undefined
// identifier → ReferenceError → 'unknown'). Evaluated ONCE at module init
// (synchronous, top-level IIFE) so it also sidesteps the bun `define`
// async-context bug (oven-sh/bun#26168) that motivated the original
// module-level cache in sessionStorage-paths.ts.
const CACHED_MACRO_VERSION: string = (() => {
  try {
    return MACRO.VERSION
  } catch {
    return 'unknown'
  }
})()

/**
 * The bundled build version (e.g. `"1.3.46"`), or `'unknown'` when running
 * without the bundler `define` (dev / bare bun / tests). Cached at module init.
 */
export function resolveMacroVersion(): string {
  return CACHED_MACRO_VERSION
}
