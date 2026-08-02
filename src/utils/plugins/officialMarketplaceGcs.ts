/**
 * inc-5046: fetch the official marketplace from a GCS mirror instead of
 * git-cloning GitHub on every startup.
 *
 * Backend (acosmi#317037) publishes a marketplace-only zip alongside the
 * titanium squashfs, keyed by base repo SHA. This module fetches the `latest`
 * pointer, compares against a local sentinel, and downloads+extracts the zip
 * when there's a new SHA. Callers decide fallback behavior on failure.
 */

import axios from 'axios'
import { mkdir, rename, rm } from 'fs/promises'
import { Agent as HttpsAgent } from 'node:https'
import { join } from 'path'
import { waitForScrollIdle } from '../../bootstrap/state.js'
import type { AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS } from '../../services/analytics/index.js'
import { logEvent } from '../../services/analytics/index.js'
import { logForDebugging } from '../debug.js'
import { parseZipModes, unzipFile } from '../dxt/zip.js'
import { errorMessage, getErrnoCode } from '../errors.js'
import {
  readCanonicalPluginTextFile,
  revalidatePluginPath,
  resolveInternalPluginPath,
  resolvePluginComponentPath,
  writeCanonicalPluginFile,
} from './pluginPathSecurity.js'

type SafeString = AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS

/**
 * 官方插件市场镜像基址(content-addressed zip:`{base}/latest` + `{base}/{sha}.zip`)。
 *
 * 为什么可配 + 跟随主镜像:旧默认 `downloads.acosmi.com/crabcode-releases/...`
 * 在中国大陆从未被填充(真机 HTTP 000,git 回退每次启动慢)。现默认改为**跟随原生
 * 安装镜像** `getMirrorBaseUrl()`(updates.acosmi.com/crabcode,国内可达、自有可控),
 * 子路径 `/plugins/crabcode-plugins-official`,由 sync-plugins-to-mirror 工作流填充。
 *
 * 覆盖优先级:
 *   1. `CRABCODE_PLUGIN_MIRROR_BASE_URL` —— 插件镜像专用显式覆盖(最高)
 *   2. 否则 = `{getMirrorBaseUrl()}/plugins/crabcode-plugins-official`
 *      (`CRABCODE_MIRROR_BASE_URL` 一并改主镜像 + 插件镜像同源,内网/自建场景)
 */
function getOfficialMarketplaceMirrorBaseUrl(): string {
  const override = process.env.CRABCODE_PLUGIN_MIRROR_BASE_URL
  const raw =
    override && override.trim().length > 0
      ? override.trim()
      : 'https://updates.acosmi.com/crabcode/plugins/crabcode-plugins-official'
  let parsed: URL
  try {
    parsed = new URL(raw)
  } catch {
    throw new Error('Official marketplace mirror base must be a valid URL')
  }
  if (
    parsed.protocol !== 'https:' ||
    parsed.username !== '' ||
    parsed.password !== '' ||
    parsed.search !== '' ||
    parsed.hash !== ''
  ) {
    throw new Error(
      'Official marketplace mirror base must use HTTPS without userinfo, query, or fragment',
    )
  }
  return parsed.href.replace(/\/+$/, '')
}

export type OfficialMarketplaceGcsTransportOptions = {
  /** remote ingress-only network boundary; global CLI/startup callers leave unset. */
  hardenedIngress?: boolean
}

type OfficialMarketplaceGcsRequestKind = 'latest' | 'zip'

const OFFICIAL_MARKETPLACE_LATEST_MAX_BYTES = 1024
const OFFICIAL_MARKETPLACE_ZIP_MAX_BYTES = 64 * 1024 * 1024
const BASE_REPOSITORY_SHA_PATTERN = /^[0-9a-f]{40}$/

function resolveOfficialMarketplaceMirrorBaseUrl(
  mirrorBase: string,
  hardenedIngress: boolean,
): string {
  if (!hardenedIngress) return mirrorBase

  let parsed: URL
  try {
    parsed = new URL(mirrorBase)
  } catch {
    throw new Error(
      'Official marketplace mirror base must be a canonical HTTPS URL',
    )
  }
  if (
    parsed.protocol !== 'https:' ||
    parsed.username !== '' ||
    parsed.password !== '' ||
    parsed.search !== '' ||
    parsed.hash !== ''
  ) {
    throw new Error(
      'Official marketplace mirror base must be canonical HTTPS without userinfo, query, or fragment',
    )
  }

  // Reject alternate spellings (dot segments, default ports, host casing,
  // encoded whitespace, and similar forms) instead of silently normalizing a
  // security-boundary destination. getOfficialMarketplaceMirrorBaseUrl()
  // already strips an optional trailing slash for backwards compatibility.
  const canonicalBase = parsed.toString().replace(/\/+$/, '')
  if (canonicalBase !== mirrorBase) {
    throw new Error(
      'Official marketplace mirror base must be a canonical HTTPS URL',
    )
  }
  return canonicalBase
}

