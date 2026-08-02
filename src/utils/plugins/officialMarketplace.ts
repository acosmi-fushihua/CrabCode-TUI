/**
 * Constants for the official Acosmi plugins marketplace.
 *
 * The official marketplace is hosted on GitHub and provides first-party
 * plugins developed by Acosmi. This file defines the constants needed
 * to install and identify this marketplace.
 *
 * 实际公开仓库地址为 `https://github.com/acosmi/CrabCode-Plugin`
 * （插件 manifest + 各插件源码，**非** CrabCode 产品源码仓）。
 * `OFFICIAL_MARKETPLACE_NAME` 保留 `crabcode-plugins-official` 不动 —
 * 这是 known_marketplaces.json 的兼容 key，改名会让老用户已安装的
 * marketplace 识别失败、被误标为「未安装」。
 *
 * This literal points only to the public plugin marketplace repository, not a
 * CrabCode product source repository.
 */

import type { MarketplaceSource } from './schemas.js'

/**
 * Source configuration for the official Acosmi plugins marketplace.
 * Used when auto-installing the marketplace on startup.
 *
 * `repo` 字面更新为 `acosmi/CrabCode-Plugin`（实际公开仓库）。
 */
export const OFFICIAL_MARKETPLACE_SOURCE = {
  source: 'github',
  repo: 'acosmi/CrabCode-Plugin',
} as const satisfies MarketplaceSource

/**
 * Legacy repo literal used by older marketplace registrations. Kept as a
 * named constant so the migration helper `migrateOfficialMarketplaceRepo`
 * can rewrite stale `known_marketplaces.json` source.repo fields. Do not
 * reference this from new code — use `OFFICIAL_MARKETPLACE_SOURCE`.
 */
export const LEGACY_OFFICIAL_MARKETPLACE_REPO = 'acosmi/crabcode-plugins-official'

/**
 * Display name for the official marketplace.
 * This is the name under which the marketplace will be registered
 * in the known_marketplaces.json file.
 *
 * 保持该兼容字面不变，避免破坏既有用户安装识别。
 */
export const OFFICIAL_MARKETPLACE_NAME = 'crabcode-plugins-official'

/**
 * Rewrites a stale `known_marketplaces.json` entry where the official
 * marketplace was registered under the legacy `acosmi/crabcode-plugins-
 * official` repo literal to the new `acosmi/CrabCode-Plugin` literal.
 * Idempotent; returns `true` if a rewrite happened.
 *
 * The implementation is lazy-delegated to marketplaceManager so the migration
 * shares its proper-lockfile transaction, schema validation, atomic writer,
 * and standard/cowork/CRABCODE_PLUGIN_CACHE_DIR path resolution. Keeping this
 * compatibility wrapper avoids a static circular import: marketplaceManager
 * imports the official constants above.
 *
 * Safe to call on every startup — no-op when the entry is absent or
 * already on the new literal.
 */
export async function migrateOfficialMarketplaceRepo(): Promise<boolean> {
  try {
    const manager = await import('./marketplaceManager.js')
    return await manager.migrateOfficialMarketplaceRepoInRegistry()
  } catch {
    // Fresh homes and corrupt/unreadable registries stay fail-closed/no-op.
    return false
  }
}
