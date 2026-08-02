/**
 * Type declarations for the `bun:bundle` module.
 *
 * In the Bun build pipeline, `bun:bundle` is a built-in module whose
 * `feature()` function is replaced at compile time with boolean literals.
 *
 * In non-Bun environments, the TypeScript paths mapping in tsconfig.json
 * redirects `'bun:bundle'` to `src/utils/featurePolyfill.ts`, which provides
 * a runtime-equivalent implementation.
 *
 * This declaration file ensures that:
 *   1. TypeScript understands the module shape regardless of environment
 *   2. IDE autocompletion works for `import { feature } from 'bun:bundle'`
 *   3. The 196+ source files importing from 'bun:bundle' type-check cleanly
 *
 * The signature matches `featurePolyfill.ts`'s exported `feature()` function.
 */
declare module 'bun:bundle' {
  /**
   * Returns whether a feature flag is enabled.
   *
   * In Bun builds: replaced at compile time with a boolean literal.
   * In non-Bun environments: resolved at runtime via env vars, config file,
   * or default values (see `featurePolyfill.ts`).
   *
   * @param flag - The feature flag name (for example, 'KAIROS')
   * @returns `true` if the flag is enabled, `false` otherwise
   */
  export function feature(flag: string): boolean;
}
