/**
 * Build-time macros injected by Bun bundler define feature.
 * These are replaced with literal values at bundle time.
 * The `typeof MACRO !== 'undefined'` guard is used in some places.
 */
declare const MACRO: {
  readonly VERSION: string
  /**
   * F1 daemon/产物代际握手 (2026-06-11)：`${version}+${git short sha(12)}`，
   * 由 scripts/build-ts.ts / build-worker.ts define 注入（env CRABCODE_BUILD_ID
   * 逐字优先；git 不可用 → `${version}+unknown` = 非权威，比对永远视为一致）。
   * dev 裸 bun 运行（无 define）时 undefined → 握手判定按非权威跳过。
   */
  readonly BUILD_ID: string | undefined
  readonly PACKAGE_URL: string
  readonly BUILD_TIME: string | undefined
  readonly ISSUES_EXPLAINER: string
  /** Build channel — "ant" for internal Acosmi builds, "external" for public releases. */
  readonly CHANNEL: 'ant' | 'external'
  /** Build environment — "production", "test", or "development". */
  readonly BUILD_ENV: 'production' | 'test' | 'development'
  /** Feedback/support channel URL or identifier for internal Ant builds. */
  readonly FEEDBACK_CHANNEL: string
  /** npm package name for native binaries (e.g. "@acosmi/crabcode-native"). Undefined for non-native builds. */
  readonly NATIVE_PACKAGE_URL: string | undefined
  /** Bundled changelog string for the current version (ant builds only). */
  readonly VERSION_CHANGELOG: string | undefined
}
