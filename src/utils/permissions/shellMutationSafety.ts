import type { ToolPermissionContext } from '../../Tool.js'
import type { PermissionResult } from './PermissionResult.js'
import { validatePath } from './pathValidation.js'

export type ShellMutationCommand = {
  argv: readonly string[]
  /**
   * Bash commands and native PowerShell applications may have filesystem
   * semantics that are not represented in the built-in path catalog. Cmdlets
   * are handled by PowerShellTool's parameter-aware validator, so callers can
   * disable the conservative unknown-application scan for those.
   */
  inspectUnknownArguments?: boolean
}

const HELP_OR_VERSION = new Set(['-h', '--help', '-v', '-V', '--version'])

const SAFE_ARGUMENT_COMMANDS = new Set([
  ':',
  'cat',
  'column',
  'cut',
  'diff',
  'echo',
  'false',
  'file',
  'grep',
  'head',
  'hexdump',
  'jq',
  'ls',
  'md5sum',
  'nl',
  'od',
  'paste',
  'printf',
  'pwd',
  'sha1sum',
  'sha256sum',
  'stat',
  'strings',
  'tail',
  'tr',
  'true',
  'uniq',
  'wc',
  // PowerShell output-only cmdlets. Their arguments are values, not paths.
  'write-host',
  'write-output',
])

const UNINSPECTABLE_EXECUTION_COMMANDS = new Set([
  'bash',
  'busybox',
  'builtin',
  'cmd',
  'command',
  'csh',
  'dash',
  'exec',
  'fish',
  'ksh',
  'powershell',
  'pwsh',
  'sh',
  'tcsh',
  'toybox',
  'zsh',
])

const SENSITIVE_INTERPRETERS = new Set([
  'bun',
  'deno',
  'lua',
  'luajit',
  'node',
  'nodejs',
  'osascript',
  'perl',
  'php',
  'pypy',
  'pypy3',
  'pythonw',
  'ruby',
])

const FAIL_CLOSED_WHEN_UNPARSED = new Set([
  ...UNINSPECTABLE_EXECUTION_COMMANDS,
  ...SENSITIVE_INTERPRETERS,
  '7z',
  '7za',
  'awk',
  'bsdtar',
  'curl',
  'dd',
  'gawk',
  'git',
  'install',
  'ln',
  'mawk',
  'nawk',
  'rg',
  'ripgrep',
  'rsync',
  'scp',
  'sed',
  'sort',
  'tar',
  'tee',
  'truncate',
  'unzip',
  'wget',
])

const KNOWN_GIT_SUBCOMMANDS = new Set([
  'add',
  'am',
  'annotate',
  'apply',
  'archive',
  'bisect',
  'blame',
  'branch',
  'bundle',
  'cat-file',
  'checkout',
  'checkout-index',
  'cherry',
  'cherry-pick',
  'clean',
  'clone',
  'column',
  'commit',
  'commit-tree',
  'config',
  'count-objects',
  'credential',
  'credential-cache',
  'credential-store',
  'describe',
  'diff',
  'diff-files',
  'diff-index',
  'diff-tree',
  'difftool',
  'fetch',
  'fetch-pack',
  'filter-branch',
  'for-each-ref',
  'format-patch',
  'fsck',
  'gc',
  'get-tar-commit-id',
  'grep',
  'hash-object',
  'help',
  'index-pack',
  'init',
  'interpret-trailers',
  'log',
  'ls-files',
  'ls-remote',
  'ls-tree',
  'maintenance',
  'merge',
  'merge-base',
  'merge-file',
  'merge-index',
  'merge-ours',
  'merge-recursive',
  'merge-tree',
  'mktag',
  'mktree',
  'multi-pack-index',
  'mv',
  'name-rev',
  'notes',
  'pack-objects',
  'pack-redundant',
  'pack-refs',
  'patch-id',
  'prune',
  'prune-packed',
  'pull',
  'push',
  'range-diff',
  'read-tree',
  'rebase',
  'reflog',
  'remote',
  'remote-ext',
  'remote-fd',
  'repack',
  'replace',
  'request-pull',
  'rerere',
  'reset',
  'restore',
  'rev-list',
  'rev-parse',
  'revert',
  'rm',
  'send-email',
  'shortlog',
  'show',
  'show-branch',
  'show-index',
  'show-ref',
  'sparse-checkout',
  'status',
  'stripspace',
  'submodule',
  'switch',
  'symbolic-ref',
  'tag',
  'unpack-file',
  'unpack-objects',
  'update-index',
  'update-ref',
  'update-server-info',
  'upload-archive',
  'upload-pack',
  'var',
  'verify-commit',
  'verify-pack',
  'verify-tag',
  'version',
  'whatchanged',
  'worktree',
  'write-tree',
])

function commandBasename(raw: string): string {
  const basename = raw.split(/[\\/]/).at(-1)?.toLowerCase() ?? ''
  return basename.endsWith('.exe') ? basename.slice(0, -4) : basename
}

function passthrough(message = 'No protected shell mutation detected'): PermissionResult {
  return { behavior: 'passthrough', message }
}

function safetyAsk(command: string, reason: string): PermissionResult {
  const message = `${command} requires explicit approval because ${reason}`
  return {
    behavior: 'ask',
    message,
    decisionReason: {
      type: 'safetyCheck',
      reason: message,
      classifierApprovable: false,
    },
    suggestions: [],
  }
}

