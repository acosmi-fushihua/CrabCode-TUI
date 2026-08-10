import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  test,
} from 'bun:test'
import {
  mkdtempSync,
  rmSync,
} from 'node:fs'
import { lstat, symlink } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { getCrabCodeTempDir } from '../../src/utils/permissions/filesystem.js'
import {
  _createSandboxCommandTempDirForTest,
  SANDBOX_EXEC_CONFIG_VERSION,
  buildSandboxExecConfig,
  cleanupSandboxCommandTempDir,
  deriveSandboxExecConfig,
  generateSandboxCommandId,
} from '../../src/utils/sandbox/sandboxExecConfig.js'
import {
  resetSandboxNetworkProxyForTests,
  stopSandboxFilteringProxy,
} from '../../src/utils/sandbox/sandboxNetworkProxy.js'
import { resetSettingsCache } from '../../src/utils/settings/settingsCache.js'

let configDir: string
let previousConfigDir: string | undefined
const commandTempDirs = new Set<string>()
beforeAll(() => {
  configDir = mkdtempSync(join(tmpdir(), 'crabcode-tui-sandbox-config-'))
  previousConfigDir = process.env.CRABCODE_CONFIG_DIR
  process.env.CRABCODE_CONFIG_DIR = configDir
  resetSettingsCache()
})

afterAll(async () => {
  await stopSandboxFilteringProxy()
  resetSandboxNetworkProxyForTests()
  if (previousConfigDir === undefined) delete process.env.CRABCODE_CONFIG_DIR
  else process.env.CRABCODE_CONFIG_DIR = previousConfigDir
  resetSettingsCache()
  rmSync(configDir, { recursive: true, force: true })
})

afterEach(async () => {
  for (const path of commandTempDirs) {
    await cleanupSandboxCommandTempDir(path)
  }
  commandTempDirs.clear()
})

function context() {
  const cwd = process.cwd()
  return {
    cwd,
    cwdFile: join(cwd, '.crabcode-cwd-probe'),
  }
}

