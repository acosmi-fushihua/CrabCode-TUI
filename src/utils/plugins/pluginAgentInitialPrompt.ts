import type { PluginManifest } from './schemas.js'
import {
  loadPluginOptions,
  substitutePluginVariables,
  substituteUserConfigInContent,
} from './pluginOptionsStorage.js'

export function resolvePluginAgentInitialPrompt(
  rawValue: unknown,
  options: {
    pluginPath: string
    sourceName: string
    manifest: PluginManifest
  },
): string | undefined {
  if (typeof rawValue !== 'string' || !rawValue.trim()) return undefined
  const { pluginPath, sourceName, manifest } = options
  let prompt = substitutePluginVariables(rawValue.trim(), {
    path: pluginPath,
    source: sourceName,
  })
  if (manifest.userConfig) {
    prompt = substituteUserConfigInContent(
      prompt,
      loadPluginOptions(sourceName),
      manifest.userConfig,
    )
  }
  return prompt
}