function validateMutationTarget(
  operation: string,
  rawPath: string,
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  // Once a caller has identified an output operand it is always a local
  // pathname. A spelling containing `://` is only URL-like at an unknown
  // input boundary; treating it as a URL here would let a protected custom
  // config alias bypass the identity check.
  if (rawPath === '-') return null

  const result = validatePath(rawPath, cwd, context, 'create')
  if (result.allowed) return null

  if (result.decisionReason?.type === 'rule') {
    return {
      behavior: 'deny',
      message: `${operation} to '${result.resolvedPath}' was blocked by a deny rule.`,
      decisionReason: result.decisionReason,
    }
  }

  if (result.decisionReason?.type === 'safetyCheck') {
    return {
      behavior: 'ask',
      message: result.decisionReason.reason,
      blockedPath: result.resolvedPath,
      decisionReason: result.decisionReason,
      suggestions: [],
    }
  }

  // A dynamic/glob/provider target cannot be compared with the selected
  // config-root identity. Known mutators must fail closed rather than letting
  // bypassPermissions turn an unresolved write into an allow.
  if (result.decisionReason?.type === 'other') {
    return safetyAsk(operation, result.decisionReason.reason)
  }

  // Ordinary paths outside acceptEdits are intentionally not promoted to a
  // bypass-immune prompt here. The shell permission mode owns that decision;
  // this floor only protects sensitive identities and uninspectable targets.
  return null
}

function looksLikeUrl(value: string): boolean {
  return value.includes('://')
}

function looksLikeRemoteSpec(value: string): boolean {
  if (value.startsWith('git@')) return true
  const colon = value.indexOf(':')
  if (colon < 0) return false
  const prefix = value.slice(0, colon)
  const isWindowsDrive = /^[A-Za-z]$/.test(prefix)
  return !isWindowsDrive && !prefix.includes('/') && !prefix.includes('\\')
}

function isHelpOrVersionOnly(args: readonly string[]): boolean {
  return args.length === 1 && HELP_OR_VERSION.has(args[0]!)
}

function unknownOption(command: string, detail = 'its option grammar was not recognized') {
  return safetyAsk(command, detail)
}

function checkTee(
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  let pastDoubleDash = false
  for (const arg of args) {
    if (arg === '--') {
      pastDoubleDash = true
      continue
    }
    if (!pastDoubleDash && arg.startsWith('-') && arg !== '-') {
      if (
        ['-a', '--append', '-i', '--ignore-interrupts', '-p'].includes(arg) ||
        arg.startsWith('--output-error')
      ) {
        continue
      }
      return unknownOption('tee')
    }
    const result = validateMutationTarget('tee output', arg, cwd, context)
    if (result) return result
  }
  return null
}

function checkTransfer(
  command: 'rsync' | 'scp',
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  const positionals: string[] = []
  let index = 0
  let pastDoubleDash = false
  while (index < args.length) {
    const arg = args[index]!
    if (arg === '--') {
      pastDoubleDash = true
      index++
      continue
    }
    if (!pastDoubleDash && command === 'rsync') {
      const safeBooleanLong = new Set([
        '--archive',
        '--recursive',
        '--verbose',
        '--quiet',
        '--compress',
        '--dry-run',
        '--checksum',
        '--update',
        '--existing',
        '--ignore-existing',
        '--partial',
        '--progress',
        '--human-readable',
        '--itemize-changes',
        '--stats',
        '--whole-file',
        '--sparse',
      ])
      const safeValueLong = new Set([
        '--timeout',
        '--contimeout',
        '--bwlimit',
        '--max-size',
        '--min-size',
        '--chmod',
        '--exclude',
        '--include',
        '--port',
      ])
      if (safeBooleanLong.has(arg)) {
        index++
        continue
      }
      if (safeValueLong.has(arg)) {
        if (args[index + 1] === undefined) return unknownOption(command, 'an option value is missing')
        index += 2
        continue
      }
      if ([...safeValueLong].some(option => arg.startsWith(`${option}=`))) {
        index++
        continue
      }
      if (/^-[arvqzncuihP]+$/.test(arg)) {
        index++
        continue
      }
    }
    if (!pastDoubleDash && command === 'scp') {
      if (arg === '-P' || arg === '-l') {
        if (args[index + 1] === undefined) return unknownOption(command, 'an option value is missing')
        index += 2
        continue
      }
      if (/^-[346BCpqrTv]+$/.test(arg)) {
        index++
        continue
      }
    }
    if (!pastDoubleDash && arg.startsWith('-')) {
      return unknownOption(
        command,
        'its option grammar is outside the closed safe allowlist and may hide local paths or delegated execution',
      )
    }
    positionals.push(arg)
    index++
  }

  if (positionals.length < 2) {
    return safetyAsk(command, 'a source and destination could not both be identified')
  }
  const destination = positionals.at(-1)!
  if (looksLikeRemoteSpec(destination)) return null
  return validateMutationTarget(`${command} destination`, destination, cwd, context)
}

function checkTruncate(
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  let index = 0
  let pastDoubleDash = false
  while (index < args.length) {
    const arg = args[index]!
    if (arg === '--') {
      pastDoubleDash = true
      index++
      continue
    }
    if (!pastDoubleDash && ['-s', '--size', '-r', '--reference'].includes(arg)) {
      if (args[index + 1] === undefined) return unknownOption('truncate', 'an option value is missing')
      index += 2
      continue
    }
    if (
      !pastDoubleDash &&
      (arg.startsWith('--size=') ||
        arg.startsWith('--reference=') ||
        ['-c', '--no-create', '-o', '--io-blocks'].includes(arg))
    ) {
      index++
      continue
    }
    if (!pastDoubleDash && arg.startsWith('-')) return unknownOption('truncate')
    const result = validateMutationTarget('truncate target', arg, cwd, context)
    if (result) return result
    index++
  }
  return null
}

