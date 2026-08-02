/**
 * /vision — native TUI authorization for the chat media sidecar.
 *
 *   - 启用是"精确目的地同意"事务：绑定**必须等于**写入时刻重新解析的预览
 *     目的地（不接受用户自选模型——目的地由选择器决定，用户只对它点头/摇头，
 *     预览不可得则拒绝且不落任何设置。
 *   - 启用/停用都是 `enabled` + `consent` 的单次
 *     原子嵌套补丁；停用清空旧绑定（目录变更后的新目的地永不继承旧同意）。
 *   - 停用同时是数据撤回边界：清空描述缓存（L1 + 磁盘 + 世代标记）。
 */
import {
  clearChatMediaSidecarCache,
  previewChatSidecarDestination,
} from '../../services/api/chatMediaSidecar.js'
import type { LocalCommandCall, LocalCommandResult } from '../../types/command.js'
import {
  getMediaSidecarConsentBinding,
  isMediaSidecarConsentEnabled,
  type MediaSidecarConsentBinding,
} from '../../utils/mediaSidecar/settings.js'
import { classifyModelImageModality } from '../../utils/model/imageModality.js'
import { getMainLoopModel } from '../../utils/model/model.js'
import { updateSettingsForSource } from '../../utils/settings/settings.js'

export type VisionCommandDeps = {
  getMainModel: typeof getMainLoopModel
  preview: typeof previewChatSidecarDestination
  isConsentEnabled: typeof isMediaSidecarConsentEnabled
  getConsentBinding: typeof getMediaSidecarConsentBinding
  classifyModality: typeof classifyModelImageModality
  writeSettings: typeof updateSettingsForSource
  clearCache: typeof clearChatMediaSidecarCache
}

const DEFAULT_DEPS: VisionCommandDeps = {
  getMainModel: getMainLoopModel,
  preview: previewChatSidecarDestination,
  isConsentEnabled: isMediaSidecarConsentEnabled,
  getConsentBinding: getMediaSidecarConsentBinding,
  classifyModality: classifyModelImageModality,
  writeSettings: updateSettingsForSource,
  clearCache: clearChatMediaSidecarCache,
}

const USAGE =
  '用法：/vision（查看状态与预览目的地）· /vision on（授权预览的目的地模型）· /vision off（撤回授权并清空描述缓存）'

function bindingLabel(binding: MediaSidecarConsentBinding): string {
  return `${binding.provider}/${binding.modelId}`
}

/**
 * The destination provider differs from the one serving the current chat model.
 *
 * 2026-07-27: same-provider used to be a hard boundary, which made the fallback
 * structurally impossible for any main model whose provider ships no vision
 * model. The boundary was only ever a proxy for "the user knows who receives
 * the image"; the consent binding records `(provider, modelId)` exactly and
 * serves that purpose directly. The proxy is gone, so the disclosure it used to
 * imply must now be stated outright — otherwise authorization would silently
 * widen who receives the pixels.
 */
function isCrossProvider(destination: {
  mainProvider: string
  provider: string
}): boolean {
  return destination.provider !== destination.mainProvider
}

function crossProviderNotice(destination: {
  mainProvider: string
  provider: string
  modelId: string
}): string {
  return (
    `注意：该目的地与当前对话模型不同厂——图片将发往 ${destination.provider} 的 ` +
    `${destination.modelId}，而当前对话模型由 ${destination.mainProvider} 提供。`
  )
}

function statusText(deps: VisionCommandDeps): string {
  const mainModel = deps.getMainModel()
  const modality = deps.classifyModality(mainModel)
  const lines: string[] = []
  lines.push(
    `当前主模型：${mainModel}（图片输入：${
      modality === 'supported'
        ? '支持——发图直达模型，无需兜底'
        : modality === 'text_only'
          ? '不支持（纯文本模型）'
          : '未声明（按隐私边界不盲发）'
    }）`,
  )
  const enabled = deps.isConsentEnabled()
  const binding = deps.getConsentBinding()
  lines.push(
    `视觉兜底授权：${
      enabled && binding ? `已授权 → ${bindingLabel(binding)}` : '未授权（默认关闭）'
    }`,
  )
  const { destination, reason } = deps.preview(mainModel)
  if (destination) {
    lines.push(
      `当前可绑定的目的地：${destination.provider}/${destination.modelId}`,
    )
    if (isCrossProvider(destination)) {
      lines.push(crossProviderNotice(destination))
    }
    if (
      enabled &&
      binding &&
      (binding.provider !== destination.provider ||
        binding.modelId !== destination.modelId)
    ) {
      lines.push(
        '注意：现有授权与当前目的地不一致（目录已变更），兜底已安全降级为占位文字；执行 /vision on 可重新绑定。',
      )
    }
    if (!enabled || !binding) {
      lines.push('执行 /vision on 即授权把降级图片发给上述目的地模型做描述。')
    }
  } else {
    lines.push(
      `当前无可绑定目的地（原因：${reason ?? 'no_eligible_model'}）——同厂商目录内没有可用的视觉聊天模型时，兜底只能落占位文字。`,
    )
  }
  lines.push(USAGE)
  return lines.join('\n')
}

function enableText(deps: VisionCommandDeps): string {
  const mainModel = deps.getMainModel()
  // 绑定 = 写入时刻重新解析的预览结果；授权时重解析以闭合目录漂移窗口。
  const { destination, reason } = deps.preview(mainModel)
  if (!destination) {
    return `未授权：当前无可绑定目的地（原因：${reason ?? 'no_eligible_model'}），设置未变更。`
  }
  const consent: MediaSidecarConsentBinding = {
    provider: destination.provider,
    modelId: destination.modelId,
  }
  const result = deps.writeSettings('userSettings', {
    mediaSidecar: {
      enabled: true,
      consent,
    },
  })
  if (result.error) {
    return `授权写入失败：${result.error.message}`
  }
  return (
    `已授权视觉兜底 → ${bindingLabel(consent)}。此后主模型不支持图片时，` +
    '图片将发给该模型生成文字描述（仅此一个精确目的地；目录变更会使授权自动失效）。' +
    (isCrossProvider(destination) ? `\n${crossProviderNotice(destination)}` : '') +
    '\n撤回：/vision off'
  )
}

function disableText(deps: VisionCommandDeps): string {
  const result = deps.writeSettings('userSettings', {
    mediaSidecar: {
      enabled: false,
      consent: null,
    },
  })
  if (result.error) {
    return `撤回写入失败：${result.error.message}`
  }
  // 同意撤回也是数据撤回边界：清空 L1 + 磁盘描述缓存并推进世代标记。
  deps.clearCache()
  return '已撤回视觉兜底授权并清空描述缓存；此后降级图片只使用占位文字。重新授权：/vision on'
}

export function visionCommand(
  args: string,
  deps: VisionCommandDeps = DEFAULT_DEPS,
): LocalCommandResult {
  const action = args.trim().toLowerCase()
  if (action === '') return { type: 'text', value: statusText(deps) }
  if (action === 'on') return { type: 'text', value: enableText(deps) }
  if (action === 'off') return { type: 'text', value: disableText(deps) }
  return { type: 'text', value: `无法识别的参数「${args.trim()}」。${USAGE}` }
}

export const call: LocalCommandCall = async args => visionCommand(args)
