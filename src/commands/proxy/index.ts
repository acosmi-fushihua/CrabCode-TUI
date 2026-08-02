import type { Command } from '../../commands.js'
import { t } from '../../i18n/index.js'

/**
 * /proxy — 网络代理诊断与声明：生效代理 + 系统代理探测 +
 * `use-system` 写入 / `off` 撤销用户级 settings.env。
 * W-SYSPROXY-DISCOVERY P0 + P1（2026-07-24 立项审计 → 实施方案 §二 / §五）。
 *
 * 代理地址属本机网络拓扑
 * 信息，且 use-system 会改写用户设置并切换整机出口——远端配对客户端一律无权。
 */
export default {
  type: 'local',
  name: 'proxy',
  get description() {
    return t('cmd_proxy_desc')
  },
  // headless 排障同样需要；写入面只在用户显式带 use-system / off 参数时触发。
  supportsNonInteractive: true,
  load: () => import('./proxy.js'),
} satisfies Command