function checkLink(
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  const positionals: string[] = []
  let targetDirectory: string | undefined
  let symbolic = false
  let index = 0
  let pastDoubleDash = false
  while (index < args.length) {
    const arg = args[index]!
    if (arg === '--') {
      pastDoubleDash = true
      index++
      continue
    }
    if (!pastDoubleDash && (arg === '-t' || arg === '--target-directory')) {
      targetDirectory = args[index + 1]
      if (!targetDirectory) return unknownOption('ln', 'its target directory is missing')
      index += 2
      continue
    }
    if (!pastDoubleDash && arg.startsWith('--target-directory=')) {
      targetDirectory = arg.slice('--target-directory='.length)
      if (!targetDirectory) return unknownOption('ln', 'its target directory is missing')
      index++
      continue
    }
    if (
      !pastDoubleDash &&
      (/^-[sfnvTr]+$/.test(arg) ||
        [
          '--symbolic',
          '--force',
          '--no-dereference',
          '--verbose',
          '--no-target-directory',
          '--relative',
        ].includes(arg))
    ) {
      symbolic ||= arg === '--symbolic' || arg.includes('s')
      index++
      continue
    }
    if (!pastDoubleDash && arg.startsWith('-')) return unknownOption('ln')
    positionals.push(arg)
    index++
  }
  if (!symbolic) {
    return safetyAsk(
      'ln',
      'hard links can expose a protected config file through an otherwise safe inode alias',
    )
  }
  if (positionals.length === 0) {
    return safetyAsk('ln', 'a symbolic-link source could not be identified')
  }
  const sources = targetDirectory
    ? positionals
    : positionals.length === 1
      ? positionals
      : positionals.slice(0, -1)
  for (const source of sources) {
    const result = validateMutationTarget(
      'ln symbolic source',
      source,
      cwd,
      context,
    )
    if (result) return result
  }
  const target = targetDirectory ?? (positionals.length <= 1 ? '.' : positionals.at(-1)!)
  return validateMutationTarget('ln target', target, cwd, context)
}

function checkTar(args: readonly string[]): PermissionResult | null {
  const safeBooleanLong = new Set([
    '--list',
    '--verbose',
    '--gzip',
    '--gunzip',
    '--ungzip',
    '--bzip2',
    '--xz',
    '--lzip',
    '--lzma',
    '--lzop',
    '--zstd',
    '--auto-compress',
    '--wildcards',
    '--no-wildcards',
    '--anchored',
    '--no-anchored',
    '--ignore-case',
    '--no-ignore-case',
    '--wildcards-match-slash',
    '--no-wildcards-match-slash',
    '--verbatim-files-from',
    '--null',
  ])
  const safeValueLong = new Set(['--file', '--exclude'])
  let sawList = false
  let index = 0
  while (index < args.length) {
    const arg = args[index]!
    if (arg === '--') {
      index++
      continue
    }
    if (safeBooleanLong.has(arg)) {
      sawList ||= arg === '--list'
      index++
      continue
    }
    if (safeValueLong.has(arg)) {
      if (args[index + 1] === undefined) return unknownOption('tar', 'a list option value is missing')
      index += 2
      continue
    }
    if ([...safeValueLong].some(option => arg.startsWith(`${option}=`))) {
      index++
      continue
    }
    if (/^-[tfvzjJa]+$/.test(arg)) {
      sawList ||= arg.includes('t')
      index++
      continue
    }
    if (arg.startsWith('-')) {
      return safetyAsk(
        'tar',
        'only a closed list-only option set is safe; other flags may write files or execute delegated commands',
      )
    }
    index++
  }
  return sawList
    ? null
    : safetyAsk('tar', 'archive creation or extraction writes inferred members and symlinks')
}

function checkUnzip(args: readonly string[]): PermissionResult | null {
  let sawReadOnlyAction = false
  for (const arg of args) {
    if (arg === '--') continue
    if (arg === '--help') {
      sawReadOnlyAction = true
      continue
    }
    if (/^-[ltpcZvhq]+$/.test(arg)) {
      sawReadOnlyAction ||= /[ltpcZvh]/.test(arg)
      continue
    }
    if (arg.startsWith('-')) {
      return safetyAsk(
        'unzip',
        'only a closed listing, testing, or stdout option set is safe',
      )
    }
  }
  return sawReadOnlyAction
    ? null
    : safetyAsk('unzip', 'archive extraction writes member names not represented in argv')
}

function check7z(args: readonly string[]): PermissionResult | null {
  const action = args.find(arg => !arg.startsWith('-'))?.toLowerCase()
  if (action !== 'l' && action !== 't') {
    return safetyAsk(
      '7z',
      'only list and test actions are proven non-writing; extract, add, update, delete, and rename actions mutate inferred paths',
    )
  }
  // Even list/test actions have switches that can redirect output or invoke
  // external codecs. Keep their grammar deliberately closed.
  for (const arg of args) {
    if (!arg.startsWith('-')) continue
    if (/^-(?:ba|bb[0-3]|bd|bs[opt][0-2]|bsp[0-2]|slt|scrc[A-Za-z0-9-]+|p.*|r-?|x!.+|i!.+)$/.test(arg)) {
      continue
    }
    return safetyAsk('7z', 'an option outside the closed list/test allowlist was present')
  }
  return null
}

function optionValue(
  args: readonly string[],
  index: number,
  shortOption: string,
  longOption: string,
): { value?: string; consumed: number } | null {
  const arg = args[index]!
  if ((shortOption !== '' && arg === shortOption) || arg === longOption) {
    return { value: args[index + 1], consumed: 2 }
  }
  if (arg.startsWith(`${longOption}=`)) {
    return { value: arg.slice(longOption.length + 1), consumed: 1 }
  }
  if (
    shortOption !== '' &&
    arg.startsWith(shortOption) &&
    arg.length > shortOption.length
  ) {
    return { value: arg.slice(shortOption.length), consumed: 1 }
  }
  return null
}

