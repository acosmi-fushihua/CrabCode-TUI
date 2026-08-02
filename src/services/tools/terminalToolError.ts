/**
 * Terminal tool-error classification and repeat
 * scanning for the agent-loop circuit breaker.
 *
 * A "terminal" tool error CANNOT succeed on retry within the same session:
 * retrying it only burns the user's turn/token budget. The 2026-07-01 xlsx
 * incident looped 221 turns / 55 min on two such errors — Bash "No suitable
 * shell" ×12 (getShellConfig is memoized per session, so a SHELL change never
 * takes effect within the session) and office-engine-missing / LibreOffice
 * convert-failure ×N.
 *
 * Design (audit deviation A, 2026-07-01): classification is STRING-based on the
 * `formatError()` output, NOT `instanceof ShellError` — the "No suitable shell"
 * error is a plain `Error` (Shell.ts:139), and `formatError` renders a real
 * `ShellError` as "Exit code N + stderr" WITHOUT that phrase. Treating every
 * `ShellError` (ordinary command failures carry exit codes) as terminal would
 * misfire the breaker on retryable command failures. The marker embedded in the
 * tool_result content mirrors the proven `InputValidationError: ` prefix that
 * `countRecentInputValidationErrors` (toolExecution.ts) already scans for.
 */

import type { Message } from '../../types/message.js'

/**
 * N+1 hard-stop threshold. On the 3rd same-signature terminal error the query
 * loop hard-stops (query.ts). Occurrences 1–2 only escalate the model-facing
 * "stop retrying" hint (two-stage: N=2 soft nudge → N+1 hard stop).
 */
export const TERMINAL_CIRCUIT_BREAK_THRESHOLD = 3

/**
 * Occurrence 扫描上限（单位:user 消息条数）。marker 文案的语义
 * 都是"**会话内**"同签名计数——2026-07-04 审计 J3:此前的 10 条滑窗让隔了几轮
 * 的同签名重复从头计数、熔断永不触发。改为会话级全扫、cap 最近 200 条 user 消息
 * （防超长会话 O(n) 扫描;仅 terminal 命中时才扫，rare-path）。与
 * countRecentInputValidationErrors 的 10 条滑窗**刻意分离**:输入校验错是高频
 * 瞬态（近期窗口即代表性），终态错是低频确定性（跨越任意距离都算数）。
 */
export const OCCURRENCE_SCAN_CAP = 200

export type TerminalToolClassification = {
  /**
   * Stable category token. Identical causes collapse to one signature so
   * repeats accrue even across different file arguments (e.g. 3 distinct xlsx
   * files all hitting office_engine_missing count as 3 toward the breaker).
   */
  signature: string
  /** Model/user-facing explanation of WHY this is terminal. */
  reason: string
}

export type TerminalToolErrorContext = {
  toolName?: string
  input?: unknown
}

