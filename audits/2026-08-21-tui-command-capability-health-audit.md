# TUI 命令能力健康审计（2026-08-21）

- 审计日期：2026-08-21（America/Los_Angeles）
- 仓库：`CrabCode-TUI`
- 分支：`main`，跟踪 `upstream/main`
- `HEAD`：`08750956ab2feb8877ae82a9f49f60c3457fd0b2`（`Merge pull request #1 from acosmi/codex/tui-command-health-remediation`）
- 产品版本：`package.json` / workspace `1.0.35`
- 前置落档：[2026-08-20 TUI 命令健康审计、决策与落地记录](./2026-08-20-tui-command-health-root-cause-remediation.md)
- 审计结论：**有条件通过。已移植命令链路健康；多数参考命令仍是诚实 fail-closed，不是随机失灵。合同把「有 owner」算健康，默认公开可用性更窄。**

## 1. 最终结论

本轮是对当前工作树的独立复核，不是复述 08-20 修复说明。复核对象是「用户在纯 TUI 里输入 `/token` 之后，发现、提交、执行、失败、取消和呈现是否与唯一 owner 一致」。

三层事实必须分开：

1. **执行链路健康（已通过）**：08-20 合入后，精确 Enter 提交、slash 禁用持久化、catalog 与 dispatcher 同 identity、MCP owner/lane、direct SDK MCP fail-closed、handler 假成功修复，仍然成立。严格合同 104 token × 9 维 = 936 格：706 verified、230 N/A、0 shared-only、0 unverified；`bun scripts/verify-direct-tui-command-capabilities.mjs --require-complete` 状态 `verified`。
2. **能力覆盖不完整（产品缺口，不是回归）**：参考集合 96 个 token 中 54 个、加上本项目 6 个扩展，共 **60 个** 在 renderer 边界 fail-closed。用户看到「在纯 TUI 中不可用；命令未发送」是设计结果。
3. **合同健康 ≠ 默认公开可用性（本轮新发现）**：runtime catalog 的 25 个 token 在合同里算「有 TypeScript owner」。默认公开构建（`USER_TYPE` 未设、feature 默认、未登录）里，`advisor` / `files` / `extra-usage` / `install-slack-app` 会被门控移出可执行清单，随后走与未移植命令相同的 fail-closed 文案。

因此：**已广告且当前门控放行的命令可用且健康；未移植命令诚实不可用；若干「合同 runtime-owned」命令对默认公开用户实际不可用。** 没有发现会把未知 slash 误发给模型、或把未实现命令标成成功的 P0 执行缺陷。

## 2. 审计范围与方法

### 2.1 范围内

- 104 个已知 invocation token（参考 96 + 本项目扩展 8）
- Rust renderer-local 调度（`crates/crabcode-tui/src/tui_app.rs::handle_local_command`）
- TypeScript `DIRECT_TUI_BUILTINS` 与 `projectDirectTuiCommandCatalogEntries`
- slash 解析、route 禁用、catalog refresh
- 默认 feature / 鉴权门控对真实可执行清单的影响
- 既有严格证据门禁与 production smoke

### 2.2 范围外

- 未登录真实 OAuth 提供方、未打开真实浏览器、未写用户磁盘的端到端手工点击
- GUI / AppServer / Ink 命令树
- 动态 plugin / skill / workflow 的业务正确性（它们不在 104 分母内，但会改变用户可见 slash 面）
- 工作树里与命令无关的本地改动：`README.md`、`README.en.md`、`scripts/install.sh`

### 2.3 方法

| 步骤 | 结果 |
|---|---|
| 对照 08-20 落档与当前 `HEAD` | 修复提交已合入 `main` |
| 读取合同 `contracts/direct-tui-command-capabilities/v1/command-capabilities.json` | 分母、owner、fail-closed 分组、生命周期 profile 完整 |
| 静态读取 renderer / runtime 注册表与 handler | 见第 4、5 节 |
| 导出默认环境下 `getDirectTuiBuiltInCommandDefinitions()` | 22 个 command 对象、25 个 invocation name，与 runtime catalog 集合一致 |
| `--report` 合同报告 | `status: verified`，`referenceStatus: snapshot_only` |
| `--require-complete` 严格证据 | `status: verified`（Bun 1.3.14，约 38s） |
| 定向单测 | catalog / projection / refresh / route policy / retained actions / production smoke 通过；lifecycle contract 自测 4/4 通过 |

健康定义与 08-20 相同：有唯一执行 owner，且发现到 transcript 恢复与该 owner 一致；或明确不可用并在 renderer 边界 fail-closed，不误发 backend、不退化成普通 prompt、不假成功。

