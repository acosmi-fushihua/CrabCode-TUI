/**
 * Ensures that external native package stubs exist for E2E testing.
 * The built CLI (dist/index.js) has external dependencies that are marked
 * as externals during bundling. In dev/test environments, these packages
 * may not be installed, so we create minimal stubs.
 *
 * This file is safe to import multiple times (idempotent).
 */
import { existsSync, lstatSync, mkdirSync, writeFileSync } from 'fs';
import { join, resolve } from 'path';

const PROJECT_ROOT = resolve(import.meta.dir, '../..');
const MODULES_DIR = join(PROJECT_ROOT, 'node_modules');

// `@acosmi-ai/sandbox-runtime` 已退场，dist/index.js 不再 require/import；
// 测试也不得在 node_modules 中重新生成它。
// 历史 stub class 形：constructor/initialize/start/stop/isRunning + SandboxRuntimeConfigSchema +
//   SandboxViolationStore.record/getAll，已不复用
const STUBS: Record<string, Record<string, string>> = {
  'color-diff-napi': {
    'index.js': 'export class ColorDiff {} export class ColorFile {} export function getSyntaxTheme() { return null; } export default {};',
  },
  'modifiers-napi': { 'index.js': 'export function getActiveModifiers() { return 0; } export default {};' },
  'audio-capture-napi': { 'index.js': 'export class AudioCapture { start() {} stop() {} } export default {};' },
};

export function ensureExternalStubs(): void {
  for (const [pkg, files] of Object.entries(STUBS)) {
    const pkgDir = join(MODULES_DIR, ...pkg.split('/'));
    // Never probe a guessed entry filename and then write through an existing
    // package symlink. Real packages commonly use `dist/index.js`; the old
    // check followed Bun's node_modules symlink and overwrote the immutable
    // store package.json with a 0.0.0 stub during test preload.
    if (existsSync(pkgDir)) {
      const stat = lstatSync(pkgDir);
      if (stat.isDirectory() || stat.isSymbolicLink()) continue;
      throw new Error(`external test package path is not a directory: ${pkgDir}`);
    }
    mkdirSync(pkgDir, { recursive: true });
    for (const [filename, content] of Object.entries(files)) {
      writeFileSync(join(pkgDir, filename), content);
    }
    writeFileSync(
      join(pkgDir, 'package.json'),
      JSON.stringify({ name: pkg, version: '0.0.0-stub', main: 'index.js', type: 'module' }),
    );
  }
}
