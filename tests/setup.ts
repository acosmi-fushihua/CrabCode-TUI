/**
 * Global test setup for CrabCode test suite.
 *
 * This file is loaded before all tests via `bunfig.toml` preload.
 * It configures the test environment to be isolated from production:
 * - Disables telemetry and analytics
 * - Disables auto-update checks
 * - Sets test-mode flags
 * - Provides temp directory management
 */

import { mkdirSync, rmSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

import { ensureExternalStubs } from './helpers/ensureExternalStubs.js';

const TEST_ROOT = join(tmpdir(), `crabcode-test-${process.pid}`);

// ---------------------------------------------------------------------------
// Native-package stubs — MUST run in the global preload, before any test
// module loads. Several `src/` modules `import` externalized native packages
// at top level (e.g. `src/components/StructuredDiff/colorDiff.ts` →
// `color-diff-napi`; also `sharp` / `modifiers-napi` / `audio-capture-napi` /
// Native-only modules are not declared deps, so a fresh CI `node_modules`
// (frozen-lockfile) lacks them → the first unit test that
// transitively imports such a module throws "Cannot find package …" at load
// time, cascading failures across the run. Locally the stubs persist in
// node_modules from a prior e2e run, which masked the gap. Calling the
// idempotent stub installer here (preload) guarantees they exist before any
// import resolves. Real locked JS packages such as Sharp and MCPB are never
// stubbed or modified by this test-only path.
ensureExternalStubs();

// ---------------------------------------------------------------------------
// Environment isolation — these must be set before any src/ imports so that
// module-level const captures (e.g., BashTool, AgentTool) see the test values.
// ---------------------------------------------------------------------------

process.env.CRABCODE_TEST = '1';
process.env.CRABCODE_DISABLE_TELEMETRY = '1';
process.env.CRABCODE_DISABLE_AUTO_UPDATE = '1';
process.env.DISABLE_BACKGROUND_TASKS = '1';
process.env.CRABCODE_DISABLE_AUTO_MEMORY = '1';
// Every test process receives its own state authority. Tests that need a
// different root must replace this with another temporary directory; falling
// back to ~/.crabcode is never permitted from the shared harness.
process.env.CRABCODE_CONFIG_DIR = join(TEST_ROOT, 'state');

// Prevent tests from accidentally hitting real API endpoints
if (!process.env.ACOSMI_API_KEY) {
  process.env.ACOSMI_API_KEY = 'test-key-not-real';
}

// `MACRO` is a build-time constant the production bundler defines via
// esbuild `define`. Several module-init code paths (e.g.
// `tools/AgentTool/AgentTool.tsx:99` `MACRO.CHANNEL === 'ant' ?`) read it
// at import time, which crashes any test that transitively loads them
// (the message-factory chain pulls AgentTool in via builtInAgents). Set
// a benign default here so future tests touching that chain don't trip
// `ReferenceError: MACRO is not defined` at module init.
;(globalThis as unknown as { MACRO: Record<string, string> }).MACRO ??= {
  VERSION: 'test',
  PACKAGE_URL: 'crabcode',
  BUILD_TIME: '',
  ISSUES_EXPLAINER: '',
  CHANNEL: 'external',
  BUILD_ENV: 'test',
  FEEDBACK_CHANNEL: '',
};

// ---------------------------------------------------------------------------
// Global temp directory for test artifacts
// ---------------------------------------------------------------------------

/**
 * Returns the root temp directory for the current test run.
 * Created once per process, cleaned up on exit.
 */
export function getTestRoot(): string {
  mkdirSync(TEST_ROOT, { recursive: true });
  return TEST_ROOT;
}

/**
 * Creates a unique temp directory for a single test case.
 * Caller is responsible for cleanup (or rely on process-level cleanup).
 */
export function createTestDir(prefix = 'case'): string {
  const dir = join(TEST_ROOT, `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`);
  mkdirSync(dir, { recursive: true });
  return dir;
}

// ---------------------------------------------------------------------------
// Process-level cleanup
// ---------------------------------------------------------------------------

function cleanupTestRoot(): void {
  try {
    rmSync(TEST_ROOT, { recursive: true, force: true });
  } catch {
    // Best-effort cleanup; temp dir will be reclaimed by OS eventually.
  }
}

process.on('exit', cleanupTestRoot);
process.on('SIGINT', () => {
  cleanupTestRoot();
  process.exit(130);
});
process.on('SIGTERM', () => {
  cleanupTestRoot();
  process.exit(143);
});

// ---------------------------------------------------------------------------
// Console noise suppression — silence noisy startup warnings in tests
// ---------------------------------------------------------------------------

const originalWarn = console.warn;
console.warn = (...args: unknown[]) => {
  const msg = String(args[0]);
  // Suppress known noisy warnings that don't affect test correctness
  if (
    msg.includes('ExperimentalWarning') ||
    msg.includes('DeprecationWarning') ||
    msg.includes('punycode')
  ) {
    return;
  }
  originalWarn.apply(console, args);
};

// ---------------------------------------------------------------------------
// V116.1 P1-2 (2026-07-24): model-capabilities 磁盘缓存 schema v2 播种助手。
// 12 个测试文件曾各自手写 v1 信封 {models, timestamp};v2 后统一走这里,
// 保证 principalHash/filterStatus/fetchedAt 与生产读取端 (loadCache +
// getCatalogState) 的校验一致。降级/过期/跨主体场景用 opts 显式覆写。
// ---------------------------------------------------------------------------
export function modelCapabilitiesCacheV2(
  models: unknown[],
  opts: {
    filterStatus?: string
    fetchedAt?: number
    principalHash?: string
  } = {},
): string {
  // 延迟 require:setup.ts 在所有测试前加载,避免为无关测试拉起 config 链。
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const { getCurrentCatalogPrincipalHash } =
    require('../src/utils/model/modelCapabilities.js') as typeof import('../src/utils/model/modelCapabilities.js')
  return JSON.stringify({
    schemaVersion: 2,
    principalHash: opts.principalHash ?? getCurrentCatalogPrincipalHash(),
    filterStatus: opts.filterStatus ?? 'ok',
    fetchedAt: opts.fetchedAt ?? Date.now(),
    models,
  })
}