## 3. 分母与健康切面

| 项目 | 数量 |
|---|---:|
| 参考 token | 96（18 renderer / 24 runtime / 54 fail-closed） |
| 本项目扩展 | 8（renderer `find`、runtime `com`、fail-closed 6） |
| 完整 token 分母 | **104**（19 renderer / 25 runtime / 60 fail-closed） |
| 生命周期 profile | 21 |
| 每 token 维度 | 9 |
| coverage cells | 936 |
| verified / N/A / shared-only / unverified | **706 / 230 / 0 / 0** |

N/A 表示该 owner 根本没有相应阶段（例如原子 handler 没有 renderer 中途取消点），不是缺测。

### 3.1 默认公开构建的实际可执行面

在 `USER_TYPE` 未设、feature 默认表、配置可读、无订阅的条件下，live registry 为：

| 状态 | Token |
|---|---|
| Renderer 可见且有实现 | `help` `exit` `quit` `find` `model` `color` `rename` `usage` `bug` `feedback` `plugin` `plugins` `marketplace` `login` `logout` `mcp` `context` `reload-plugins` `btw`（19） |
| Runtime 默认可发现 | `clear`（含 `reset`/`new`）、`compact`（含 `com`）、`compact-history`、`cost`、`init`、`insights`、`local-models`、`pr-comments`、`proxy`、`release-notes`、`review`、`security-review`、`smallmodel`、`statusline`、`vision`、以及非 CSI u 终端上的 `terminal-setup` |
| Runtime 隐藏但仍可精确键入 | `heapdump`、`output-style`（已废弃，只提示改 settings） |
| Runtime 合同有 owner、默认被门控后 fail-closed | `advisor`（GrowthBook / first-party beta）、`files`（仅 `USER_TYPE=ant`）、`extra-usage`（订阅 + 允许的 billingType）、`install-slack-app`（`availability: crabcode-ai`） |
| 明确 fail-closed | 其余 60 个已知 token |

对默认公开用户：大约 **19 个 renderer + 16 个 runtime 主名** 可发现并走完整生命周期；约 **4 个 runtime token** 虽在合同里属 catalog owner，默认会话里与未移植命令同一条 fail-closed 文案。

## 4. 已移植能力：可用性判断

### 4.1 Renderer-local（19）

`handle_local_command` 对固定集合有独立实现，且 runtime catalog 碰撞（除保留私有 `bug|feedback|logout|reload-plugins`）优先。抽查结果：

- `help` / `exit` / `quit` / `find`：纯 renderer 状态机，无 backend。
- `model`：无参打开 picker；`manage` 打开直连模型管理；具体 id 走 `set_model`。
- `color` / `rename`：走 retained private action，TypeScript 仍是持久化权威。
- `usage` / `plugin` / `plugins` / `marketplace`：打开直连用量/插件管理面。
- `login` / `logout` / `mcp` / `context` / `reload-plugins` / `btw`：映射既有 SDK/control，不新造协议。
- 空参 `/local-models` 在已广告时被 renderer 截获为模型管理面；带 subcommand 的 `/local-models install|add|remove|...` 仍交给 TypeScript。这是有意分工，不是丢 handler。

合同将这些 token 标为 verified。本轮未发现 renderer 把已知不可用 token 发给模型的路径：未广告时命中 `UNAVAILABLE_LOCAL_COMMAND_TOKENS`，status 为「未发送」。

### 4.2 Runtime catalog（25，含别名 `com`/`reset`/`new`）

`DIRECT_TUI_BUILTINS` 在默认旗标下 22 个 command 对象，invocation 集合与合同 runtime catalog 完全重合，没有合同外泄漏，也没有合同内缺注册。

Handler 抽查：