function isOsascriptBashInvocation(
  context: TerminalToolErrorContext | undefined,
): boolean {
  if (context?.toolName !== 'Bash') return false
  if (
    typeof context.input !== 'object' ||
    context.input === null ||
    !('command' in context.input) ||
    typeof context.input.command !== 'string'
  ) {
    return false
  }
  return /(?:^|&&|\|\||;|\n)\s*(?:(?:env\s+)?[A-Za-z_][A-Za-z0-9_]*=[^\s]+\s+)*(?:(?:"|')?\/usr\/bin\/)?osascript(?:"|')?(?:\s|$)/u.test(
    context.input.command,
  )
}

/**
 * Classify a tool runtime error (already rendered via `formatError`) as
 * session-terminal. Returns null for anything retryable. Deliberately a small,
 * high-precision allowlist — the general "stuck with no forward progress" case
 * is covered by the no-progress watchdog (query.ts), not here.
 *
 * 收录原则：解析链
 * "确定性终态枚举制" —— 凡"同输入同会话重试必然同败"的错误消息，其产生处必须
 * 同 PR 在此收录签名（产生处有锚注释指回本表；判据是字符串，文案即契约）。
 * "换参数可解"的错误（token 超限 / 文件超 50MB 等）不收录，由看门狗兜底。
 */
export function classifyTerminalToolError(
  formattedError: string,
  context?: TerminalToolErrorContext,
): TerminalToolClassification | null {
  // The computer-control incident repeatedly retried an AppleScript fallback
  // after macOS had already returned the deterministic assistive-access error
  // -1719. Ordinary ShellError text remains echoable and is excluded below;
  // this narrow exception additionally proves that the failed Bash input
  // actually invoked osascript, so a command that merely prints the same text
  // cannot manufacture a terminal marker.
  if (
    isOsascriptBashInvocation(context) &&
    /(?:not allowed assistive access|is not authorized to send Apple events|\(-1719\))/iu.test(
      formattedError,
    )
  ) {
    return {
      signature: 'osascript_assistive_access_denied',
      reason:
        'macOS 已拒绝该 osascript 辅助功能/Apple Events 路径；它不是 CrabCode 第一方电脑控制的后备通道。当前授权或应用身份不变时，原样重试必然同样失败。',
    }
  }
  // 回音室排除（2026-07-04 审计 J2）:真 ShellError 经 formatError 渲染为
  // "Exit code N" + stderr + stdout（src/utils/toolErrors.ts）,而普通命令的
  // stderr/stdout 可能**回显**任意签名短语（cat 相关源文件、编译错误引用文案、
  // 用户脚本 echo 等）→ 误判终态、假熔断。全部终态签名的 producer 都是 plain
  // Error（绝不以 "Exit code N" 开头）,以渲染形态前缀硬排除:零漏杀、零误杀。
  if (/^Exit code \d+/.test(formattedError)) return null
  if (formattedError.includes('No suitable shell found')) {
    return {
      signature: 'no_suitable_shell',
      reason:
        '当前会话内没有可用的 Posix shell。shell 配置在进程启动时已固定（会话内更改 SHELL / CRABCODE_SHELL 不会热重载生效），因此本会话内重试任何 shell 命令都必然同样失败。',
    }
  }
  if (
    formattedError.includes(
      'office parsing needs a parsing engine, which is not installed',
    )
  ) {
    return {
      signature: 'office_engine_missing',
      reason:
        '本机未安装 office 解析引擎（LibreOffice / pandoc），本会话内无法解析该 office 文档；即使安装引擎也需重启会话才生效。',
    }
  }
  if (
    formattedError.includes('failed to convert') &&
    formattedError.includes('to PDF')
  ) {
    return {
      signature: 'office_convert_failed',
      reason:
        'office → PDF 转换引擎对该文件确定性失败（相同输入重试结果相同）。',
    }
  }
  if (
    formattedError.includes('timed out converting') &&
    formattedError.includes('to PDF')
  ) {
    return {
      signature: 'office_convert_timeout',
      reason:
        'LibreOffice 转换该文件超时（内置看门狗已等满全额超时）。同一文件重试大概率再次超时，每次白等数分钟；不要原样重试。可尝试其他文件，或让用户手动把文件转换为 PDF 后再读。',
    }
  }
  if (
    formattedError.includes(
      'PDF page rendering requires poppler, which is not installed',
    )
  ) {
    return {
      signature: 'poppler_missing',
      reason:
        '本机未安装 poppler，本会话内 PDF 页图渲染必然失败；即使安装也需重启会话才生效。请引导用户去设置 → 多媒体安装 poppler（或 brew/apt 手动安装）。',
    }
  }
  if (
    formattedError.includes('Reading full PDFs is not supported with this model')
  ) {
    return {
      signature: 'pdf_model_unsupported',
      reason:
        '当前模型不支持整读该 PDF，且本机缺少页图渲染引擎；换用 pages 参数同样走页图链、同因同败。换支持 PDF 的模型或安装 poppler 前，重试读取该 PDF 必然失败。',
    }
  }
  if (formattedError.includes('PDF is password-protected')) {
    return {
      signature: 'pdf_password_protected',
      reason:
        '该 PDF 有密码保护，同一文件重试必然同样失败。请向用户索取无密码版本。',
    }
  }
  // pdf_corrupted 三变体共享一个签名（2026-07-04 审计 J1）:签名键=病因不=文案。
  // 三处 producer 的 reason 字面都是 'corrupted'（pdf.ts stderr 判损 / magic
  // bytes 缺失 / pdftoppm 零页产出）,共享 occurrence 桶才能跨变体合并计数。
  if (
    formattedError.includes('PDF file is corrupted or invalid') ||
    formattedError.includes('File is not a valid PDF') ||
    formattedError.includes('pdftoppm produced no output pages')
  ) {
    return {
      signature: 'pdf_corrupted',
      reason: '该 PDF 文件已损坏或格式非法，同一文件重试必然同样失败。',
    }
  }
  if (formattedError.includes('PDF file is empty')) {
    return {
      signature: 'pdf_empty',
      reason:
        '该 PDF 文件为空（0 字节），同一文件重试必然同样失败。请向用户确认文件是否完整。',
    }
  }
  // J6（2026-07-04 审计 PR-8）:网关 402 权益不足在媒体生成路径的终态。
  // producer 真源 = MediaGenerationTool/generate.ts 的
  // MEDIA_ENTITLEMENT_MISSING_MESSAGE_PREFIX（权益类错误是该工具"返回不 throw"
  // 契约的文档化例外,专为抵达本分类器)。
  if (formattedError.includes('媒体生成权益不足')) {
    return {
      signature: 'media_entitlement_missing',
      reason:
        '该账号当前无可用媒体生成权益（网关 HTTP 402），本会话内重试必然同样失败。请引导用户购买包含生成模型的套餐后重启会话。',
    }
  }
  // W-WEBFETCH-FAKEIP-SSRF PR-R1a（2026-07-24 审计）:WebFetch 本地 SSRF 守卫
  // 拒绝是解析链确定性终态——同一 URL 同会话重试必然同样被拒（真 SSRF 目标恒拒；
  // fake-ip 误杀已由环境探测放行，剩余拒绝即真终态）。producer 真源 =
  // src/tools/WebFetchTool/utils.ts::WEB_FETCH_SSRF_BLOCKED_PREFIX（文案即契约，
  // 判据 = 该前缀字面；回音室由入口 "Exit code N" 硬排除兜住）。
  if (formattedError.includes('WebFetch blocked:')) {
    return {
      signature: 'web_fetch_ssrf_blocked',
      reason:
        'WebFetch 本地 SSRF 守卫已确定性拒绝该目标（解析地址属本地/私有/保留段）。同一 URL 在本会话内重试必然同样失败；请改用其它可达来源，或按错误信息中的指引让用户调整代理客户端 / 会话代理后重启会话再试。',
    }
  }
  return null
}

/**
 * Build the machine-scannable + model-readable marker appended to a terminal
 * tool_result's content. The `signature`/`occurrence` attributes are read back
 * by the query loop (summarizeTurnToolResults) and by countRecentTerminalToolErrors.
 */
export function buildTerminalErrorMarker(
  cls: TerminalToolClassification,
  occurrence: number,
): string {
  const stopGuidance =
    occurrence >= 2
      ? `这已是本会话内第 ${occurrence} 次同一终态失败。停止一切重试与"换花样"绕行；如无其它可行路径，立即调用 AskUserQuestion 向用户说明并索取所需（例如安装缺失组件或重启会话）。继续重试只会白白消耗用户的轮次/代币预算。`
      : '此错误在本会话内不可重试。请勿重试同类操作；如无其它可行路径，调用 AskUserQuestion 向用户说明并索取所需（例如安装缺失组件或重启会话）。'
  return (
    `<terminal_tool_error signature="${cls.signature}" occurrence="${occurrence}">` +
    `${cls.reason} ${stopGuidance}` +
    `</terminal_tool_error>`
  )
}

const MARKER_RE = /<terminal_tool_error signature="([^"]+)" occurrence="(\d+)">/g