describe('sandbox exec configuration', () => {
  test('derivation preserves the complete versioned security shape', () => {
    const config = deriveSandboxExecConfig(context())

    expect(config.configVersion).toBe(SANDBOX_EXEC_CONFIG_VERSION)
    expect(config.securityLevel).toBe('allowlist')
    expect(config.cwd).toBe(process.cwd())
    expect(config.filesystem.allowWrite).toContain(context().cwdFile)
    expect(Object.keys(config).sort()).toEqual(
      [
        'configVersion',
        'cwd',
        'fidelity',
        'filesystem',
        'network',
        'securityLevel',
        'tmpDir',
        'weaker',
      ].sort(),
    )
    expect(Object.keys(config.filesystem).sort()).toEqual(
      ['allowRead', 'allowWrite', 'denyRead', 'denyWrite'].sort(),
    )
    expect(Object.keys(config.network).sort()).toEqual(
      [
        'policy',
        'allowedDomains',
        'deniedDomains',
        'allowUnixSockets',
        'allowAllUnixSockets',
        'allowLocalBinding',
        'httpProxyPort',
        'socksProxyPort',
      ].sort(),
    )
    expect(config.weaker).toEqual({
      nestedSandbox: false,
      networkIsolation: false,
    })
  })

  test('builder returns a one-shot stdin document matching the returned report', async () => {
    const privateTemp = join(
      getCrabCodeTempDir(),
      'sandbox-commands',
      generateSandboxCommandId(),
    )
    commandTempDirs.add(privateTemp)
    const built = await buildSandboxExecConfig({
      ...context(),
      tmpDir: privateTemp,
      tmpDirAliases: [privateTemp],
    })
    const parsed = JSON.parse(built.configJson)
    expect(parsed.configVersion).toBe(SANDBOX_EXEC_CONFIG_VERSION)
    expect(parsed.fidelity).toEqual(built.fidelity)
    expect(parsed.network.httpProxyPort).toBe(built.networkProxyPort)
    expect(built.configJson.length).toBeLessThan(8 * 1024 * 1024)
  })

  test('each command receives only its private temp subtree', () => {
    const sharedTempRoot = getCrabCodeTempDir()
    const privateTemp = join(sharedTempRoot, 'sandbox-commands', 'command-a')
    const lexicalAlias = privateTemp.replace(/^\/private\/tmp\//, '/tmp/')
    const config = deriveSandboxExecConfig({
      ...context(),
      tmpDir: privateTemp,
      tmpDirAliases: [lexicalAlias],
    })

    expect(config.tmpDir).toBe(privateTemp)
    expect(config.filesystem.allowWrite).toContain(privateTemp)
    expect(config.filesystem.allowWrite).toContain(lexicalAlias)
    expect(config.filesystem.allowWrite).not.toContain(sharedTempRoot)
  })

  test('command ids carry 128 bits and do not reuse a small collision space', () => {
    const ids = Array.from({ length: 256 }, generateSandboxCommandId)
    expect(ids.every(id => /^[0-9a-f]{32}$/.test(id))).toBe(true)
    expect(new Set(ids).size).toBe(ids.length)
  })

  test('private temp leaf creation is exclusive and cleanup is idempotent', async () => {
    const root = mkdtempSync(join(tmpdir(), 'crabcode-private-tmp-root-'))
    const privateTemp = join(
      root,
      'sandbox-commands',
      generateSandboxCommandId(),
    )
    try {
      await _createSandboxCommandTempDirForTest(privateTemp, [], root)
      expect((await lstat(privateTemp)).isDirectory()).toBe(true)

      await expect(
        _createSandboxCommandTempDirForTest(privateTemp, [], root),
      ).rejects.toMatchObject({ code: 'EEXIST' })

      await cleanupSandboxCommandTempDir(privateTemp)
      await cleanupSandboxCommandTempDir(privateTemp)
      await expect(lstat(privateTemp)).rejects.toMatchObject({ code: 'ENOENT' })
    } finally {
      rmSync(root, { recursive: true, force: true })
    }
  })

  test('pre-occupied private leaf symlink is rejected without following it', async () => {
    if (process.platform === 'win32') return

    const root = mkdtempSync(join(tmpdir(), 'crabcode-private-tmp-symlink-'))
    const attackerTarget = mkdtempSync(
      join(tmpdir(), 'crabcode-private-tmp-attacker-'),
    )
    const privateTemp = join(
      root,
      'sandbox-commands',
      generateSandboxCommandId(),
    )
    try {
      rmSync(privateTemp, { recursive: true, force: true })
      rmSync(join(root, 'sandbox-commands'), { recursive: true, force: true })
      // The parent is host-created here; only the leaf is attacker-controlled.
      const seedTemp = join(
        root,
        'sandbox-commands',
        generateSandboxCommandId(),
      )
      await _createSandboxCommandTempDirForTest(seedTemp, [], root)
      await cleanupSandboxCommandTempDir(seedTemp)
      symlink(attackerTarget, privateTemp, 'dir')

      await expect(
        _createSandboxCommandTempDirForTest(privateTemp, [], root),
      ).rejects.toMatchObject({ code: 'EEXIST' })
      expect((await lstat(privateTemp)).isSymbolicLink()).toBe(true)
    } finally {
      rmSync(root, { recursive: true, force: true })
      rmSync(attackerTarget, { recursive: true, force: true })
    }
  })

  test('a symlinked sandbox-commands parent is rejected before leaf creation', async () => {
    if (process.platform === 'win32') return

    const root = mkdtempSync(join(tmpdir(), 'crabcode-private-parent-root-'))
    const attackerParent = mkdtempSync(
      join(tmpdir(), 'crabcode-private-parent-attacker-'),
    )
    const id = generateSandboxCommandId()
    const privateTemp = join(root, 'sandbox-commands', id)
    try {
      symlink(attackerParent, join(root, 'sandbox-commands'), 'dir')
      await expect(
        _createSandboxCommandTempDirForTest(privateTemp, [], root),
      ).rejects.toThrow('parent is not a host-owned real directory')
      await expect(lstat(join(attackerParent, id))).rejects.toMatchObject({
        code: 'ENOENT',
      })
    } finally {
      rmSync(root, { recursive: true, force: true })
      rmSync(attackerParent, { recursive: true, force: true })
    }
  })
})