function checkCurl(
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  const longWriteOptions = [
    '--output',
    '--output-dir',
    '--dump-header',
    '--cookie-jar',
    '--trace',
    '--trace-ascii',
    '--stderr',
    '--etag-save',
    '--alt-svc',
    '--hsts',
    '--libcurl',
  ] as const
  const shortWriteOptions = new Map([
    ['-o', '--output'],
    ['-D', '--dump-header'],
    ['-c', '--cookie-jar'],
  ])
  let index = 0
  while (index < args.length) {
    const arg = args[index]!
    if (arg.startsWith('--expand-')) {
      return safetyAsk(
        'curl',
        'expand options evaluate a variable DSL that can hide output paths or file reads',
      )
    }
    if (
      arg === '-K' ||
      arg === '--config' ||
      arg.startsWith('-K') ||
      arg.startsWith('--config=')
    ) {
      return safetyAsk('curl', 'a config file can hide local output directives')
    }
    if (
      arg === '-O' ||
      [
        '--remote-name',
        '--remote-name-all',
        '--remote-name-no-overwrite',
        '--remote-header-name',
      ].includes(arg) ||
      /^-[^-]*O/.test(arg)
    ) {
      return safetyAsk('curl', 'the local output filename is inferred from remote metadata')
    }
    if (arg === '-w' || arg === '--write-out' || arg.startsWith('--write-out=')) {
      const format = arg.includes('=') ? arg.slice(arg.indexOf('=') + 1) : args[index + 1]
      if (format === undefined) return safetyAsk('curl', 'a write-out format is missing')
      if (format.includes('%output{')) {
        return safetyAsk('curl', 'the write-out DSL can redirect to a hidden local filename')
      }
      index += arg.includes('=') ? 1 : 2
      continue
    }
    if (arg.startsWith('-') && !arg.startsWith('--') && arg.length > 2) {
      const cluster = arg.slice(1)
      if (cluster.includes('K')) {
        return safetyAsk('curl', 'a config file can hide local output directives')
      }
      if (cluster.includes('O')) {
        return safetyAsk('curl', 'the local output filename is inferred from remote metadata')
      }
      const writeOffset = [...cluster].findIndex(char =>
        ['o', 'D', 'c'].includes(char),
      )
      const writeOutOffset = cluster.indexOf('w')
      if (writeOutOffset >= 0 && (writeOffset < 0 || writeOutOffset < writeOffset)) {
        const attached = cluster.slice(writeOutOffset + 1)
        const format = attached || args[index + 1]
        if (!format) return safetyAsk('curl', 'a write-out format is missing')
        if (format.includes('%output{')) {
          return safetyAsk('curl', 'the write-out DSL can redirect to a hidden local filename')
        }
        index += attached ? 1 : 2
        continue
      }
      if (writeOffset >= 0) {
        const attached = cluster.slice(writeOffset + 1)
        const path = attached || args[index + 1]
        if (!path) return safetyAsk('curl', 'an output path is missing')
        const result = validateMutationTarget('curl output', path, cwd, context)
        if (result) return result
        index += attached ? 1 : 2
        continue
      }
      if (writeOutOffset >= 0) {
        const attached = cluster.slice(writeOutOffset + 1)
        const format = attached || args[index + 1]
        if (!format) return safetyAsk('curl', 'a write-out format is missing')
        if (format.includes('%output{')) {
          return safetyAsk('curl', 'the write-out DSL can redirect to a hidden local filename')
        }
        index += attached ? 1 : 2
        continue
      }
    }
    let handled = false
    for (const longOption of longWriteOptions) {
      const shortOption = [...shortWriteOptions].find(([, long]) => long === longOption)?.[0] ?? ''
      const extracted = optionValue(args, index, shortOption, longOption)
      if (!extracted) continue
      if (!extracted.value) return safetyAsk('curl', 'an output path is missing')
      const result = validateMutationTarget('curl output', extracted.value, cwd, context)
      if (result) return result
      index += extracted.consumed
      handled = true
      break
    }
    if (handled) continue
    index++
  }
  return null
}

function checkWget(
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  const writeOptions = new Map([
    ['-O', '--output-document'],
    ['-o', '--output-file'],
    ['-a', '--append-output'],
    ['', '--save-cookies'],
    ['', '--warc-file'],
    ['', '--warc-cdx'],
    ['', '--warc-tempdir'],
    ['-P', '--directory-prefix'],
  ])
  const spider = args.includes('--spider')
  let index = 0
  while (index < args.length) {
    const arg = args[index]!
    if (arg === '-b' || arg === '--background') {
      return safetyAsk('wget', 'background mode escapes the inspected command lifecycle and creates an inferred log')
    }
    if (arg === '--use-askpass' || arg.startsWith('--use-askpass=')) {
      return safetyAsk('wget', 'the askpass option can delegate to an executable')
    }
    if (
      arg === '-e' ||
      arg === '--execute' ||
      arg === '--config' ||
      arg.startsWith('-e') ||
      arg.startsWith('--execute=') ||
      arg.startsWith('--config=')
    ) {
      return safetyAsk('wget', 'configuration directives can hide local output or delegated behavior')
    }
    if (
      ['--content-disposition', '--trust-server-names', '--backup-converted'].includes(arg)
    ) {
      return safetyAsk('wget', 'the local output filename or a backup filename is inferred')
    }
    if (arg.startsWith('-') && !arg.startsWith('--') && arg.length > 2) {
      const cluster = arg.slice(1)
      if (cluster.includes('e')) {
        return safetyAsk('wget', 'configuration directives can hide local output or delegated behavior')
      }
      const writeOffset = [...cluster].findIndex(char =>
        ['O', 'o', 'a', 'P'].includes(char),
      )
      if (writeOffset >= 0) {
        if (spider) return safetyAsk('wget', 'spider mode is only safe without local output options')
        const attached = cluster.slice(writeOffset + 1)
        const path = attached || args[index + 1]
        if (!path) return safetyAsk('wget', 'an output path is missing')
        const result = validateMutationTarget('wget output', path, cwd, context)
        if (result) return result
        index += attached ? 1 : 2
        continue
      }
    }
    let handled = false
    for (const [shortOption, longOption] of writeOptions) {
      const extracted = optionValue(args, index, shortOption, longOption)
      if (!extracted) continue
      if (!extracted.value) return safetyAsk('wget', 'an output path is missing')
      if (spider) {
        return safetyAsk('wget', 'spider mode is only safe without local output options')
      }
      const result = validateMutationTarget('wget output', extracted.value, cwd, context)
      if (result) return result
      index += extracted.consumed
      handled = true
      break
    }
    if (handled) continue
    index++
  }
  if (spider) return null
  return safetyAsk('wget', 'the local output filename is inferred from the URL')
}

