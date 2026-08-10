/**
 * 沙箱违规反馈环的判据层（W-SANDBOX-ENFORCED-DEADCODE PR-4）
 *
 * 回答一个问题：**这条命令的输出里，哪几行是「沙箱把它拦下来了」？**
 *
 * ## 为什么需要它
 *
 * PR-2/PR-3 之后沙箱是真的了：写 `$HOME` 会被 Landlock/Seatbelt/受限令牌拒绝。
 * 但被拒绝时命令打出来的是 **操作系统的话**（`Permission denied` /
 * `Operation not permitted` / `Access is denied.`），跟「文件不存在」「参数写错了」
 * 长得一模一样。没有这一层，模型只能靠猜；猜错的方向恰好是本立项要消灭的那个 ——
 * 把隔离拒绝当成命令 bug 反复重试，或者一步跳到 `dangerouslyDisableSandbox`
 * 把用户开的隔离直接掀掉。
 *
 * ## 模式表的来源纪律（#0 证据铁律）
 *
 * 这些字符串**没有仓内 producer 可以 import** —— 它们是 libc `strerror` /
 * Seatbelt / Win32 的文案，由内核和 C 运行时产生，不是我们写的。这与 #16 签名表
 * 的 `osascript_assistive_access_denied` 是同一类文档化例外。
 *
 * 所以每一条模式**必须**带 `source` 字段写明出处，且出处要么是
 * (a) 本仓 Rust 侧的实证注释/配置，要么 (b) 明确的 POSIX/Win32 errno 定义。
 * 「我记得 Linux 好像会报这个」不构成 source，不许进表。
 *
 * ## 判据只在「这条命令真的跑在沙箱里」时才生效
 *
 * 本模块只做文本分类；「跑没跑在沙箱里」由调用方
 * (`sandbox-adapter.ts::annotateStderrWithSandboxFailures` 的 `context.sandboxed`)
 * 提供，并且**默认 false**。少了这道门，一条普通的 `cat /root/x` 权限错误会被
 * 标成「沙箱拦截」—— 那是凭空造证据，比不注解糟得多。
 */

/**
 * 一次拒绝的种类。**刻意按「观察到的错误」而不是「猜到的机制」命名**：
 * TS 侧看得到的只有一行文本，看不到是 Landlock 还是 seccomp 还是 Seatbelt
 * 拒的。把 `Permission denied` 标成 `landlock-denied` 就是在编造因果。
 */
export type SandboxDenialKind =
  | 'operation-not-permitted'
  | 'permission-denied'
  | 'read-only-filesystem'
  | 'network-unreachable'
  | 'windows-access-denied'
  | 'seatbelt-deny-log'

export type SandboxDenialPattern = {
  readonly kind: SandboxDenialKind
  /**
   * 大小写不敏感的子串判据。**刻意不用正则**：正则容易写出跨行/回溯陷阱，而
   * 我们逐行扫一个可能有几十万行的输出。
   */
  readonly needle: string
  /** 会产生这条文案的平台（**文档用**，匹配时不按平台筛选，见文件头）。 */
  readonly platforms: readonly ('darwin' | 'linux' | 'win32')[]
  /** 出处。空串不允许（`patterns_all_declare_a_source` 钉住）。 */
  readonly source: string
}

/**
 * 模式表。新增一条必须同时给出 `source`，并在
 * `tests/unit/sandbox-violation-feedback.test.ts` 补正反两个用例。
 */
