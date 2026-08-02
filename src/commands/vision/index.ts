import type { Command } from '../../commands.js'
import { t } from '../../i18n/index.js'

/**
 * /vision — TUI 授权/撤回"文本模型 → 视觉模型"图片兜底（chat media sidecar）
 * 的精确目的地同意。W-VISION-CONSENT-SURFACE PR-R2b（2026-07-24 审计 F5）。
 */
export default {
  type: 'local',
  name: 'vision',
  get description() {
    return t('cmd_vision_desc')
  },
  argumentHint: '[on|off]',
  // 同意授权是一次刻意的交互行为；headless 不提供静默授权通道。
  supportsNonInteractive: false,
  load: () => import('./vision.js'),
} satisfies Command