function checkAwk(command: string, args: readonly string[]): PermissionResult | null {
  if (
    args.some(
      arg =>
        ['-f', '--file', '-l', '--load'].includes(arg) ||
        arg.startsWith('--file=') ||
        arg.startsWith('--load=') ||
        (/^-[fl].+/.test(arg) && arg.length > 2),
    )
  ) {
    return safetyAsk(command, 'an external DSL program or extension may write files or execute commands')
  }
  let index = 0
  while (index < args.length) {
    const arg = args[index]!
    if (['-e', '--source', '-E', '--exec'].includes(arg) || arg.startsWith('--source=') || arg.startsWith('--exec=')) {
      return safetyAsk(command, 'a program supplied through an option is outside the proven pure DSL form')
    }
    if (['-F', '-v', '--field-separator', '--assign'].includes(arg)) {
      if (args[index + 1] === undefined) return safetyAsk(command, 'a recognized option value is missing')
      index += 2
      continue
    }
    if (/^-F.+/.test(arg) || /^-v.+/.test(arg) || arg.startsWith('--field-separator=') || arg.startsWith('--assign=')) {
      index++
      continue
    }
    if (arg.startsWith('-')) return unknownOption(command, 'its option grammar is outside the closed safe allowlist')
    break
  }
  const program = args[index]
  if (!program) return null
  const lower = program.toLowerCase()
  const printRedirect = lower.indexOf('print') >= 0 && lower.slice(lower.indexOf('print')).includes('>')
  const printfRedirect = lower.indexOf('printf') >= 0 && lower.slice(lower.indexOf('printf')).includes('>')
  if (
    lower.includes('system(') ||
    lower.includes('getline') ||
    lower.includes('|') ||
    lower.includes('@load') ||
    printRedirect ||
    printfRedirect
  ) {
    return safetyAsk(command, 'its DSL can redirect output, load code, or execute a nested command')
  }
  return null
}

function sedProgramIsProvenSafe(program: string): boolean {
  const script = program.trim()
  if (/^(?:p|P|d|D|q|Q|n|N|=)$/.test(script)) return true
  if (!script.startsWith('s') || script.length < 3) return false
  const delimiter = script[1]!
  if (/^[A-Za-z0-9\s\\]$/.test(delimiter)) return false
  let escaped = false
  let delimiters = 0
  let flagsStart = script.length
  for (let index = 2; index < script.length; index++) {
    const char = script[index]!
    if (escaped) {
      escaped = false
      continue
    }
    if (char === '\\') {
      escaped = true
      continue
    }
    if (char === delimiter) {
      delimiters++
      if (delimiters === 2) {
        flagsStart = index + 1
        break
      }
    }
  }
  return delimiters === 2 && /^[0-9gpiImM]*$/.test(script.slice(flagsStart).trim())
}

function checkSed(
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  const scripts: string[] = []
  const positionals: string[] = []
  let inPlace = false
  let index = 0
  let pastDoubleDash = false
  while (index < args.length) {
    const arg = args[index]!
    if (arg === '--') {
      pastDoubleDash = true
      index++
      continue
    }
    if (!pastDoubleDash && (arg === '-f' || arg === '--file' || arg.startsWith('--file=') || /^-f.+/.test(arg))) {
      return safetyAsk('sed', 'an external script can hide write or command-execution directives')
    }
    if (!pastDoubleDash && (arg === '-e' || arg === '--expression')) {
      const script = args[index + 1]
      if (!script) return safetyAsk('sed', 'an expression value is missing')
      scripts.push(script)
      index += 2
      continue
    }
    if (!pastDoubleDash && arg.startsWith('--expression=')) {
      scripts.push(arg.slice('--expression='.length))
      index++
      continue
    }
    if (!pastDoubleDash && /^-e.+/.test(arg)) {
      scripts.push(arg.slice(2))
      index++
      continue
    }
    if (
      !pastDoubleDash &&
      (arg.startsWith('-i') || arg.startsWith('--in-place'))
    ) {
      inPlace = true
      index++
      continue
    }
    if (!pastDoubleDash && ['-n', '--quiet', '--silent', '-E', '-r', '--regexp-extended'].includes(arg)) {
      index++
      continue
    }
    if (!pastDoubleDash && arg.startsWith('-')) return unknownOption('sed')
    positionals.push(arg)
    index++
  }
  if (scripts.length === 0 && positionals.length > 0) scripts.push(positionals.shift()!)
  if (scripts.some(script => !sedProgramIsProvenSafe(script))) {
    return safetyAsk('sed', 'the DSL is not in the proven non-writing, non-executing subset')
  }
  if (!inPlace) return null
  if (positionals.length === 0) {
    return safetyAsk('sed', 'in-place editing has no statically identified file operand')
  }
  for (const file of positionals) {
    const result = validateMutationTarget('sed in-place target', file, cwd, context)
    if (result) return result
  }
  return null
}