| Token | 类型 | 本轮判断 |
|---|---|---|
| `clear` / `reset` / `new` | local（direct 投影 `supportsNonInteractive: true`） | 会话清理；有成功/失败/取消/恢复证据 |
| `compact` / `com` | local | 空历史真实 error；有取消与 transcript 恢复证据 |
| `compact-history` | local | 同上家族 |
| `cost` | local | 只读用量文案；订阅者默认隐藏但仍可精确键入 |
| `init` | prompt | onboarding 写失败走真实 error terminal |
| `insights` | prompt | 阶段边界检查 abort；不再伪造取消 |
| `local-models` | local | 子命令走 local-model client；失败返回 text |
| `pr-comments` | prompt | 公开用户走 gh 回退 prompt；`USER_TYPE=ant` 改为「已迁到 plugin」说明 |
| `proxy` | local | 只读诊断 + `use-system`/`off` 写 `settings.env`；PAC 拒绝写入 |
| `release-notes` | local | 拉取超时回退缓存或 changelog URL |
| `review` | prompt | 本地 review prompt，依赖 `gh` |
| `security-review` | prompt | 公开用户执行 markdown 内 shell 权威缝；ant 改为 plugin 说明 |
| `smallmodel` | local | 写失败抛错，不再假成功；**不校验模型名**（见 5.3） |
| `statusline` | prompt | 直接 TUI 与 print/SDK 对象分离，避免扩大 headless 面 |
| `terminal-setup` | local adapter | 捕获 historical `onDone`，不把 local-jsx 协议加宽 |
| `vision` | local | 授权绑定写入时刻重解析的目的地；写失败返回文本 |
| `heapdump` | local，隐藏 | 失败返回错误文本 |
| `output-style` | local-jsx 白名单 | 已废弃，提示改 settings 文件；`/config` 本身 fail-closed |
| `advisor` | local | 先写 settings 成功再 commit AppState；默认 `isEnabled=false` |
| `files` | local | **仅 ant**；公开用户不进 inventory |
| `extra-usage` | local | 依赖订阅与 billingType；默认通常不进 inventory |
| `install-slack-app` | local 投影 | 需 crabcode-ai 可用性；打开浏览器；失败仍可能累加点击计数（见 5.3） |

direct TUI 将 `QueryEngine.interactive = true`，因此 `output-style` 的 local-jsx 分支不会被 `command_not_headless_executable` 误伤。

Production smoke 覆盖了 19 个生产 definition 的真实 lazy-load（不含 `clear`/`compact` 家族，那些走独立 terminal-gap 矩阵）。`--require-complete` 把这些证据钉进门禁。

## 5. 缺陷与分叉

严重度：P0 执行安全/假成功；P1 已广告命令在默认路径不可用或终态撒谎；P2 合同/可用性/预览旗标分叉；P3 体验与边角。

本轮 **未发现 P0/P1**。下列为 P2/P3。

### 5.1 P2 合同「runtime-owned」与默认公开可用性分叉

合同把 `files`、`advisor`、`extra-usage`、`install-slack-app` 放在 `owners.runtimeCatalog`。这在「实现存在且广告后可执行」意义上成立。

默认公开会话里它们会被 `isEnabled` / `availability` 滤掉。滤掉之后 renderer 仍认识这些名字（它们也在 `UNAVAILABLE_LOCAL_COMMAND_TOKENS` 里），于是用户得到与 `/settings`、`/resume` 相同的句子：

```text
/{name} 在纯 TUI 中不可用；命令未发送
```

这会把三种不同事实混成一句：

- 从未移植（如 `/settings`、`/resume`）
- 本构建永远关闭（如公开包里的 `/files`）
- 当前账户/实验未放行（如 `/advisor`、`/extra-usage`）

风险不是误执行，而是运维与用户把「未登录所以 extra-usage 不可用」理解成「TUI 坏了」。

### 5.2 P2 预览旗标会把合同 fail-closed token 广告成可执行 stub

`/proactive` 源码写明「Phase -1 stub」，`call` 只返回尚未实现。合同把它放在 `nonReferenceKnownTokens.failClosed`。

但 `DIRECT_TUI_BUILTINS` 在 `feature('PROACTIVE') || feature('KAIROS')` 为真时，用 `projectDirectRendererNeutralLocal` 把它强行标成 `supportsNonInteractive: true` 并注册。默认 feature 表两者都是 `false`，所以默认包仍 fail-closed。一旦预览旗标打开：

- catalog 可以广告 `/proactive`
- 执行成功返回 stub 文本，看起来像「命令可用」
- 合同分母仍把它算 fail-closed

`/version` 有真实 handler，但 `isEnabled` 要求 `USER_TYPE === 'ant'`；合同同样 fail-closed。ant 内部构建会与合同分叉。

`/workflows` 在 `WORKFLOW_SCRIPTS`（默认 false）下进入 builtins，**不在 104 分母内**。这是动态/旗标面，不是合同泄漏，但预览包的 slash 面大于合同。

### 5.3 P3 已移植命令的产品边角

