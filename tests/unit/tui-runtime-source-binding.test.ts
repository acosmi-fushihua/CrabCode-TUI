import { describe, expect, test } from 'bun:test'
import {
  mkdirSync,
  mkdtempSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import {
  bindTuiRuntimeArtifact,
  bindTuiRuntimeBuild,
  bindTuiRuntimeInputs,
  createTuiRuntimeBuildConfiguration,
  normalizeTuiRuntimeAccountBridgeConfiguration,
  verifyTuiRuntimeArtifactBinding,
  verifyTuiRuntimeBuildBinding,
  verifyTuiRuntimeReleaseBuildBinding,
  verifyTuiRuntimeSourceBinding,
} from '../../scripts/tui-runtime-source-binding.mjs'

const releaseEnvironment = {
  ACCOUNT_BRIDGE_CONTROL_PLANE_ENDPOINT:
    'https://acosmi.com/api/v4/account-bridge/control-plane/v2',
  ACCOUNT_BRIDGE_ELIGIBILITY_PUBLIC_KEY_BASE64URL:
    Buffer.alloc(32, 1).toString('base64url'),
  ACCOUNT_BRIDGE_CONNECTOR_POLICY_PUBLIC_KEY_BASE64URL:
    Buffer.alloc(32, 2).toString('base64url'),
  ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL:
    Buffer.alloc(32, 3).toString('base64url'),
}

function createMetafile(
  root: string,
  artifact: string,
  {
    profile = 'development',
    minify = false,
    nodeEnv = 'development',
    environment = {},
  }: {
    profile?: 'development' | 'release'
    minify?: boolean
    nodeEnv?: 'development' | 'production'
    environment?: Record<string, string>
  } = {},
) {
  const entryPoint = 'src/entrypoints/tuiRuntime.ts'
  const sourceBinding = bindTuiRuntimeInputs(root, [entryPoint])
  const artifactBinding = bindTuiRuntimeArtifact(artifact)
  const identity = {
    entryPoint,
    output: 'dist/tui-runtime/index.js',
    version: '1.2.3',
    buildId: '1.2.3+0123456789ab',
  }
  const buildConfiguration = createTuiRuntimeBuildConfiguration({
    profile,
    minify,
    nodeEnv,
    accountBridgeConfiguration:
      normalizeTuiRuntimeAccountBridgeConfiguration(environment),
  })
  const boundBuild = {
    schemaVersion: 3,
    ...identity,
    imageProcessorNapi: false,
    buildConfiguration,
    sourceBinding,
    artifactBinding,
  }
  return {
    inputs: { [entryPoint]: { bytes: 23 } },
    outputs: { './index.js': { entryPoint } },
    crabcodeTuiBuild: {
      ...boundBuild,
      buildBinding: bindTuiRuntimeBuild(boundBuild),
    },
  }
}

describe('TUI runtime source/artifact binding', () => {
  test('detects both stale source graphs and changed bundle bytes', () => {
    const root = mkdtempSync(join(tmpdir(), 'crabcode-tui-binding-'))
    const source = join(root, 'src/entrypoints/tuiRuntime.ts')
    const artifact = join(root, 'dist/tui-runtime/index.js')
    mkdirSync(join(root, 'src/entrypoints'), { recursive: true })
    mkdirSync(join(root, 'dist/tui-runtime'), { recursive: true })
    writeFileSync(source, 'export const value = 1\n')
    writeFileSync(artifact, 'console.log(1)\n')

    try {
      const metafile = createMetafile(root, artifact)
      expect(() =>
        verifyTuiRuntimeBuildBinding(metafile, {
          version: '1.2.3',
          buildId: '1.2.3+0123456789ab',
        }),
      ).not.toThrow()
      expect(() => verifyTuiRuntimeSourceBinding(root, metafile)).not.toThrow()
      expect(() =>
        verifyTuiRuntimeArtifactBinding(artifact, metafile),
      ).not.toThrow()

      const missingEntryPoint = structuredClone(metafile)
      missingEntryPoint.inputs = {}
      expect(() =>
        verifyTuiRuntimeSourceBinding(root, missingEntryPoint),
      ).toThrow('does not contain its entry point')

      const wrongOutput = structuredClone(metafile)
      wrongOutput.outputs['./index.js']!.entryPoint = 'src/other.ts'
      expect(() => verifyTuiRuntimeSourceBinding(root, wrongOutput)).toThrow(
        'single bound entry-point output',
      )

      writeFileSync(source, 'export const value = 2\n')
      expect(() => verifyTuiRuntimeSourceBinding(root, metafile)).toThrow(
        'source binding is stale',
      )
      writeFileSync(source, 'export const value = 1\n')

      writeFileSync(artifact, 'console.log(2)\n')
      expect(() =>
        verifyTuiRuntimeArtifactBinding(artifact, metafile),
      ).toThrow('artifact binding is stale')

      writeFileSync(artifact, 'console.log(1)\n')
      metafile.crabcodeTuiBuild.buildId = '1.2.3+ba9876543210'
      expect(() => verifyTuiRuntimeBuildBinding(metafile)).toThrow(
        'build binding is stale',
      )
    } finally {
      rmSync(root, { recursive: true, force: true })
    }
  })

  test('rejects input paths outside the repository root', () => {
    const root = mkdtempSync(join(tmpdir(), 'crabcode-tui-binding-root-'))
    try {
      expect(() => bindTuiRuntimeInputs(root, ['../outside.ts'])).toThrow(
        'escaped the repository',
      )
    } finally {
      rmSync(root, { recursive: true, force: true })
    }
  })

  test('rejects aliased inputs and symbolic-link directory escapes', () => {
    const root = mkdtempSync(join(tmpdir(), 'crabcode-tui-binding-root-'))
    const outside = mkdtempSync(join(tmpdir(), 'crabcode-tui-binding-outside-'))
    try {
      mkdirSync(join(root, 'src'), { recursive: true })
      writeFileSync(join(root, 'src/runtime.ts'), 'export {}\n')
      expect(() =>
        bindTuiRuntimeInputs(root, ['src/runtime.ts', 'src/./runtime.ts']),
      ).toThrow('aliased duplicate paths')

      writeFileSync(join(outside, 'escaped.ts'), 'export {}\n')
      symlinkSync(outside, join(root, 'linked-source'))
      expect(() =>
        bindTuiRuntimeInputs(root, ['linked-source/escaped.ts']),
      ).toThrow('symbolic-link directory')
    } finally {
      rmSync(root, { recursive: true, force: true })
      rmSync(outside, { recursive: true, force: true })
    }
  })

  test('rejects artifact symlinks and a self-consistent wrong build id', () => {
    const root = mkdtempSync(join(tmpdir(), 'crabcode-tui-binding-root-'))
    const source = join(root, 'src/entrypoints/tuiRuntime.ts')
    const artifact = join(root, 'dist/tui-runtime/index.js')
    const artifactLink = join(root, 'dist/tui-runtime/index-link.js')
    mkdirSync(join(root, 'src/entrypoints'), { recursive: true })
    mkdirSync(join(root, 'dist/tui-runtime'), { recursive: true })
    writeFileSync(source, 'export const value = 1\n')
    writeFileSync(artifact, 'console.log(1)\n')
    symlinkSync(artifact, artifactLink)

    try {
      expect(() => bindTuiRuntimeArtifact(artifactLink)).toThrow(
        'must not be a symbolic link',
      )

      const metafile = createMetafile(root, artifact)
      const build = metafile.crabcodeTuiBuild
      build.buildId = '1.2.3+ba9876543210'
      build.buildBinding = bindTuiRuntimeBuild(build)
      expect(() =>
        verifyTuiRuntimeBuildBinding(metafile, {
          buildId: '1.2.3+0123456789ab',
        }),
      ).toThrow('build identity mismatch for buildId')
    } finally {
      rmSync(root, { recursive: true, force: true })
    }
  })

  test('rejects development artifacts and accepts only the bound release configuration', () => {
    const root = mkdtempSync(join(tmpdir(), 'crabcode-tui-binding-release-'))
    const source = join(root, 'src/entrypoints/tuiRuntime.ts')
    const artifact = join(root, 'dist/tui-runtime/index.js')
    mkdirSync(join(root, 'src/entrypoints'), { recursive: true })
    mkdirSync(join(root, 'dist/tui-runtime'), { recursive: true })
    writeFileSync(source, 'export const value = 1\n')
    writeFileSync(artifact, 'console.log(1)\n')

    try {
      const developmentMetafile = createMetafile(root, artifact)
      expect(() =>
        verifyTuiRuntimeReleaseBuildBinding(
          developmentMetafile,
          releaseEnvironment,
        ),
      ).toThrow('required release profile')

      const releaseWithoutMinification = createMetafile(root, artifact, {
        profile: 'release',
        minify: false,
        nodeEnv: 'development',
        environment: releaseEnvironment,
      })
      expect(() =>
        verifyTuiRuntimeReleaseBuildBinding(
          releaseWithoutMinification,
          releaseEnvironment,
        ),
      ).toThrow('required release profile')

      const minifiedDevelopmentBuild = createMetafile(root, artifact, {
        profile: 'development',
        minify: true,
        nodeEnv: 'production',
        environment: releaseEnvironment,
      })
      expect(() =>
        verifyTuiRuntimeReleaseBuildBinding(
          minifiedDevelopmentBuild,
          releaseEnvironment,
        ),
      ).toThrow('required release profile')

      const releaseMetafile = createMetafile(root, artifact, {
        profile: 'release',
        minify: true,
        nodeEnv: 'production',
        environment: releaseEnvironment,
      })
      expect(() =>
        verifyTuiRuntimeReleaseBuildBinding(
          releaseMetafile,
          releaseEnvironment,
          {
            version: '1.2.3',
            buildId: '1.2.3+0123456789ab',
          },
          {
            accountBridgeArtifactPublicKeyBase64url:
              releaseEnvironment.ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL,
          },
        ),
      ).not.toThrow()

      expect(() =>
        verifyTuiRuntimeReleaseBuildBinding(
          releaseMetafile,
          releaseEnvironment,
          {},
          {
            accountBridgeArtifactPublicKeyBase64url:
              Buffer.alloc(32, 9).toString('base64url'),
          },
        ),
      ).toThrow('packaged component provenance contract')

      const selfConsistentDevelopmentTamper = structuredClone(releaseMetafile)
      selfConsistentDevelopmentTamper.crabcodeTuiBuild.buildConfiguration =
        createTuiRuntimeBuildConfiguration({
          profile: 'development',
          minify: false,
          nodeEnv: 'development',
          accountBridgeConfiguration:
            normalizeTuiRuntimeAccountBridgeConfiguration(releaseEnvironment),
        })
      selfConsistentDevelopmentTamper.crabcodeTuiBuild.buildBinding =
        bindTuiRuntimeBuild(
          selfConsistentDevelopmentTamper.crabcodeTuiBuild,
        )
      expect(() =>
        verifyTuiRuntimeReleaseBuildBinding(
          selfConsistentDevelopmentTamper,
          releaseEnvironment,
        ),
      ).toThrow('required release profile')

      const wrongReleaseEnvironment = {
        ...releaseEnvironment,
        ACCOUNT_BRIDGE_ARTIFACT_PUBLIC_KEY_BASE64URL:
          Buffer.alloc(32, 4).toString('base64url'),
      }
      const selfConsistentConfigurationTamper = structuredClone(releaseMetafile)
      selfConsistentConfigurationTamper.crabcodeTuiBuild.buildConfiguration =
        createTuiRuntimeBuildConfiguration({
          profile: 'release',
          minify: true,
          nodeEnv: 'production',
          accountBridgeConfiguration:
            normalizeTuiRuntimeAccountBridgeConfiguration(
              wrongReleaseEnvironment,
              { required: true },
            ),
        })
      selfConsistentConfigurationTamper.crabcodeTuiBuild.buildBinding =
        bindTuiRuntimeBuild(
          selfConsistentConfigurationTamper.crabcodeTuiBuild,
        )
      let configurationMismatchMessage = ''
      try {
        verifyTuiRuntimeReleaseBuildBinding(
          selfConsistentConfigurationTamper,
          releaseEnvironment,
        )
      } catch (error) {
        configurationMismatchMessage = String(error)
      }
      expect(configurationMismatchMessage).toContain(
        'public configuration binding is stale',
      )
      for (const value of Object.values(releaseEnvironment)) {
        expect(configurationMismatchMessage).not.toContain(value)
      }
      for (const value of Object.values(wrongReleaseEnvironment)) {
        expect(configurationMismatchMessage).not.toContain(value)
      }

      const staleSchema = structuredClone(releaseMetafile)
      staleSchema.crabcodeTuiBuild.schemaVersion = 2
      staleSchema.crabcodeTuiBuild.buildBinding = bindTuiRuntimeBuild(
        staleSchema.crabcodeTuiBuild,
      )
      expect(() => verifyTuiRuntimeBuildBinding(staleSchema)).toThrow(
        'build identity is invalid',
      )
    } finally {
      rmSync(root, { recursive: true, force: true })
    }
  })
})
