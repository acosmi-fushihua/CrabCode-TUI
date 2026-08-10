import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'fs'
import { join } from 'path'
import { pathToFileURL } from 'url'
import { DiagnosticTrackingService } from '../../src/services/diagnosticTracking.js'
import { fileUriToPath } from '../../src/utils/path.js'

/**
 * 契约：从 `file://` URI 取回文件系统路径**只能**走 fileUriToPath，不能手写
 * `uri.slice(7)` / `uri.replace('file://','')`。
 *
 * 背景：朴素前缀算术只对**我们自己手拼**的 `file://` + 裸路径成立，而 RFC 8089 /
 * LSP / MCP 规范形态是 `file:///C:/…`（三斜杠 + 百分号编码）—— 也就是第三方 IDE
 * 与 MCP 服务端实际回给我们的形态。剥 7 字符会多留一个前导斜杠、把百分号编码原样
 * 留着、把 UNC 主机名当成目录，于是 baseline 查不到、路径显示错、文件名显示成一串
 * %E6%94%AF。
 *
 * 本文件的负向对照是**内建**的：每组都同时断言「朴素剥离给出的是另一个（错的）
 * 结果」。把 fileUriToPath 的实现换回 slice(7)，第一组当场转红。
 */

const NAIVE_STRIP = (uri: string) => uri.slice('file://'.length)

const IS_WIN = process.platform === 'win32'
const ABS = IS_WIN
  ? 'C:\\Users\\u a\\proj\\src\\demo.ts'
  : '/home/u a/proj/src/demo.ts'
const CJK_ABS = IS_WIN
  ? 'C:\\Users\\u\\支付宝 码牌.pptx'
  : '/home/u/支付宝 码牌.pptx'

describe('fileUriToPath — 规范形态还原', () => {
  test('canonical file URI 还原成原生路径，且与朴素剥离结果不同', () => {
    const uri = pathToFileURL(ABS).href

    expect(fileUriToPath(uri)).toBe(ABS)

    // 负向对照：这正是修复前的行为，它给出的是另一个东西
    expect(NAIVE_STRIP(uri)).not.toBe(ABS)
  })

  test('百分号编码（中文 + 空格）被正确解码', () => {
    const uri = pathToFileURL(CJK_ABS).href

    expect(uri).toContain('%') // 前提：canonical 形态确实带编码
    expect(fileUriToPath(uri)).toBe(CJK_ABS)
    expect(NAIVE_STRIP(uri)).toContain('%') // 负向对照：朴素剥离留着编码
  })

  test('我们自己手拼的旧形态也仍然还原得回去（不回归）', () => {
    // POSIX 上手拼恰好凑成合法三斜杠；Windows 上是畸形串，靠 WHATWG 的盘符特例救回
    expect(fileUriToPath(`file://${ABS}`)).toBe(ABS)
  })

  test('非 file:// 输入原样返回', () => {
    expect(fileUriToPath('_crabcode_fs_right:/a/b.ts')).toBe(
      '_crabcode_fs_right:/a/b.ts',
    )
    expect(fileUriToPath('untitled:Untitled-1')).toBe('untitled:Untitled-1')
    expect(fileUriToPath('')).toBe('')
  })

  test('解析失败时退回朴素剥离，永不抛', () => {
    // POSIX 上 `file://C:\…` 的 host 非空，Node 抛 ERR_INVALID_FILE_URL_HOST；
    // Windows 上有盘符特例不抛。两边都不许把异常漏出去。
    const weird = 'file://C:\\Users\\x\\a.ts'
    expect(() => fileUriToPath(weird)).not.toThrow()
    expect(fileUriToPath(weird).length).toBeGreaterThan(0)

    // 上面那条在 win32 上其实走的是成功分支（盘符特例），catch 分支照不到。
    // 编码的路径分隔符是**两个平台都**会让 Node 抛 ERR_INVALID_FILE_URL_PATH 的输入，
    // 用它才真正覆盖到兜底。
    const encodedSep = 'file:///C:/a/b%2Fc.txt'
    expect(() => fileUriToPath(encodedSep)).not.toThrow()
    expect(fileUriToPath(encodedSep)).toBe('/C:/a/b%2Fc.txt') // 兜底 = 旧的朴素剥离行为
  })

  test.skipIf(!IS_WIN)('UNC：主机名进路径而不是当成目录', () => {
    expect(fileUriToPath('file://server/share/a.ts')).toBe(
      '\\\\server\\share\\a.ts',
    )
    // 负向对照：朴素剥离把主机名读成了相对目录
    expect(NAIVE_STRIP('file://server/share/a.ts')).toBe('server/share/a.ts')
  })
})