function parseOfficialMarketplaceBaseRepositorySha(
  data: unknown,
  hardenedIngress: boolean,
): string {
  const raw = String(data)
  if (hardenedIngress) {
    // sync-plugins-to-mirror writes `git rev-parse HEAD` with printf (no
    // newline). Exact lowercase SHA-1 syntax prevents path/cache-key injection.
    if (!BASE_REPOSITORY_SHA_PATTERN.test(raw)) {
      throw new Error(
        'latest pointer must be exactly a 40-character lowercase hexadecimal base repository SHA',
      )
    }
    return raw
  }

  // Preserve the legacy global CLI/startup mirror contract, including its
  // whitespace tolerance. Only remote ingress ingress adopts the stricter format.
  const sha = raw.trim()
  if (!sha) throw new Error('latest pointer returned empty body')
  return sha
}

function officialMarketplaceGcsRequestConfig(
  kind: OfficialMarketplaceGcsRequestKind,
  hardenedIngress: boolean,
): NonNullable<Parameters<typeof axios.get>[1]> {
  const isLatest = kind === 'latest'
  const maxBytes = isLatest
    ? OFFICIAL_MARKETPLACE_LATEST_MAX_BYTES
    : OFFICIAL_MARKETPLACE_ZIP_MAX_BYTES
  return {
    responseType: isLatest ? 'text' : 'arraybuffer',
    timeout: isLatest ? 10_000 : 60_000,
    ...(hardenedIngress && {
      // Request-local settings override mutable axios defaults and disable
      // HTTPS_PROXY discovery at the remote ingress trust boundary.
      proxy: false,
      maxRedirects: 0,
      maxContentLength: maxBytes,
      maxBodyLength: maxBytes,
      httpsAgent: new HttpsAgent({
        minVersion: 'TLSv1.2',
        rejectUnauthorized: true,
      }),
    }),
  }
}

/** Test seams for transport policy without performing mirror I/O. */
export function __resolveOfficialMarketplaceMirrorBaseUrlForTest(
  mirrorBase: string,
  hardenedIngress: boolean,
): string {
  return resolveOfficialMarketplaceMirrorBaseUrl(
    mirrorBase,
    hardenedIngress,
  )
}

export function __parseOfficialMarketplaceBaseRepositoryShaForTest(
  data: unknown,
  hardenedIngress: boolean,
): string {
  return parseOfficialMarketplaceBaseRepositorySha(data, hardenedIngress)
}

export function __getOfficialMarketplaceGcsRequestConfigForTest(
  kind: OfficialMarketplaceGcsRequestKind,
  hardenedIngress: boolean,
): NonNullable<Parameters<typeof axios.get>[1]> {
  return officialMarketplaceGcsRequestConfig(kind, hardenedIngress)
}

// Zip arc paths are seed-dir-relative (marketplaces/crabcode-plugins-official/…)
// so the titanium seed machinery can use the same zip. Strip this prefix when
// extracting for a laptop install.
const ARC_PREFIX = 'marketplaces/crabcode-plugins-official/'

/** Extract already-decoded official marketplace entries under one staging root. */
export async function extractOfficialMarketplaceArchive(
  files: Record<string, Uint8Array>,
  modes: Record<string, number>,
  stagingRoot: string,
): Promise<void> {
  for (const [arcPath, data] of Object.entries(files)) {
    if (!arcPath.startsWith(ARC_PREFIX)) continue
    const relativePath = arcPath.slice(ARC_PREFIX.length)
    if (!relativePath || relativePath.endsWith('/')) continue

    const destination = await resolvePluginComponentPath(
      stagingRoot,
      relativePath,
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'official marketplace archive entry',
      },
    )
    const parent = await resolveInternalPluginPath(
      stagingRoot,
      join(destination, '..'),
      {
        mustExist: false,
        rejectSymlinks: true,
        component: 'official marketplace archive directory',
      },
    )
    await mkdir(parent, { recursive: true })
    await revalidatePluginPath(
      stagingRoot,
      parent,
      'official marketplace archive directory',
    )
    const mode = modes[arcPath]
    const executableMode = mode && mode & 0o111 ? mode & 0o777 : undefined
    await writeCanonicalPluginFile(
      stagingRoot,
      destination,
      data,
      'official marketplace archive entry',
      {
        exclusive: true,
        mode: executableMode,
        bestEffortMode: executableMode !== undefined,
      },
    )
  }
}