function checkSort(
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  let index = 0
  while (index < args.length) {
    const arg = args[index]!
    if (
      arg === '--compress-program' ||
      arg.startsWith('--compress-program=')
    ) {
      return safetyAsk('sort', 'the compression option delegates to an executable')
    }
    const output = optionValue(args, index, '-o', '--output')
    if (output) {
      if (!output.value) return safetyAsk('sort', 'an output path is missing')
      const result = validateMutationTarget('sort output', output.value, cwd, context)
      if (result) return result
      index += output.consumed
      continue
    }
    const temporaryDirectory = optionValue(
      args,
      index,
      '-T',
      '--temporary-directory',
    )
    if (temporaryDirectory) {
      if (!temporaryDirectory.value) {
        return safetyAsk('sort', 'a temporary output directory is missing')
      }
      const result = validateMutationTarget(
        'sort temporary directory',
        temporaryDirectory.value,
        cwd,
        context,
      )
      if (result) return result
      index += temporaryDirectory.consumed
      continue
    }
    index++
  }
  return null
}

function gitSubcommand(args: readonly string[]): { name: string; args: readonly string[] } | null {
  let index = 0
  while (index < args.length) {
    const arg = args[index]!
    if (['-C', '-c', '--git-dir', '--work-tree', '--namespace'].includes(arg)) {
      index += 2
      continue
    }
    if (arg.startsWith('--git-dir=') || arg.startsWith('--work-tree=') || arg.startsWith('--namespace=')) {
      index++
      continue
    }
    if (arg.startsWith('-')) {
      index++
      continue
    }
    return { name: arg, args: args.slice(index + 1) }
  }
  return null
}

function forbiddenGitGlobalOverride(args: readonly string[]): string | null {
  for (const arg of args) {
    if (
      ['-C', '-c', '--config-env', '--exec-path', '--git-dir', '--work-tree', '--namespace'].includes(arg) ||
      (/^-c.+/.test(arg) && arg.length > 2) ||
      arg.startsWith('--config-env=') ||
      arg.startsWith('--exec-path=') ||
      arg.startsWith('--git-dir=') ||
      arg.startsWith('--work-tree=') ||
      arg.startsWith('--namespace=')
    ) {
      return arg
    }
    if (!arg.startsWith('-')) break
  }
  return null
}

function checkGitClone(
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  const valueFlags = new Set([
    '-b', '--branch', '-o', '--origin', '--depth', '--filter', '--reference',
    '--reference-if-able', '--separate-git-dir', '-c', '--config', '-j', '--jobs',
    '-u', '--upload-pack', '--template',
  ])
  const positionals: string[] = []
  let index = 0
  let pastDoubleDash = false
  while (index < args.length) {
    const arg = args[index]!
    if (arg === '--') {
      pastDoubleDash = true
      index++
      continue
    }
    if (!pastDoubleDash && valueFlags.has(arg)) {
      const value = args[index + 1]
      if (!value) return safetyAsk('git clone', 'an option value is missing')
      if (['-c', '--config', '-u', '--upload-pack', '--template'].includes(arg)) {
        return safetyAsk('git clone', 'clone-time config, template, or upload commands can delegate execution')
      }
      if (arg === '--separate-git-dir') {
        const result = validateMutationTarget('git separate directory', value, cwd, context)
        if (result) return result
      }
      index += 2
      continue
    }
    if (!pastDoubleDash && arg.startsWith('--separate-git-dir=')) {
      const result = validateMutationTarget('git separate directory', arg.slice(arg.indexOf('=') + 1), cwd, context)
      if (result) return result
      index++
      continue
    }
    if (!pastDoubleDash && (arg.startsWith('--config=') || arg.startsWith('--upload-pack=') || arg.startsWith('--template=') || /^-[cu].+/.test(arg))) {
      return safetyAsk('git clone', 'clone-time config, template, or upload commands can delegate execution')
    }
    if (!pastDoubleDash && arg.startsWith('-')) {
      index++
      continue
    }
    positionals.push(arg)
    index++
  }
  if (positionals.length === 0) return null
  if (positionals.length > 2) return safetyAsk('git clone', 'more than one destination-like operand was present')
  const destination = positionals.length === 1 ? '.' : positionals[1]!
  return validateMutationTarget('git clone destination', destination, cwd, context)
}

function checkGitConfig(args: readonly string[]): PermissionResult | null {
  const valueFlags = new Set(['--file', '--blob', '--type', '--default', '--comment', '--expiry-date'])
  const writeFlag = args.some(arg =>
    ['--add', '--replace-all', '--unset', '--unset-all', '--rename-section', '--remove-section', '--edit'].includes(arg),
  )
  let positionals = 0
  let index = 0
  while (index < args.length) {
    const arg = args[index]!
    if (valueFlags.has(arg)) {
      index += 2
      continue
    }
    if (!arg.startsWith('-')) positionals++
    index++
  }
  return writeFlag || positionals >= 2
    ? safetyAsk('git config', 'configuration writes can change hooks, filters, credentials, or executable policy')
    : null
}

const GIT_SUBCOMMANDS_WITH_INFERRED_WRITES = new Set([
  'add',
  'am',
  'apply',
  'bisect',
  'branch',
  'checkout',
  'checkout-index',
  'cherry-pick',
  'clean',
  'commit',
  'commit-tree',
  'fast-import',
  'fetch',
  'format-patch',
  'gc',
  'hash-object',
  'index-pack',
  'init',
  'merge',
  'merge-file',
  'merge-index',
  'mktag',
  'mktree',
  'mv',
  'notes',
  'pack-objects',
  'prune',
  'pull',
  'rebase',
  'reflog',
  'remote',
  'repack',
  'replace',
  'reset',
  'restore',
  'revert',
  'rm',
  'stash',
  'submodule',
  'switch',
  'tag',
  'unpack-objects',
  'update-index',
  'update-ref',
  'update-server-info',
  'worktree',
])