export const SANDBOX_DENIAL_PATTERNS: readonly SandboxDenialPattern[] = [
  {
    kind: 'operation-not-permitted',
    needle: 'operation not permitted',
    platforms: ['linux', 'darwin'],
    source:
      'POSIX EPERM 的 strerror 文案。本仓 Linux 侧 seccomp 的拒绝动作逐条是 ' +
      '`ScmpAction::Errno(libc::EPERM)`（crates/acosmi-sandbox/src/linux/seccomp.rs）；' +
      'macOS Seatbelt 拒绝同样以 EPERM 返回（crates/acosmi-sandbox/src/platform.rs ' +
      '的 unix-socket notice 自述「否则用户只会看到一个没有解释的 EPERM」）。',
  },
  {
    kind: 'permission-denied',
    needle: 'permission denied',
    platforms: ['linux', 'darwin'],
    source:
      'POSIX EACCES 的 strerror 文案。本仓 Linux 侧 Landlock 的拒绝即 EACCES ' +
      '（crates/acosmi-sandbox/src/linux/async_runner.rs 的验收项自述「写 ' +
      '`/etc/test.txt` 应 EACCES」）。',
  },
  {
    kind: 'read-only-filesystem',
    needle: 'read-only file system',
    platforms: ['linux', 'darwin'],
    source:
      'POSIX EROFS 的 strerror 文案。本仓 Linux 侧把只读挂载点 remount ro ' +
      '（crates/acosmi-sandbox/src/linux/namespace.rs::"remount read-only"），' +
      '写这些路径即 EROFS。',
  },
  {
    kind: 'network-unreachable',
    needle: 'network is unreachable',
    platforms: ['linux', 'darwin'],
    source:
      'POSIX ENETUNREACH 的 strerror 文案。网络隔离生效时对外连接以此形态失败。' +
      '**刻意不收 `Connection refused` 与 DNS 解析失败** —— 那两个在没有沙箱的' +
      '机器上同样天天出现，收进来就是把普通故障标成沙箱拦截。',
  },
  {
    kind: 'windows-access-denied',
    needle: 'access is denied',
    platforms: ['win32'],
    source:
      'Win32 ERROR_ACCESS_DENIED(5) 的 `FormatMessage` 文案。本仓 Windows 后端用' +
      '受限令牌拒绝越界访问（crates/acosmi-sandbox/src/windows/），实测形态见 ' +
      'tests/windows_integration.rs 头注释的 ACCESS DENIED 取证段。',
  },
  {
    kind: 'windows-access-denied',
    needle: 'unauthorizedaccessexception',
    platforms: ['win32'],
    source:
      '.NET/PowerShell 对同一个 Win32 ERROR_ACCESS_DENIED 的包装类型名；' +
      'PowerShellTool 的输出里拒绝多以它出现，而不是裸 `Access is denied.`。',
  },
  {
    kind: 'seatbelt-deny-log',
    needle: 'deny(1) file-',
    platforms: ['darwin'],
    source:
      'macOS Seatbelt 内核拒绝日志行的固定形态（`Sandbox: <proc>(<pid>) ' +
      'deny(1) file-write-data <path>`）。收它是因为它**无歧义**：这行字只有 ' +
      'Seatbelt 会打。',
  },
  {
    kind: 'seatbelt-deny-log',
    needle: 'deny(1) network-',
    platforms: ['darwin'],
    source: '同上，网络类拒绝的形态（`deny(1) network-outbound`）。',
  },
]

/**
 * 嵌套隔离（容器 / namespace）相关的命令 token。命中时注解里多给一条解释，
 * 指向 `sandbox.enableWeakerNestedSandbox`（关联方矩阵 #19：未开启时嵌套容器
 * 命令失败**属预期**，注解要能解释而不是让用户以为坏了）。
 *
 * 这些是命令名不是错误文案，所以判据放在命令/输出行两侧都查。
 */
export const NESTED_ISOLATION_TOKENS: readonly string[] = [
  'docker',
  'podman',
  'unshare',
  'nsenter',
  'bwrap',
  'chroot',
  'systemd-nspawn',
]

/** 一行被判定为沙箱拒绝的记录。 */
export type SandboxDenial = {
  readonly kind: SandboxDenialKind
  /** 原始行（已 trim + 截断，见 `MAX_DENIAL_LINE_CHARS`）。 */
  readonly line: string
}