/**
 * Fetch the official marketplace from GCS and extract to installLocation.
 * Idempotent — checks a `.gcs-sha` sentinel before downloading the ~3.5MB zip.
 *
 * @param installLocation where to extract (must be inside marketplacesCacheDir)
 * @param marketplacesCacheDir the plugins marketplace cache root — passed in
 *   by callers (rather than imported from pluginDirectories) to break a
 *   circular-dep edge through marketplaceManager
 * @returns the fetched SHA on success (including no-op), null on any failure
 *   (network, 404, zip parse). Caller decides whether to fall through to git.
 */
export async function fetchOfficialMarketplaceFromGcs(
  installLocation: string,
  marketplacesCacheDir: string,
  options?: OfficialMarketplaceGcsTransportOptions,
): Promise<string | null> {
  // Defense in depth: this function does `rm(installLocation, {recursive})`
  // during the atomic swap. A corrupted known_marketplaces.json (gh-32793 —
  // Windows path read on WSL, literal tilde, manual edit) could point at the
  // user's project. Refuse any path outside the marketplaces cache dir.
  // Same guard as refreshMarketplace() at marketplaceManager.ts:~2392 but
  // inside the function so ALL callers are covered.
  let cacheDir: string
  let resolvedLoc: string
  try {
    await mkdir(marketplacesCacheDir, { recursive: true })
    cacheDir = await resolveInternalPluginPath(
      marketplacesCacheDir,
      marketplacesCacheDir,
      { component: 'official marketplace cache root' },
    )
    resolvedLoc = await resolveInternalPluginPath(cacheDir, installLocation, {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'official marketplace install location',
    })
  } catch (error) {
    logForDebugging(
      `fetchOfficialMarketplaceFromGcs: refusing unsafe cache path ${installLocation}: ${errorMessage(error)}`,
      { level: 'error' },
    )
    return null
  }

  // Network + zip extraction competes for the event loop with scroll frames.
  // This is a fire-and-forget startup call — delaying by a few hundred ms
  // until scroll settles is invisible to the user.
  await waitForScrollIdle()

  const start = performance.now()
  let outcome: 'noop' | 'updated' | 'failed' = 'failed'
  let sha: string | undefined
  let bytes: number | undefined
  let errKind: string | undefined

  try {
    // 1. Latest pointer — ~40 bytes, backend sets Cache-Control: no-cache,
    //    max-age=300. Cheap enough to hit every startup.
    const hardenedIngress = !!options?.hardenedIngress
    const mirrorBase = resolveOfficialMarketplaceMirrorBaseUrl(
      getOfficialMarketplaceMirrorBaseUrl(),
      hardenedIngress,
    )
    const latest = await axios.get(
      `${mirrorBase}/latest`,
      officialMarketplaceGcsRequestConfig('latest', hardenedIngress),
    )
    sha = parseOfficialMarketplaceBaseRepositorySha(
      latest.data,
      hardenedIngress,
    )

    // 2. Sentinel check — `.gcs-sha` at the install root holds the last
    //    extracted SHA. Matching means we already have this content.
    const sentinelPath = await resolveInternalPluginPath(
      cacheDir,
      join(resolvedLoc, '.gcs-sha'),
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'official marketplace GCS sentinel',
      },
    )
    const currentSha = await readCanonicalPluginTextFile(
      cacheDir,
      sentinelPath,
      'official marketplace GCS sentinel',
    ).then(
      s => s.trim(),
      () => null, // ENOENT — first fetch, proceed to download
    )
    if (currentSha === sha) {
      outcome = 'noop'
      return sha
    }

    // 3. Download zip and extract to a staging dir, then atomic-swap into
    //    place. Crash mid-extract leaves a .staging dir (next run rm's it)
    //    rather than a half-written installLocation.
    // The publisher currently exposes neither a signature nor a ZIP digest
    // sidecar. Authenticated TLS is therefore the current authenticity root;
    // the base-repository SHA selects a version/path but does not authenticate
    // the ZIP bytes. Digest/signature enforcement remains an upstream
    // publishing-contract gate and must not be implied by this SHA check.
    const zipResp = await axios.get(
      `${mirrorBase}/${sha}.zip`,
      officialMarketplaceGcsRequestConfig('zip', hardenedIngress),
    )
    const zipBuf = Buffer.from(zipResp.data)
    bytes = zipBuf.length
    const files = await unzipFile(zipBuf)
    // fflate doesn't surface external_attr, so parse the central directory
    // ourselves to recover exec bits. Without this, hooks/scripts extract as
    // 0644 and `sh -c "/path/script.sh"` (hooks.ts:~1002) fails with EACCES
    // on Unix. Git-clone preserves +x natively; this keeps GCS at parity.
    const modes = parseZipModes(zipBuf)

    let staging = await resolveInternalPluginPath(
      cacheDir,
      `${resolvedLoc}.staging`,
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'official marketplace staging directory',
      },
    )
    await rm(staging, { recursive: true, force: true })
    await mkdir(staging, { recursive: true })
    staging = await resolveInternalPluginPath(cacheDir, staging, {
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'official marketplace staging directory',
    })
    await extractOfficialMarketplaceArchive(files, modes, staging)
    const stagingSentinel = await resolvePluginComponentPath(
      staging,
      '.gcs-sha',
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'official marketplace staging sentinel',
      },
    )
    await writeCanonicalPluginFile(
      staging,
      stagingSentinel,
      sha,
      'official marketplace staging sentinel',
      { exclusive: true },
    )

    // Atomic swap: rm old, rename staging. Brief window where installLocation
    // doesn't exist — acceptable for a background refresh (caller retries next
    // startup if it crashes here).
    resolvedLoc = await resolveInternalPluginPath(cacheDir, resolvedLoc, {
      mustExist: false,
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'official marketplace install location',
    })
    await rm(resolvedLoc, { recursive: true, force: true })
    staging = await resolveInternalPluginPath(cacheDir, staging, {
      rejectSymlinks: true,
      rejectRoot: true,
      component: 'official marketplace staging directory',
    })
    const activeInstallLocation = await resolveInternalPluginPath(
      cacheDir,
      resolvedLoc,
      {
        mustExist: false,
        rejectSymlinks: true,
        rejectRoot: true,
        component: 'official marketplace install location',
      },
    )
    await rename(staging, activeInstallLocation)

    outcome = 'updated'
    return sha
  } catch (e) {
    errKind = classifyGcsError(e)
    logForDebugging(
      `Official marketplace GCS fetch failed: ${errorMessage(e)}`,
      { level: 'warn' },
    )
    return null
  } finally {
    // tengu_plugin_remote_fetch schema shared with the telemetry PR
    // (.daisy/inc-5046/index.md) — adds source:'marketplace_gcs'. All string
    // values below are static enums or a git SHA — not code/filepaths/PII.
    logEvent('tengu_plugin_remote_fetch', {
      source: 'marketplace_gcs' as SafeString,
      host: 'downloads.acosmi.com' as SafeString,
      is_official: true,
      outcome: outcome as SafeString,
      duration_ms: Math.round(performance.now() - start),
      ...(bytes !== undefined && { bytes }),
      ...(sha && { sha: sha as SafeString }),
      ...(errKind && { error_kind: errKind as SafeString }),
    })
  }
}