1. **`/output-style` 是死胡同**：隐藏 + 废弃文案指向 settings 文件，而 `/config`、`/settings` 均为 fail-closed。命令本身终态诚实，只是没有 TUI 内替代入口。
2. **`/install-slack-app`**：`openBrowser` 失败前已经 `saveGlobalConfig` 累加 `slackAppInstallCount`。
3. **`/smallmodel`**：任意非空字符串可写入 `userSettings.smallModel`，没有 `advisor` 那种 `validateModel`。写失败会抛错，这点是健康的。
4. **`/vim` / `/brief`**：TypeScript handler 与 private `retained.vim.toggle` / `retained.brief.toggle` 协议仍在；slash 在 renderer 侧 fail-closed，因为 native renderer 没有完整 vim keymap / brief 过滤状态机。协议面大于 slash 面，但 slash 行为诚实。
5. **`/skills` fail-closed，但 skill 仍可当 slash 用**：`DEEP_RESEARCH` 默认 true，bundled `/deep-research` 等可被发现。用户敲 `/skills` 会看到「纯 TUI 不可用」，并不等于 skill 子系统关闭。

### 5.4 08-20 已关闭、本轮确认仍关闭的缺陷

未复发：

- 精确 runtime token 首次 Enter 只补空格
- `--disable-slash-commands` 被后续 plugin/MCP refresh 复活
- catalog owner 与 first-wins dispatcher 分叉
- live MCP inventory 与目录分叉
- direct SDK-transport MCP 假成功
- `advisor`/`smallmodel` 写失败假成功
- `/insights` 用替换 handler 伪造取消
- 证据门禁用替换 handler 冒充 19 个命令业务成功

## 6. 60 个 fail-closed token（产品覆盖缺口）

这些 token 在合同和 Rust 守卫里都是一等公民：有测试证明「不发送」。它们不是审计遗漏，是尚未移植的 UI/权限/会话状态机。

| 分组 | Token | 原因（合同原文压缩） |
|---|---|---|
| desktop_or_workspace_ui（12） | `add-dir` `chrome` `config` `copy` `desktop` `diff` `doctor` `ide` `keybindings` `memory` `settings` `theme` | 依赖桌面/JSX 面板或 OS 集成 |
| device_or_remote_control（10） | `android` `app` `ios` `mobile` `office` `rc` `remote` `remote-control` `remote-env` `remote-status` | 需要进程外设备/远程权威 |
| session_history_navigation（11） | `branch` `checkpoint` `continue` `export` `fork` `resume` `rewind` `session` `tag` `think-back` `thinkback-play` | 会话/历史事务未移植 |
| permission_or_execution_panel（9） | `allowed-tools` `bashes` `hooks` `passes` `permissions` `plan` `privacy-settings` `sandbox` `tasks` | 交互权限/计划/任务面，不能用纯文本发送冒充 |
| account_or_service_management（7） | `install-github-app` `rate-limit-options` `skills` `stats` `status` `update` `upgrade` | 账户/更新/技能管理面不在 direct 合同内 |
| interactive_mode_or_review_surface（5） | `agents` `effort` `fast` `ultrareview` `vim` | 交互模式/评审 UI 未移植 |
| 本项目扩展（6） | `bridge-kick` `brief` `proactive` `ultraplan` `version` `voice` | 无诚实纯 TUI 生命周期（或仅内部/预览） |

用户体感上最容易当成「TUI 坏了」的是：`/settings`、`/config`、`/resume`、`/session`、`/plan`、`/permissions`、`/vim`、`/skills`、`/memory`、`/doctor`。

## 7. 验证记录

环境：Bun `1.3.14`，rustc `1.96.0`。命令证据在完整权限下执行（沙箱会把 cargo target 指到不可写缓存，不能当作产品失败）。

| 验证 | 结果 |
|---|---|
| `HEAD` | `08750956ab2feb8877ae82a9f49f60c3457fd0b2` |
| live `DIRECT_TUI_BUILTINS` invocation 集合 | 与合同 runtime catalog 25 token 一致；`advertisedButFailClosed=[]`，`liveNotInContract=[]` |
| `bun scripts/verify-direct-tui-command-capabilities.mjs --report` | 104 token，706 verified，230 N/A，0 unverified，`status=verified`，`referenceStatus=snapshot_only` |
| 同上 `--require-complete` | `status=verified` |
| production smoke 7 tests | pass（真实 handler，只在外部权威缝 mock） |
| catalog / projection / refresh / route policy / retained / parsing | 定向文件 pass |
| lifecycle contract 自测（含 missing/skip mutation seam） | 4 pass，46 assertions |

未在本轮重跑：全量 `bun run test`、`bun run check`、`cargo test -p crabcode-tui --lib` 全集、`bun run smoke:tui`。08-20 落档在同一命令链合入前已跑通；本轮用严格命令门禁代替全量回归。若发布签字需要全量，应另开发布检查。

## 8. 剩余边界（与 08-20 一致，并补充）