/**
 * 单行判据。返回命中的第一个 kind，没命中返回 null。
 *
 * 表里的顺序即优先级：`operation not permitted` 在 `permission denied` 之前，
 * 因为 EPERM（策略拒绝）比 EACCES（访问拒绝）更具体地指向「被沙箱拦了」。
 */
export function classifySandboxDenialLine(
  line: string,
): SandboxDenialKind | null {
  const haystack = line.toLowerCase()
  for (const pattern of SANDBOX_DENIAL_PATTERNS) {
    if (haystack.includes(pattern.needle)) {
      return pattern.kind
    }
  }
  return null
}

/** 单条 denial 行在注解里展示的最大长度（超出截断，防一行几万字符）。 */
export const MAX_DENIAL_LINE_CHARS = 240

/** 注解里最多列几条（其余只计数不展开）。 */
export const MAX_DENIALS_IN_ANNOTATION = 5

/**
 * 扫描一次命令输出，挑出所有沙箱拒绝行。
 *
 * 去重按 `kind + line`：一条 `for f in *; do …` 循环会把同一句拒绝打几百遍，
 * 全塞进注解等于把真正的输出挤出模型的视野。
 */
export function collectSandboxDenials(output: string): SandboxDenial[] {
  if (output.length === 0) return []
  const seen = new Set<string>()
  const denials: SandboxDenial[] = []
  for (const rawLine of output.split('\n')) {
    const line = rawLine.replace(/\r$/, '').trim()
    if (line.length === 0) continue
    const kind = classifySandboxDenialLine(line)
    if (kind === null) continue
    const truncated =
      line.length > MAX_DENIAL_LINE_CHARS
        ? `${line.slice(0, MAX_DENIAL_LINE_CHARS)}…`
        : line
    const key = `${kind}\0${truncated}`
    if (seen.has(key)) continue
    seen.add(key)
    denials.push({ kind, line: truncated })
  }
  return denials
}

/**
 * `sandbox.ignoreViolations` 过滤器。
 *
 * ## 这个设置在本 PR 之前从来没有消费者
 *
 * schema 是 `z.record(z.string(), z.array(z.string()))`（`sandboxTypes.ts`），
 * 类型在 `types.ts` 里被写成 `unknown`，而唯一的读取点是把它**原样 JSON**
 * 塞进系统提示词（`BashTool/prompt.ts::getSimpleSandboxSection` 的
 * `Ignored violations:` 行）。也就是说：它的运行期语义从未被任何代码定义过。
 *
 * 本 PR 给出的定义（写在这里就是合同，改它要同 PR 改测试）：
 *
 *   - **键** = 违规种类（`SandboxDenialKind`）或 `'*'`（全部种类）
 *   - **值** = 子串数组，大小写不敏感地匹配**那一行的文本**；`'*'` 作为值
 *     表示该种类整体忽略
 *
 * 形状不对的条目**整条跳过**（不 throw）：这是一个用户手写的设置文件，
 * 一个写错的键不该让整条命令的反馈环失灵。
 *
 * 被忽略的违规**既不进注解也不进计数** —— 用户说了不想看见它。
 */
export function filterIgnoredDenials(
  denials: readonly SandboxDenial[],
  ignoreViolations: unknown,
): SandboxDenial[] {
  if (
    typeof ignoreViolations !== 'object' ||
    ignoreViolations === null ||
    Array.isArray(ignoreViolations)
  ) {
    return [...denials]
  }
  const config = ignoreViolations as Record<string, unknown>
  return denials.filter(denial => {
    for (const key of [denial.kind as string, '*']) {
      const patterns = config[key]
      if (!Array.isArray(patterns)) continue
      for (const pattern of patterns) {
        if (typeof pattern !== 'string' || pattern.length === 0) continue
        if (pattern === '*') return false
        if (denial.line.toLowerCase().includes(pattern.toLowerCase())) {
          return false
        }
      }
    }
    return true
  })
}