// Bounded set of errno codes we report by name. Anything else buckets as
// fs_other to keep dashboard cardinality tractable.
const KNOWN_FS_CODES = new Set([
  'ENOSPC',
  'EACCES',
  'EPERM',
  'EXDEV',
  'EBUSY',
  'ENOENT',
  'ENOTDIR',
  'EROFS',
  'EMFILE',
  'ENAMETOOLONG',
])

/**
 * Classify a GCS fetch error into a stable telemetry bucket.
 *
 * Telemetry from v2.1.83+ showed 50% of failures landing in 'other' — and
 * 99.99% of those had both sha+bytes set, meaning download succeeded but
 * extraction/fs failed. This splits that bucket so we can see whether the
 * failures are fixable (wrong staging dir, cross-device rename) or inherent
 * (disk full, permission denied) before flipping the git-fallback kill switch.
 */
export function classifyGcsError(e: unknown): string {
  if (axios.isAxiosError(e)) {
    if (e.code === 'ECONNABORTED') return 'timeout'
    if (e.response) return `http_${e.response.status}`
    return 'network'
  }
  const code = getErrnoCode(e)
  // Node fs errno codes are E<UPPERCASE> (ENOSPC, EACCES). Axios also sets
  // .code (ERR_NETWORK, ERR_BAD_OPTION, EPROTO) — don't bucket those as fs.
  if (code && /^E[A-Z]+$/.test(code) && !code.startsWith('ERR_')) {
    return KNOWN_FS_CODES.has(code) ? `fs_${code}` : 'fs_other'
  }
  // fflate sets numeric .code (0-14) on inflate/unzip errors — catches
  // deflate-level corruption ("unexpected EOF", "invalid block type") that
  // the message regex misses.
  if (typeof (e as { code?: unknown })?.code === 'number') return 'zip_parse'
  const msg = errorMessage(e)
  if (/unzip|invalid zip|central directory/i.test(msg)) return 'zip_parse'
  if (/empty body/.test(msg)) return 'empty_latest'
  return 'other'
}
