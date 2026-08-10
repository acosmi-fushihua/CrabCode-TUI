/**
 * W-OFFICE-PROFILE-URL §4 举一反三（2026-08-06）—— LSP 诊断"清除键"格式契约。
 *
 * 同根因第二例：FileEditTool / FileWriteTool 编辑完文件后调
 * `clearDeliveredDiagnosticsForFile(\`file://\${absPath}\`)` 手拼了一个 file URL，
 * 而 `deliveredDiagnostics` 的键根本不是 URL —— 它由
 * `passiveFeedback.ts::formatDiagnosticsForAttachment` 产生，那里已明确把 LSP 的
 * `params.uri` 经 `fileURLToPath` 归一成**裸文件系统路径**（源注释：'normalize to
 * file system path'）。registry 侧是精确字符串 `has` / `delete`，零归一。
 *
 * 于是这个清除调用在**所有平台**恒不命中（不只是 Windows —— POSIX 上
 * `file:///a/b.ts` 同样 ≠ `/a/b.ts`）：编辑一个文件后，上一轮已投递的同一条诊断
 * 会被跨轮去重永久吃掉，用户再也看不到"我改完了但这个错还在"。
 *
 * 本文件钉两件事：
 *   1. 生产者格式契约 —— formatDiagnosticsForAttachment 产出的 uri 是裸路径；
 *   2. 消费者行为契约 —— 用裸路径清除**有效**，用 `file://` + 路径清除**无效**
 *      （后者即修复前的实参，作为自带的负向对照）。
 * 任何一侧改格式，这里必红，提示同步另一侧。
 */
import { beforeEach, describe, expect, test } from 'bun:test'
import * as path from 'node:path'
import { pathToFileURL } from 'node:url'

import type { Diagnostic } from '../../src/services/diagnosticTracking.js'
import {
  checkForLSPDiagnostics,
  clearDeliveredDiagnosticsForFile,
  registerPendingLSPDiagnostic,
  resetAllLSPDiagnosticState,
} from '../../src/services/lsp/LSPDiagnosticRegistry.js'
import { formatDiagnosticsForAttachment } from '../../src/services/lsp/passiveFeedback.js'

/** 平台真实形态的绝对路径（win32 带盘符 + 空格，POSIX 带空格）。 */
const ABS_FILE =
  process.platform === 'win32'
    ? 'C:\\Users\\u a\\proj\\src\\demo.ts'
    : '/home/u a/proj/src/demo.ts'

const DIAG: Diagnostic = {
  message: "Cannot find name 'foo'.",
  severity: 'Error',
  range: { start: { line: 3, character: 8 }, end: { line: 3, character: 11 } },
  source: 'ts',
  code: '2304',
}

/** 走一遍投递；返回本轮实际交付的诊断条数。 */
function deliverOnce(uri: string): number {
  registerPendingLSPDiagnostic({
    serverName: 'tsserver',
    files: [{ uri, diagnostics: [{ ...DIAG }] }],
  })
  return checkForLSPDiagnostics()
    .flatMap(r => r.files)
    .reduce((n, f) => n + f.diagnostics.length, 0)
}

beforeEach(() => {
  resetAllLSPDiagnosticState()
})

describe('LSP 诊断键格式 —— 生产者契约', () => {
  test('formatDiagnosticsForAttachment 把 file:// URI 归一成裸文件系统路径', () => {
    const files = formatDiagnosticsForAttachment({
      uri: pathToFileURL(ABS_FILE).href,
      diagnostics: [
        {
          message: DIAG.message,
          severity: 1,
          range: DIAG.range,
          source: DIAG.source,
          code: DIAG.code,
        },
      ],
    })
    expect(files).toHaveLength(1)
    // 这就是 deliveredDiagnostics 的键 —— 裸路径，不是 URL。
    expect(files[0]!.uri).toBe(ABS_FILE)
    expect(files[0]!.uri.startsWith('file://')).toBe(false)
  })

  test('非 file:// 的 uri（如 untitled:）原样透传', () => {
    const files = formatDiagnosticsForAttachment({
      uri: 'untitled:Untitled-1',
      diagnostics: [],
    })
    expect(files[0]!.uri).toBe('untitled:Untitled-1')
  })
})

describe('LSP 诊断键格式 —— 消费者契约（clearDeliveredDiagnosticsForFile）', () => {
  test('跨轮去重确实生效（前提校验：没有它，下面两条都没有意义）', () => {
    expect(deliverOnce(ABS_FILE)).toBe(1)
    // 同一条诊断第二轮必须被吃掉，否则本测试的判据不成立。
    expect(deliverOnce(ABS_FILE)).toBe(0)
  })

  test('用裸路径清除 → 同一条诊断可以再次投递', () => {
    expect(deliverOnce(ABS_FILE)).toBe(1)
    expect(deliverOnce(ABS_FILE)).toBe(0)

    clearDeliveredDiagnosticsForFile(ABS_FILE)

    expect(deliverOnce(ABS_FILE)).toBe(1)
  })

  test('负向对照：用 `file://` + 路径清除 → 无效（这是修复前 FileEdit/FileWriteTool 的实参）', () => {
    expect(deliverOnce(ABS_FILE)).toBe(1)
    expect(deliverOnce(ABS_FILE)).toBe(0)

    // 修复前两处调用点传的就是这个值。它与 registry 的键（裸路径）永不相等，
    // 在 POSIX 上同样不相等 —— 所以这是个跨平台的死调用，不是 Windows 专属。
    clearDeliveredDiagnosticsForFile(`file://${ABS_FILE}`)

    expect(deliverOnce(ABS_FILE)).toBe(0)
  })

  test('负向对照：规范 file URL 清除同样无效（键不是 URL，改用 pathToFileURL 也救不回来）', () => {
    expect(deliverOnce(ABS_FILE)).toBe(1)
    expect(deliverOnce(ABS_FILE)).toBe(0)

    clearDeliveredDiagnosticsForFile(pathToFileURL(ABS_FILE).href)

    expect(deliverOnce(ABS_FILE)).toBe(0)
  })

  test('清除只影响目标文件，不误伤同目录其它文件', () => {
    const other = path.join(path.dirname(ABS_FILE), 'other.ts')
    expect(deliverOnce(ABS_FILE)).toBe(1)
    expect(deliverOnce(other)).toBe(1)
    expect(deliverOnce(ABS_FILE)).toBe(0)
    expect(deliverOnce(other)).toBe(0)

    clearDeliveredDiagnosticsForFile(ABS_FILE)

    expect(deliverOnce(ABS_FILE)).toBe(1)
    expect(deliverOnce(other)).toBe(0)
  })
})