/** 注解块的开闭标签。UI 侧 `removeSandboxViolationTags()` 按这对标签剥离。 */
export const SANDBOX_VIOLATIONS_OPEN_TAG = '<sandbox_violations>'
export const SANDBOX_VIOLATIONS_CLOSE_TAG = '</sandbox_violations>'

export type SandboxAnnotationInput = {
  readonly denials: readonly SandboxDenial[]
  /** 后端标识；探测不出来时传 null，注解里如实写「未知」而不是编一个。 */
  readonly backend: string | null
  /** `sandbox.allowUnsandboxedCommands`——决定「出路」那一段怎么写。 */
  readonly allowUnsandboxedCommands: boolean
  /** 原始命令，只用来判嵌套隔离 token（**不会**被写进注解）。 */
  readonly command: string
}

/**
 * 把 denial 列表渲染成模型可读的注解块。
 *
 * 三件事，缺一条这个反馈环就失效：
 * 1. **定性** —— 这是隔离拒绝，不是命令 bug（模型最容易搞错的一步）
 * 2. **点名** —— 具体哪几行、哪个后端拒的
 * 3. **出路** —— 先给不掀掉隔离的解法；`dangerouslyDisableSandbox` **不是**
 *    第一反应（严格档下它根本不存在）
 */
export function formatSandboxViolationAnnotation(
  input: SandboxAnnotationInput,
): string {
  const { denials, backend, allowUnsandboxedCommands, command } = input
  if (denials.length === 0) return ''

  const backendLabel = backend ?? 'unknown'
  const shown = denials.slice(0, MAX_DENIALS_IN_ANNOTATION)
  const hidden = denials.length - shown.length

  const lines: string[] = [
    SANDBOX_VIOLATIONS_OPEN_TAG,
    `The sandbox blocked ${denials.length} operation${denials.length === 1 ? '' : 's'} ` +
      `while running this command (backend: ${backendLabel}).`,
    `These lines are isolation denials, NOT bugs in the command — the command itself may be correct:`,
    ...shown.map(d => `  [${d.kind}] ${d.line}`),
  ]
  if (hidden > 0) {
    lines.push(`  … and ${hidden} more denial line${hidden === 1 ? '' : 's'}`)
  }

  lines.push('What to do:')
  lines.push(
    '  - Prefer staying inside the sandbox: work under the project directory and put temporary files in $TMPDIR.',
  )
  if (mentionsNestedIsolation(command, denials)) {
    lines.push(
      '  - This looks like a nested container / namespace launch. Nested isolation is blocked unless ' +
        '`sandbox.enableWeakerNestedSandbox` is enabled — this failure is expected, not a broken install.',
    )
  }
  lines.push(
    '  - If the path or host genuinely must be reachable, tell the user which one and let them allow it via /sandbox.',
  )
  if (allowUnsandboxedCommands) {
    lines.push(
      '  - `dangerouslyDisableSandbox: true` is a last resort, not a first response: it re-runs the command with ' +
        'NO isolation and asks the user to give up the protection they turned on. Use it only when the operation ' +
        'genuinely cannot work inside the sandbox, and say which denial made you do it.',
    )
  } else {
    lines.push(
      '  - `dangerouslyDisableSandbox` is disabled by policy in this session. Do not attempt it — adjust the approach ' +
        'or ask the user to change the sandbox settings.',
    )
  }
  lines.push(SANDBOX_VIOLATIONS_CLOSE_TAG)
  return lines.join('\n')
}

function mentionsNestedIsolation(
  command: string,
  denials: readonly SandboxDenial[],
): boolean {
  const haystacks = [command.toLowerCase(), ...denials.map(d => d.line.toLowerCase())]
  return NESTED_ISOLATION_TOKENS.some(token =>
    haystacks.some(haystack => haystack.includes(token)),
  )
}
