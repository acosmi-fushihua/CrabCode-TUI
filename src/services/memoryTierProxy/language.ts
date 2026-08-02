/**
 * W-MEMORY-SYNERGY W3 (2026-07-16, RC-5) — 记忆产物输出语言（TS 侧）。
 *
 * 分工（与 Rust `libs/acosmi-memory/acosmi-memory-orchestrator/src/
 * output_language.rs` 互为锚注）：
 *   * **行文语言**归这里 —— 全部反向 IPC LLM 调用都在 memoryTierProxy 执行，
 *     `withMemoryLanguageDirective` 在唯一执行点给消息追加输出语言指令，
 *     覆盖 Tier-1/2/3、想象、watch、进化报告的正文行文。此前 Tier prompt
 *     全英文硬编码 → 中文用户拿到英文记忆（中英混合根因之一）。
 *   * **结构语言**归 Rust —— 进化报告的固定章节骨架按语言选模板（契约
 *     #15-8：Tier prompt 模板只住 orchestrator）。
 *
 * 解析优先级与 Rust 侧一致：`memoryLanguage`（zh|en；auto 走下一级）→
 * `uiLanguage`（zh-CN→zh，en-US→en）→ 默认 zh（i18n detectLocale 的产品
 * 默认）。env `CRABCODE_MEMORY_LANGUAGE` 是 Rust 侧的显式覆盖通道；TS 侧
 * 同样尊重它，保证两侧永远同一答案。
 */

import { getGlobalConfig } from '../../utils/config.js'

export type MemoryOutputLanguage = 'zh' | 'en'

function parseLanguage(raw: string | undefined): MemoryOutputLanguage | null {
  if (!raw) return null
  const value = raw.trim().toLowerCase()
  if (value === 'zh' || value === 'zh-cn' || value === 'chinese' || value === '中文') {
    return 'zh'
  }
  if (value === 'en' || value === 'en-us' || value === 'english') {
    return 'en'
  }
  return null
}

/** 生产解析入口（每次 LLM 调用求值一次 —— 配置热更新即时生效）。 */
export function resolveMemoryOutputLanguage(): MemoryOutputLanguage {
  const fromEnv = parseLanguage(process.env.CRABCODE_MEMORY_LANGUAGE)
  if (fromEnv) return fromEnv
  try {
    const config = getGlobalConfig()
    const explicit = parseLanguage(config.memoryLanguage)
    if (explicit) return explicit
    const fromUi = parseLanguage(config.uiLanguage)
    if (fromUi) return fromUi
  } catch {
    // 配置不可读（如极早期启动）→ 产品默认。
  }
  return 'zh'
}

const DIRECTIVE_ZH = `# Output language
Write all natural-language prose in Simplified Chinese (简体中文): memory bodies, descriptions, insight text, hypotheses, report content.
Keep unchanged: code, identifiers, file paths, URLs, JSON keys, frontmatter keys, and \`name:\` slugs (stay snake_case ASCII). When the required output is JSON, apply this only to natural-language string values.`

const DIRECTIVE_EN = `# Output language
Write all natural-language prose in English: memory bodies, descriptions, insight text, hypotheses, report content.
Keep unchanged: code, identifiers, file paths, URLs, JSON keys, frontmatter keys, and \`name:\` slugs (stay snake_case ASCII). When the required output is JSON, apply this only to natural-language string values.`

export function memoryLanguageDirective(
  language: MemoryOutputLanguage,
): string {
  return language === 'zh' ? DIRECTIVE_ZH : DIRECTIVE_EN
}

/**
 * 把语言指令并入请求消息：追加到**第一条 system 消息**的末尾（不动角色
 * 结构 —— 有的 SDK 路径对多条 system 消息的合并语义不稳）；没有 system
 * 消息时在最前面补一条。幂等：已含指令头则原样返回（防重复注入）。
 */
export function withMemoryLanguageDirective(
  messages: { role: string; content: string }[],
  language: MemoryOutputLanguage,
): { role: string; content: string }[] {
  const directive = memoryLanguageDirective(language)
  if (messages.some(message => message.content.includes('# Output language'))) {
    return messages
  }
  const systemIndex = messages.findIndex(message => message.role === 'system')
  if (systemIndex === -1) {
    return [{ role: 'system', content: directive }, ...messages]
  }
  return messages.map((message, index) =>
    index === systemIndex
      ? { ...message, content: `${message.content}\n\n${directive}` }
      : message,
  )
}