const GIT_VALUE_ONLY_OPTIONS = new Set([
  '-F',
  '-m',
  '--author',
  '--cleanup',
  '--date',
  '--file',
  '--format',
  '--grep',
  '--message',
  '--pretty',
  '--subject-prefix',
])

function checkGitExplicitOutputs(
  subcommand: string,
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  let index = 0
  while (index < args.length) {
    const arg = args[index]!
    const output = optionValue(args, index, '-o', '--output')
    if (output) {
      if (!output.value) return safetyAsk(`git ${subcommand}`, 'an output path is missing')
      const result = validateMutationTarget(
        `git ${subcommand} output`,
        output.value,
        cwd,
        context,
      )
      if (result) return result
      index += output.consumed
      continue
    }
    if (
      subcommand === 'format-patch' &&
      (arg === '--output-directory' || arg.startsWith('--output-directory='))
    ) {
      const value = arg.includes('=')
        ? arg.slice(arg.indexOf('=') + 1)
        : args[index + 1]
      if (!value) return safetyAsk('git format-patch', 'an output directory is missing')
      const result = validateMutationTarget(
        'git format-patch output directory',
        value,
        cwd,
        context,
      )
      if (result) return result
      index += arg.includes('=') ? 1 : 2
      continue
    }
    index++
  }

  if (subcommand === 'bundle' && args.includes('create')) {
    const createIndex = args.indexOf('create')
    const destination = args
      .slice(createIndex + 1)
      .find(arg => !arg.startsWith('-'))
    return destination
      ? validateMutationTarget('git bundle output', destination, cwd, context)
      : safetyAsk('git bundle', 'a create output path is missing')
  }

  return null
}

function checkGitSensitiveCandidates(
  subcommand: string,
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  if (!GIT_SUBCOMMANDS_WITH_INFERRED_WRITES.has(subcommand)) return null

  const skipped = new Set<number>()
  for (let index = 0; index < args.length; index++) {
    const arg = args[index]!
    if (GIT_VALUE_ONLY_OPTIONS.has(arg)) {
      skipped.add(index)
      skipped.add(index + 1)
      index++
      continue
    }
    if (
      [...GIT_VALUE_ONLY_OPTIONS].some(
        option =>
          (option.startsWith('--') && arg.startsWith(`${option}=`)) ||
          (option.length === 2 && arg.startsWith(option) && arg.length > 2),
      )
    ) {
      skipped.add(index)
    }
  }

  for (let index = 0; index < args.length; index++) {
    if (skipped.has(index)) continue
    const candidate = candidateFromUnknownArg(args[index]!)
    if (!candidate) continue
    const result = validateMutationTarget(
      `git ${subcommand} argument`,
      candidate,
      cwd,
      context,
    )
    if (result) return result
  }

  // Inferred repository/worktree writes are safe only when the current
  // identity itself is outside the protected config root.
  return validateMutationTarget(
    `git ${subcommand} inferred destination`,
    '.',
    cwd,
    context,
  )
}

function checkGit(
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  const override = forbiddenGitGlobalOverride(args)
  if (override) {
    return safetyAsk('git', `global option '${override}' can change repositories, hooks, aliases, or executable paths`)
  }
  const subcommand = gitSubcommand(args)
  if (!subcommand) return null
  if (!KNOWN_GIT_SUBCOMMANDS.has(subcommand.name)) {
    return safetyAsk('git', 'an unknown subcommand may be an executable alias or external git helper')
  }
  if (
    ['difftool', 'credential', 'credential-cache', 'credential-store', 'filter-branch'].includes(subcommand.name) ||
    (subcommand.name === 'bisect' && subcommand.args.includes('run')) ||
    (subcommand.name === 'submodule' && subcommand.args.includes('foreach'))
  ) {
    return safetyAsk('git', 'the selected subcommand can delegate to a local tool or shell fragment')
  }
  if (
    subcommand.args.some(arg =>
      ['--ext-diff', '--textconv', '--open-files-in-pager', '--exec', '-x', '--upload-pack', '--receive-pack'].includes(arg) ||
      arg.startsWith('--open-files-in-pager=') ||
      arg.startsWith('--exec=') ||
      arg.startsWith('--upload-pack=') ||
      arg.startsWith('--receive-pack=') ||
      (/^-x.+/.test(arg) && arg.length > 2),
    )
  ) {
    return safetyAsk('git', 'the selected option can launch an external tool or transport helper')
  }
  if (subcommand.name === 'clone') return checkGitClone(subcommand.args, cwd, context)
  if (subcommand.name === 'config') return checkGitConfig(subcommand.args)
  const explicitOutput = checkGitExplicitOutputs(
    subcommand.name,
    subcommand.args,
    cwd,
    context,
  )
  if (explicitOutput) return explicitOutput
  return checkGitSensitiveCandidates(
    subcommand.name,
    subcommand.args,
    cwd,
    context,
  )
}

