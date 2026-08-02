/**
 * CrabCode i18n 语言配置
 *
 * 语言检测优先级：
 *  1. GlobalConfig.uiLanguage（~/.crabcode/config.json 中的 uiLanguage 字段）
 *  2. 环境变量 CRABCODE_UI_LANG（如 "zh-CN" 或 "en-US"）
 *  3. 默认：zh-CN（中文）
 *
 * 推荐通过 /config 面板或初始化引导中选择语言。
 * 也可临时通过环境变量覆盖：
 *   PowerShell:  $env:CRABCODE_UI_LANG = "en-US"
 *   CMD:         set CRABCODE_UI_LANG=en-US
 *   bash/WSL:    export CRABCODE_UI_LANG=en-US
 */

export type Locale = 'zh-CN' | 'en-US'

export const SUPPORTED_LOCALES: readonly Locale[] = ['zh-CN', 'en-US'] as const

/**
 * 从环境变量中解析 Locale，无法识别时返回 null
 */
function parseEnvLocale(raw: string): Locale | null {
  const s = raw.trim().toLowerCase()
  if (s === 'en' || s === 'en-us' || s === 'english') return 'en-US'
  if (s === 'zh' || s === 'zh-cn' || s === 'chinese' || s === '中文') return 'zh-CN'
  return null
}

/**
 * GlobalConfig uiLanguage 提供者 —— 由 utils/config.ts 在 enableConfigs() 时
 * 注册（依赖反转）。本模块必须保持零 import 叶子：曾经这里用
 * `require('../utils/config.js')` 在模块顶层求值整个 config 依赖图，该图经
 * settings/commands 等多路环回 i18n，环上 TDZ 抛错沿同步 require 栈穿透
 * utils/config 的 module 求值、再被本函数的 try/catch 静默吞掉 —— Bun 把
 * utils/config 缓存为「已注册但 body 永不执行」的死模块，后续所有 import
 * 拿到的 enableConfigs 调用必中 `configReadingAllowed` TDZ。
 */
let configLocaleProvider: (() => string | undefined) | null = null

export function registerConfigLocaleProvider(
  provider: () => string | undefined,
): void {
  configLocaleProvider = provider
}

/**
 * 检测当前语言，按优先级：GlobalConfig > 环境变量 > 默认 zh-CN
 *
 * GlobalConfig 经 registerConfigLocaleProvider 注册的回调读取（enableConfigs
 * 之前未注册 → 降级环境变量），本模块不直接依赖 utils/config。
 */
export function detectLocale(): Locale {
  // 1. 尝试从 GlobalConfig 读取（经注册的 provider）
  if (configLocaleProvider) {
    try {
      const v = configLocaleProvider()
      if (v && SUPPORTED_LOCALES.includes(v as Locale)) {
        return v as Locale
      }
    } catch {
      // config 尚不可读时降级到环境变量
    }
  }

  // 2. 环境变量
  if (typeof process !== 'undefined') {
    const envRaw =
      process.env['CRABCODE_UI_LANG'] ?? process.env['CRABCODE_UI_LANG'] ?? ''
    const fromEnv = parseEnvLocale(envRaw)
    if (fromEnv) return fromEnv
  }

  // 3. 默认中文
  return 'zh-CN'
}

// 惰性初始化：不在模块求值期触发 detectLocale（见 configLocaleProvider 注释，
// 模块顶层求值期做语言检测正是 utils/config 死模块毒化事故的点火点）。
let _locale: Locale | null = null

export function getLocale(): Locale {
  if (_locale === null) {
    _locale = detectLocale()
  }
  return _locale
}

export function setLocale(locale: Locale): void {
  if (SUPPORTED_LOCALES.includes(locale)) {
    _locale = locale
  }
}

/**
 * 重新从 GlobalConfig / 环境变量检测语言并应用。
 * 在 GlobalConfig 更新后（如用户在 /config 中切换语言）调用此函数。
 */
export function reloadLocale(): Locale {
  _locale = detectLocale()
  return _locale
}