- 60 个 fail-closed token 仍是产品覆盖缺口。门禁把它们算进 104 分母，功能没有被伪装成已移植。
- direct native TUI 仍不支持 SDK-transport MCP；没有 reverse `mcp_message` 之前不能开放相关入口。
- 原子外部权威命令没有 renderer 中途取消协议，合同标 N/A。
- 普通 reference gate 仍是 pinned Git blob 的 `snapshot_only`，不是动态执行邻接仓。
- **新增**：合同 runtime-owned 集合不能直接当作用户手册上的「默认都能用」清单；必须叠加 `isEnabled`、`availability`、feature 与订阅。
- **新增**：打开 `PROACTIVE`/`KAIROS` 不得把 stub `/proactive` 当成已交付能力。
- **新增**：Windows 上 Memory serving child 仍可能弹出 Console 窗口；未修复。

## 9. 建议（本轮只落档，不改代码）

1. 若要降低「TUI 命令不可用」误报：把 fail-closed 文案拆成「未移植」与「当前账户/构建未启用」，至少覆盖 `files`、`advisor`、`extra-usage`、`install-slack-app`。
2. 在 `PROACTIVE`/`KAIROS` 真正实现前，不要把 stub `/proactive` 投影进 `DIRECT_TUI_BUILTINS`；或打开旗标时把合同从 fail-closed 改为明确的 stub profile。
3. 对外说明命令能力时，使用「默认公开可发现集合」，不要直接引用 25 个 runtime catalog token。
4. 若继续移植，优先用户最容易当成故障的：`settings`/`config`、`resume`/`session`、`plan`/`permissions`、`vim`、skills 管理面。不要为了数字去转发 60 个 token。
5. Windows 记忆窗口：在 `run_coordinator` 的 serving child spawn 上加 `CREATE_NO_WINDOW`（不要与 `DETACHED_PROCESS` 组合），并补 Windows 证据测试。

## 10. 补充：Windows 是否还会拉起记忆相关窗口

**结论：未修复。** 当前 `HEAD` 上，Windows 启动 Memory 运行时仍可能弹出一个 Console 子系统窗口（任务栏/桌面上常见标题为 `acosmi-memory-orchestrator.exe`），不是 TUI 里的记忆 overlay。

因果链：

1. `acosmi-memory-orchestrator` 是 Console 子系统二进制（发布盘点 fixture 也按 `"Console"` 会话名解析），没有 `windows_subsystem = "windows"`。
2. TUI 经 `MemoryRuntimeCoordinator::spawn_supervisor` → `acosmi_daemon_launcher::spawn_detached_command` → `spawn_windows.rs`，用 `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB` 拉起 **coordinator**。这一层有意脱离 TUI 控制台，coordinator 自身通常不占一个窗。
3. coordinator 在 `libs/acosmi-memory/acosmi-memory-orchestrator/src/main.rs::run_coordinator` 里用 `tokio::process::Command` 再拉 **serving child**，只设了 `stdin(null)` 和 stdio inherit，**没有** `CREATE_NO_WINDOW`（`0x08000000`），也没有 `DETACHED_PROCESS`。
4. Win32 规则：父进程没有控制台时，再 CreateProcess 一个 CUI 子进程且不带 `CREATE_NO_WINDOW` / `DETACHED_PROCESS` / `CREATE_NEW_CONSOLE`，系统会给子进程分配**新控制台**。子进程还会 `eprintln!("acosmi-memory-orchestrator listening on …")`，窗口内容就是记忆侧车日志。

同仓库里 mermaid worker、外链打开、外部编辑器已经用 `CREATE_NO_WINDOW` 防闪窗；记忆 serving child 没有对齐。`crates/acosmi-daemon-launcher/tests/detached_command.rs` 仅 Unix，没有 Windows「不弹窗」证据。

这不是命令 token 合同范围，但是 Windows 上可复现的侧车生命周期缺陷。修复方向（本轮仍不改代码）：在 `run_coordinator` 的 Windows spawn 上加 `CREATE_NO_WINDOW`（不要和 `DETACHED_PROCESS` 组合，MSDN 写明后者会让前者被忽略），并加一条 Windows 证据测试。

## 11. 关联文档

- [2026-08-20 根因与落地](./2026-08-20-tui-command-health-root-cause-remediation.md)
- 合同：`contracts/direct-tui-command-capabilities/v1/command-capabilities.json`
- 门禁：`scripts/verify-direct-tui-command-capabilities.mjs`
- 注册表：`src/cli/headlessCommands.ts`
- Renderer 调度：`crates/crabcode-tui/src/tui_app.rs`