function checkKnownCommand(
  command: string,
  args: readonly string[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null | undefined {
  if (isHelpOrVersionOnly(args)) return null
  if (UNINSPECTABLE_EXECUTION_COMMANDS.has(command)) {
    return safetyAsk(command, 'it delegates execution through syntax this path floor cannot inspect')
  }
  if (
    SENSITIVE_INTERPRETERS.has(command) ||
    /^python(?:\d+(?:\.\d+)*)?$/.test(command)
  ) {
    return safetyAsk(command, 'a general-purpose interpreter can mutate protected paths without exposing them in argv')
  }
  switch (command) {
    case 'tee':
      return checkTee(args, cwd, context)
    case 'install':
      return safetyAsk('install', 'it has multiple target modes and ownership or permission side effects')
    case 'rsync':
    case 'scp':
      return checkTransfer(command, args, cwd, context)
    case 'dd': {
      const outputIndex = args.findIndex(arg => arg === 'of' || arg === 'of=' || arg.startsWith('of='))
      if (outputIndex < 0) return null
      const raw = args[outputIndex]!
      const path = raw === 'of' || raw === 'of=' ? args[outputIndex + 1] : raw.slice(3)
      return path
        ? validateMutationTarget('dd output', path, cwd, context)
        : safetyAsk('dd', 'its output path is missing')
    }
    case 'truncate':
      return checkTruncate(args, cwd, context)
    case 'ln':
      return checkLink(args, cwd, context)
    case 'tar':
    case 'bsdtar':
      return checkTar(args)
    case 'unzip':
      return checkUnzip(args)
    case '7z':
    case '7za':
      return check7z(args)
    case 'curl':
      return checkCurl(args, cwd, context)
    case 'wget':
      return checkWget(args, cwd, context)
    case 'awk':
    case 'gawk':
    case 'mawk':
    case 'nawk':
      return checkAwk(command, args)
    case 'sed':
      return checkSed(args, cwd, context)
    case 'sort':
      return checkSort(args, cwd, context)
    case 'git':
      return checkGit(args, cwd, context)
    case 'rg':
    case 'ripgrep':
      return args.some(arg =>
        arg === '--pre' ||
        arg.startsWith('--pre=') ||
        arg === '--hostname-bin' ||
        arg.startsWith('--hostname-bin='),
      )
        ? safetyAsk(command, 'the selected option delegates to an external executable')
        : null
    default:
      return undefined
  }
}

function candidateFromUnknownArg(arg: string): string | null {
  if (arg === '-' || looksLikeUrl(arg)) return null
  if (arg.startsWith('-')) {
    const equals = arg.indexOf('=')
    return equals >= 0 && equals + 1 < arg.length ? arg.slice(equals + 1) : null
  }
  return arg
}

function checkOneCommand(
  command: ShellMutationCommand,
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult | null {
  const [rawName, ...args] = command.argv
  if (!rawName) return null
  const name = commandBasename(rawName)
  const knownResult = checkKnownCommand(name, args, cwd, context)
  if (knownResult !== undefined) return knownResult
  if (SAFE_ARGUMENT_COMMANDS.has(name) || command.inspectUnknownArguments === false) {
    return null
  }

  // Unknown native applications are not assumed to mutate every ordinary
  // path. But if one receives a path whose canonical identity is protected,
  // bypass mode cannot safely decide whether it is an input or output, so it
  // must ask. Literal URLs are explicitly excluded.
  for (const arg of args) {
    const candidate = candidateFromUnknownArg(arg)
    if (!candidate) continue
    const result = validateMutationTarget(`${name} argument`, candidate, cwd, context)
    if (result) return result
  }
  return null
}

/**
 * Bypass-immune argv-level floor shared by BashTool and PowerShellTool.
 *
 * It is deliberately narrower than normal write authorization: safe ordinary
 * destinations remain passthrough, while config-root identity matches,
 * explicit deny rules, and command forms whose destination/delegation cannot
 * be proven are returned as deny/safetyCheck decisions. The latter reason type
 * is required so the central full-access permission floor cannot discard the
 * result as an ordinary per-action prompt.
 */
export function checkShellMutationSafetyFloor(
  commands: readonly ShellMutationCommand[],
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult {
  for (const command of commands) {
    const result = checkOneCommand(command, cwd, context)
    if (result) return result
  }
  return passthrough()
}

/**
 * Parser-degraded companion to the argv floor. It never attempts to prove a
 * mutator safe from raw text: known writers/delegators fail closed, and a
 * still-visible protected absolute/relative argument is checked by the same
 * config identity logic. This is intentionally only for an already-unparsed
 * command path, where the normal outcome is a prompt anyway.
 */
export function checkUnparsedShellMutationSafetyFloor(
  commandText: string,
  cwd: string,
  context: ToolPermissionContext,
): PermissionResult {
  const rawTokens = commandText
    .split(/[\s;&|(){}]+/)
    .map(token => token.replace(/^[&.'"`]+|['"`,]+$/g, ''))
    .filter(Boolean)

  for (const token of rawTokens) {
    const name = commandBasename(token)
    if (
      FAIL_CLOSED_WHEN_UNPARSED.has(name) ||
      /^python(?:\d+(?:\.\d+)*)?$/.test(name)
    ) {
      return safetyAsk(
        name,
        'the command could not be parsed well enough to prove its writes or delegated execution safe',
      )
    }
  }

  // A single output-only command treats the remaining text as values. Keep
  // this narrow: once shell control/redirection syntax is present, degraded
  // parsing can no longer prove that a protected-looking token is only data.
  const firstName = commandBasename(rawTokens[0] ?? '')
  if (
    SAFE_ARGUMENT_COMMANDS.has(firstName) &&
    !/[;&|(){}<>\r\n]/.test(commandText)
  ) {
    return passthrough('Output-only command has no mutation syntax')
  }

  for (const token of rawTokens) {
    const candidate = candidateFromUnknownArg(token)
    if (!candidate) continue
    const result = validateMutationTarget(
      'unparsed shell argument',
      candidate,
      cwd,
      context,
    )
    if (result) return result
  }
  return passthrough('No protected mutation visible in unparsed shell text')
}