describe('diagnosticTracking::normalizeFileUri — baseline key 必须跨形态一致', () => {
  // normalizeFileUri 是私有纯函数；bracket access 直接打真源，不复刻实现。
  const normalize = (uri: string): string =>
    (
      DiagnosticTrackingService.getInstance() as unknown as {
        normalizeFileUri(u: string): string
      }
    ).normalizeFileUri(uri)

  test('IDE 回的 canonical URI 与 baseline 的裸路径归一到同一个 key', () => {
    // 这是本次缺陷的**决定性判据**：beforeFileEdited 用裸 filePath 建 baseline，
    // getNewDiagnostics 用 IDE 回的 uri 查 baseline。两者归一结果不等 ⇒ 查不到 ⇒
    // 诊断跟踪整条链静默失效。
    expect(normalize(pathToFileURL(ABS).href)).toBe(normalize(ABS))
  })

  test('我们自己手拼的旧形态同样归一到该 key（不回归）', () => {
    expect(normalize(`file://${ABS}`)).toBe(normalize(ABS))
  })

  test('_crabcode_fs_* 自定义 scheme 仍按字面剥前缀', () => {
    // 它们不是 URI，不能走 fileUriToPath
    expect(normalize(`_crabcode_fs_right:${ABS}`)).toBe(normalize(ABS))
    expect(normalize(`_crabcode_fs_left:${ABS}`)).toBe(normalize(ABS))
  })
})

describe('消费点接线 — source contract', () => {
  // 原语本身已被上面充分覆盖；这里只钉「调用点真的接上了」，断言保持窄口径
  // （只认函数名与被淘汰的写法），不锁整段代码块，免得换行/改名就假红。
  const REPO_ROOT = join(import.meta.dir, '..', '..')
  const read = (rel: string) => readFileSync(join(REPO_ROOT, rel), 'utf8')

  const SITES = ['src/services/diagnosticTracking.ts']

  test.each(SITES)('%s 真的调用了 fileUriToPath（不只是 import）', site => {
    // 断 `fileUriToPath(` 而不是裸函数名：只留 import、删掉调用的情况必须红。
    // 不带实参名，免得编译器/格式化器改名后出现假红。
    expect(read(site)).toContain('fileUriToPath(')
  })

  test.each(SITES)('%s 不再手写 file:// 前缀算术', site => {
    const src = read(site)
    expect(src).not.toContain(".replace('file://', '')")
    expect(src).not.toContain('.replace("file://", "")')
    expect(src).not.toContain('uri.slice(7)')
  })

  test('发给 IDE / MCP 对端的协议字段用 pathToFileURL 构造', () => {
    // 手拼 `file://` + Windows 路径不是合法 URI（反斜杠不是 RFC 3986 字符），
    // 而对端是第三方实现、解析器不可知。
    expect(read('src/services/diagnosticTracking.ts')).toContain(
      'pathToFileURL(filePath).href',
    )
    expect(read('src/services/mcp/connectionAndFetch.ts')).toContain(
      'pathToFileURL(getOriginalCwd()).href',
    )
  })
})