function textOfToolResult(content: unknown): string {
  if (typeof content === 'string') return content
  if (Array.isArray(content)) {
    return content
      .filter(c => c.type === 'text')
      .map(c => c.text)
      .join('\n')
  }
  return ''
}

/**
 * Count how many is_error tool_result blocks in this SESSION already carry a
 * terminal marker for `signature`（会话级、cap OCCURRENCE_SCAN_CAP 条 user 消息）。
 * Keys on the terminal signature (cause identity) rather than tool name —
 * the signature already encodes the cause, and a cause can span tools.
 */
export function countRecentTerminalToolErrors(
  signature: string,
  messages: Message[],
): number {
  let count = 0
  let scanned = 0
  for (let i = messages.length - 1; i >= 0 && scanned < OCCURRENCE_SCAN_CAP; i--) {
    const msg = messages[i]
    if (!msg || msg.type !== 'user') continue
    scanned += 1
    const content = msg.message.content
    if (typeof content === 'string') continue
    if (!Array.isArray(content)) continue
    for (const block of content) {
      if (block.type !== 'tool_result') continue
      if (!block.is_error) continue
      if (textOfToolResult(block.content).includes(`signature="${signature}"`)) {
        count += 1
      }
    }
  }
  return count
}

export type TurnToolResultSummary = {
  /** True when this turn produced at least one tool_result block. */
  hadToolResults: boolean
  /** True when every tool_result block this turn is is_error (zero success). */
  allErrored: boolean
  /** Highest-occurrence terminal marker seen among this turn's error results. */
  terminal: { signature: string; occurrence: number } | null
}

/**
 * Summarize a single turn's tool_result blocks for the query-loop circuit
 * breaker + no-progress watchdog. `messages` = the turn's freshly produced
 * tool results (query.ts `toolResults`); attachment messages (memory/skill/
 * steer) carry no tool_result blocks and are ignored.
 */
export function summarizeTurnToolResults(
  messages: Message[],
): TurnToolResultSummary {
  let total = 0
  let errored = 0
  let terminal: { signature: string; occurrence: number } | null = null
  for (const msg of messages) {
    if (!msg || msg.type !== 'user') continue
    const content = msg.message.content
    if (!Array.isArray(content)) continue
    for (const block of content) {
      if (block.type !== 'tool_result') continue
      total += 1
      if (!block.is_error) continue
      errored += 1
      const text = textOfToolResult(block.content)
      let m: RegExpExecArray | null
      MARKER_RE.lastIndex = 0
      while ((m = MARKER_RE.exec(text)) !== null) {
        const occurrence = Number(m[2])
        if (!terminal || occurrence > terminal.occurrence) {
          terminal = { signature: m[1]!, occurrence }
        }
      }
    }
  }
  return {
    hadToolResults: total > 0,
    allErrored: total > 0 && errored === total,
    terminal,
  }
}
