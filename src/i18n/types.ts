/**
 * CrabCode i18n 类型定义
 */
import type { enUS } from './locales/en-US.js'

export type { Locale } from './config.js'

/**
 * 所有翻译键，从 en-US 基线派生。
 *
 * Every valid key maps to a string in both locale dictionaries.
 */
export type TranslationKey = keyof typeof enUS

/** 插值变量映射 */
export type InterpolationVars = Record<string, string | number>

/** 翻译字典类型（与 enUS 结构相同）。 */
export type TranslationDict = {
  readonly [K in TranslationKey]: string
}
