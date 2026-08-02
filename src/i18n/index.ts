/**
 * CrabCode 国际化（i18n）主模块
 *
 * 用法：
 *   import { t } from '../i18n/index.js'
 *   const msg = t('exit_goodbye')
 *   const msg2 = t('model_set_to', { model: 'best' })
 *
 * 切换语言（默认中文）：
 *   export CRABCODE_UI_LANG=en-US   # 英文
 *   export CRABCODE_UI_LANG=zh-CN   # 中文（默认）
 */

import { getLocale, setLocale, detectLocale, type Locale } from './config.js'
import { enUS } from './locales/en-US.js'
import { zhCN } from './locales/zh-CN.js'
import type { TranslationKey, InterpolationVars } from './types.js'

type Translations = typeof enUS

const translations: Record<Locale, Translations> = {
  'zh-CN': zhCN as unknown as Translations,
  'en-US': enUS,
}

/**
 * 翻译函数
 * @param key 翻译键
 * @param vars 插值变量，用于替换 {varName} 占位符
 * @returns 当前语言的翻译字符串
 */
export function t(key: TranslationKey, vars?: InterpolationVars): string {
  const locale = getLocale()
  const dict = translations[locale] ?? translations['zh-CN']
  let text: string = dict[key] ?? enUS[key] ?? key

  if (vars) {
    for (const [k, v] of Object.entries(vars)) {
      text = text.replace(new RegExp(`\\{${k}\\}`, 'g'), String(v))
    }
  }

  return text
}

export { getLocale, setLocale, detectLocale }
export type { Locale, TranslationKey, InterpolationVars }

const DOCS_HOST: Record<Locale, string> = {
  'zh-CN': 'https://acosmi.com',
  'en-US': 'https://acosmi.ai',
}

const DOCS_LANG: Record<Locale, 'zh' | 'en'> = {
  'zh-CN': 'zh',
  'en-US': 'en',
}

/**
 * 拼出 CrabCode 帮助站的 URL，按当前 UI 语言挂到
 * acosmi.com (zh) 或 acosmi.ai (en) 下的 /<lang>/docs/crabcode/<slug>。
 *
 * 用法:
 *   docsUrl()                                 → https://acosmi.com/zh/docs/crabcode
 *   docsUrl('overview')                       → .../zh/docs/crabcode/overview
 *   docsUrl('providers-china-region')         → .../zh/docs/crabcode/providers-china-region
 *   docsUrl('data-usage')                     → .../zh/docs/crabcode/data-usage
 *
 * 注意：文档站路由是单段 slug（`[cat]/[slug]`，slug 用连字符），不支持嵌套
 * 路径段（`providers/china-region` 会 404）。文档页标题是按 UI 语言本地化的，
 * 锚点 id 由标题文字生成（zh 页是中文锚点），因此不要传写死的英文 `#fragment`，
 * 它在 zh 页永远命中不了，只会落到页顶。
 */
export function docsUrl(slug: string = ''): string {
  const locale = getLocale()
  const host = DOCS_HOST[locale]
  const lang = DOCS_LANG[locale]
  const cleaned = slug.replace(/^\/+/, '')
  if (!cleaned) return `${host}/${lang}/docs/crabcode`
  return `${host}/${lang}/docs/crabcode/${cleaned}`
}
