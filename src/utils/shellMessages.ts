/**
 * Shell 终态错误文案真源（文案即契约：no_suitable_shell）。
 *
 * terminalToolError.ts 以 'No suitable shell found' 字符串判据把该文案分类为
 * 会话内不可重试终态；tests/unit/terminal-tool-error-classification.test.ts
 * 直接 import 本常量构造 forward 用例（producer↔判据由测试桥接钉死）。
 * 改文案必须同步签名判据和分类测试。
 *
 * 单独成模块的原因：让分类测试无需 import Shell.ts 的重依赖图
 * （analytics / bootstrap state / sandbox / bridge adapters）。
 */
export const NO_SUITABLE_SHELL_MESSAGE =
  'No suitable shell found. CrabCode requires a Posix shell environment. ' +
  'Install a valid shell (bash/zsh) and set the SHELL environment variable, ' +
  'then RESTART CrabCode: the shell is detected once per session, so changes ' +
  'to SHELL / CRABCODE_SHELL do NOT take effect in the current session.'
