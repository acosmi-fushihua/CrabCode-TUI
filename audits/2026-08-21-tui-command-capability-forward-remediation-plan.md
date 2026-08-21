# TUI 命令能力根因补齐·向前实现方案（2026-08-21）

- 依据：[2026-08-21 TUI 命令能力健康审计](./2026-08-21-tui-command-capability-health-audit.md)
- 前置：[2026-08-20 根因与落地](./2026-08-20-tui-command-health-root-cause-remediation.md)
- 基线：`main` @ `08750956ab2feb8877ae82a9f49f60c3457fd0b2`
- 性质：**实施方案**。所有裁决已定，无待决项。实施者按 Wave 顺序执行，每个 Wave 一个独立 PR，全部门禁绿后合入。
- 本方案所有文件路径、符号名、行号均已对照当前工作树核实（行号为写稿时参考值，实施时以符号定位为准）。

## 0. 总原则（不可违反的红线）

1. **向前实现，不退化**：本方案移植 10 个 token 的真实能力进 Rust 渲染层，不是批量改文案。文案分级只覆盖"确实被门控"的 token。
2. 沿用 08-20 健康定义：每个移植 token 必须有唯一 owner + 9 维生命周期证据；做不到就保持诚实 fail-closed。
3. 禁止：批量转发未移植 token、假成功、给 direct native 伪造 SDK MCP host、新增**公开** wire subtype（新协议一律走 direct-TUI 私有车道：retained action 或 `crabcode_tui_*` 私有 control）、动 reserved-private 集合（`bug|feedback|logout|reload-plugins`）。
4. 每个 Wave 完成时 `bun scripts/verify-direct-tui-command-capabilities.mjs --require-complete` 必须 `verified`，合同分母恒为 104、cells 恒为 936。
5. 工作树里 `README.md`、`README.en.md`、`scripts/install.sh` 是用户本地改动，任何 Wave 不得触碰。

## 1. 裁决记录（第一性原理，含被否方案）

| # | 问题 | 裁决 | 理由 / 被否方案 |
|---|---|---|---|
| R1 | fail-closed 文案混淆三种事实怎么拆 | **Rust 侧静态两张表 + 门控因由枚举**，不改协议 | 文案只在"token 未被 catalog 广告"时显示，此时静态分类永真（一旦门控放行，token 进 catalog 走 runtime first-wins，永不命中该路径）。被否：catalog refresh 协议加 gated 列表——动态真相无增量收益，却扩了私有 wire 面、加了 UTF-16/limits 关联方 |
| R2 | `/proactive` stub 被预览旗标广告 | **删除投影**（保留 `src/commands/proactive.ts` 模块本身），加静态注册表测试封死"合同 fail-closed token 被投影"整类问题 | stub 交付 ≠ 能力交付。被否：合同加 stub profile——等于给"看起来能用"发许可证 |
| R3 | `/version`（ant 门控真 handler）与合同分叉 | 合同保持 fail-closed（合同对象=默认公开构建），新增 `buildAnnex` 节登记 ant/旗标构建的额外广告面，verify 脚本双向断言 | 被否：把 version 挪进 runtimeCatalog——会把默认公开分母写谎 |
| R4 | 60 个 token 移植哪些 | 移植 10 个：`settings` `config` `vim` `resume` `session` `continue` `permissions` `plan` `skills` + 扩展 `brief`。其余 50 个保持 fail-closed | 依据审计 §9.4 用户误判优先级 + 协议缝就绪度（vim/brief retained 协议已双侧存在；session picker 组件已存在）。被否：`/memory`（依赖记忆侧车协议面，Windows 侧车缺陷未修完不动）、`/doctor`（无诊断框架权威可绑定）、其余 48 个（08-20 红线：不为数字转发） |
| R5 | vim 实现范围 | Normal/Insert 双模 + 核心动作集（见 W1），**无** count 前缀、寄存器、visual 模式、ex 命令、独立 undo | composer 是单输入框不是编辑器；核心集已覆盖真实输入场景。被否：完整 vim 仿真 |
| R6 | 运行中 `/resume` 怎么做 | 新增 2 个私有 StructuredIO control（`crabcode_tui_session_list` / `crabcode_tui_session_resume`），TS 保持会话存储唯一权威，渲染层只管 picker 生命周期 | 启动 picker 是 setup 阶段桥（StructuredIO 交接前），运行中必须走 control 车道。被否：进程内重启冒充切换、渲染层扫磁盘 |
| R7 | `/smallmodel` 校验 | set 路径接入既有 `validateModel`（advisor 同款：允许集 + 别名 + 真实 API 探测），失败抛真实 error | 与 advisor 行为对齐，无新造校验器 |
| R8 | `/install-slack-app` 计数 | `openBrowser` 成功后才累加 `slackAppInstallCount`；`logEvent` 点击埋点保留在最前 | 计数语义是"完成安装跳转"不是"尝试" |
| R9 | Windows 弹窗修哪些 spawn 点 | **只修** `run_coordinator` serving child 一处，加 `CREATE_NO_WINDOW`（禁止组合 `DETACHED_PROCESS`，MSDN：组合时前者被忽略） | 全仓 Windows spawn 点已逐一排查（见 §2）。被否：给 `acosmi-supervisor/src/child.rs` 加 flag——它管理前台 TUI，必须继承控制台，加了会坏 |
| R10 | `/plan` 形态 | 复用既有公开 control `set_permission_mode`；v1 只做模式切换 + 瞬时状态文案，**不做**常驻模式徽标 | 后端可经 `EnterPlanModeTool` 自行改模式且无反向投影，常驻徽标会说谎。被否：新增模式同步协议（超出本轮） |
| R11 | `/skills` 形态 | 只读清单面板；Enter 将 `/{name} ` 预填进 composer，不代执行 | 技能执行 owner 是现有 slash 派发链，面板不造第二执行器 |
| R12 | `/settings` v1 可编辑范围 | 可编辑白名单 = `smallModel`、`outputStyle`（均为 `SettingsJson` 真实键：`src/utils/settings/types.ts` L379、L649）；各 source 完整 JSON 只读展示 + 文件路径 | 复杂键（permissions、env、mcp）各有专属权威面，settings 面板不越权。`editorMode` 存 global config 不在 settings，归 `/vim` 管 |

## 2. Windows spawn 点全仓排查结论（举一反三，已核实）

| 位置 | 现状 | 判定 |
|---|---|---|
| `libs/acosmi-memory/acosmi-memory-orchestrator/src/main.rs::run_coordinator`（L152–162 spawn serving child） | 无 creation_flags，stdout/stderr inherit | **唯一缺陷点**，W0.1 修复 |
| `crates/acosmi-daemon-launcher/src/spawn_windows.rs`（拉起 coordinator） | `DETACHED_PROCESS\|CREATE_NEW_PROCESS_GROUP\|CREATE_BREAKAWAY_FROM_JOB` | 健康（DETACHED 不分配控制台），不动 |
| `crates/crabcode-tui/src/tui_link_opener.rs` L190–192、`external_editor.rs` L808–810 | `CREATE_NO_WINDOW` | 健康，是 W0.1 的模仿模板 |
| `crates/crabcode-mermaid/src/subprocess.rs` L35、`crabcode_mermaid_worker.rs` L502 | `0x0800_0200` | 健康 |
| `crates/crabcode-pager-render/src/link_opener.rs` L67、`crabcode-cli/src/native_generation.rs` L1500 | 含 `DETACHED_PROCESS` | 健康 |
| `crates/acosmi-supervisor/src/child.rs` L328 | 仅 `CREATE_NEW_PROCESS_GROUP` | 健康（父进程持有控制台，子进程含前台 TUI 必须继承）。**禁止加 CREATE_NO_WINDOW** |
| `crates/acosmi-exec/src/process.rs` L279 | 仅 `CREATE_NEW_PROCESS_GROUP` | 健康（父端 TUI 有控制台），不动 |
| `libs/acosmi-memory` 其余 `Command::new` | 全部在 build.rs / tests | 非产品路径，不动 |

## 3. Wave 总览与合同终态

| Wave | 内容 | 移植 token | 规模 |
|---|---|---|---|
| W0 | 诚实性与安全修复（5 项） | 无 | 小 |
| W1 | `/vim` `/brief`（retained 缝已就绪） | vim, brief | 中 |
| W2 | 设置面 `/settings` `/config` + `/output-style` 收尾 | settings, config | 中大 |
| W3 | 会话面 `/resume` `/session` `/continue` | resume, session, continue | 大 |
| W4 | `/permissions` `/plan` | permissions, plan | 中 |
| W5 | `/skills` 清单面 | skills | 小中 |

合同分母演进（总数恒 104；每 Wave 增量更新，下表为 W5 后终态）：

| 项 | 现值 | 终态 |
|---|---:|---:|
| rendererLocal（参考） | 18 | 27 |
| runtimeCatalog（参考） | 24 | 24 |
| failClosed（参考） | 54 | 45 |
| 扩展 rendererLocal | 1（find） | 2（find, brief） |
| 扩展 failClosed | 6 | 5（bridge-kick, proactive, ultraplan, version, voice） |
| lifecycle profiles | 21 | 28（新增 7 个，见各 Wave） |
| coverage cells | 936 | 936 |

failClosedGroups 终态：desktop_or_workspace_ui 12→10、session_history_navigation 11→8、permission_or_execution_panel 9→7、account_or_service_management 7→6、interactive_mode_or_review_surface 5→4、device_or_remote_control 10 不变。

每移植一个 token 的合同固定 8 步（全 Wave 通用，来源：合同结构核查）：
1. 从 `failClosedGroups[].invocationTokens`（或 `nonReferenceKnownTokens.failClosed`）删除；
2. 加入 `owners.rendererLocal.invocationTokens`（或 `nonReferenceKnownTokens.rendererLocal`）；
3. 更新 `invariants.rendererLocalReferenceCount` / `failClosedOnlyReferenceCount`；
4. 从 `reference_fail_closed`/`extension_fail_closed` profile 挪到本 Wave 新建 profile，补 9 维 coverage（原子动作的 cancellation 标 `not_applicable` 并写 note）；
5. `lifecycleAudit.discovery.rendererLocalVisible` 加入该 token；
6. 加入 `ownershipPrecedence.runtimeCollisionWinsForRendererFallback`（**不进** reservedRendererPrivate）；
7. Rust 同步：`FIXED_LOCAL_COMMAND_COMPLETIONS` 增行（zh/en 描述经 `fixed_local_command_description` 补齐）、从 `UNAVAILABLE_LOCAL_COMMAND_TOKENS` 删除；
8. `lifecycleAudit.evidence`（**id→对象字典**，非数组）登记新证据（`rust_test` 带 markers / `bun_test` 带 markers+testNames），每个 verified 格引用之；未被引用的新证据会导致门禁失败。

## 4. W0 诚实性与安全修复

### W0.1 Windows 记忆侧车弹窗

- 改 `libs/acosmi-memory/acosmi-memory-orchestrator/src/main.rs::run_coordinator`，在 L162 `kill_on_drop(true)` 后追加：

```rust
#[cfg(windows)]
{
    // CUI 子进程 + 无控制台父进程：不加此 flag 系统会新分配控制台窗口。
    // 禁止与 DETACHED_PROCESS 组合（MSDN：组合时 CREATE_NO_WINDOW 被忽略）。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}
```

- stdio 三项**不动**（inherit 让子进程日志继续流入 coordinator 被 daemon-launcher 重定向的 log 文件）。
- 把 flag 抽成 `pub(crate) const SERVING_CHILD_CREATION_FLAGS: u32`，加 `#[cfg(all(test, windows))]` 单测断言值为 `0x0800_0000`（Windows-only 测试先例：`libs/acosmi-memory/.../event_sink.rs` 的 `windows_tests`、`crates/acosmi-sandbox/tests/windows_token_repro.rs`）。
- **诚实边界，写进 PR 描述**：默认 CI 的 rust job 只跑 ubuntu，该测试需在 release `build-windows` 机或本地 Windows 手工执行一次并留证；本方案不新增 CI 矩阵。
- 门禁：`cargo test --locked --manifest-path libs/acosmi-memory/Cargo.toml --workspace`（独立 workspace，根 workspace 不含它）。
- 关联方：不触碰 §2 表中任何其他 spawn 点。

### W0.2 预览旗标 stub 投影封死 + buildAnnex

- `src/cli/headlessCommands.ts`：删除 `DIRECT_TUI_PROACTIVE` 常量（L156–157）及 `DIRECT_TUI_BUILTINS` 里的条件展开（L226–228）。`src/commands/proactive.ts` 文件保留不动。
- 合同新增顶层键 `buildAnnex`（schemaVersion 保持 1，verify 脚本同步识别）：

```json
"buildAnnex": {
  "description": "默认公开构建之外可能额外广告的 token 及条件；不参与 104 分母",
  "entries": [
    { "token": "version", "condition": "USER_TYPE=ant && !IS_DEMO" },
    { "token": "commit", "condition": "USER_TYPE=ant && !IS_DEMO" },
    { "token": "commit-push-pr", "condition": "USER_TYPE=ant && !IS_DEMO" },
    { "token": "init-verifiers", "condition": "USER_TYPE=ant && !IS_DEMO" },
    { "token": "workflows", "condition": "feature(WORKFLOW_SCRIPTS)" }
  ]
}
```

（前四项来源 `ANT_RENDERER_NEUTRAL_BUILTINS` L77–80；workflows 来源 L86–90。ant 专属 token 的具体名以实现时 `commit`/`commitPushPr`/`initVerifiers` 的 invocation name 为准。）
- 新增 bun 测试（放 `tests/unit/direct-tui-command-catalog.test.ts` 同族）：穷举 `PROACTIVE`/`KAIROS`/`WORKFLOW_SCRIPTS` × `USER_TYPE`（ant/未设）组合，断言 `DIRECT_TUI_BUILTINS` invocation 集合 ⊆ 合同 runtimeCatalog ∪ buildAnnex，且 ∩ 合同 failClosed（扣除 buildAnnex）= ∅。旗标注入用 `feature()` 的 env 优先级（`src/utils/featurePolyfill.ts` L171–201）。
- 关联方：`--require-complete` 的注册表一致性断言、`tests/unit/direct-tui-command-catalog-projection.test.ts`。

### W0.3 `/install-slack-app` 计数时序

- `src/commands/install-slack-app/install-slack-app.ts`：把 `saveGlobalConfig` 累加块移到 `await openBrowser(...)` 之后、仅 `success === true` 分支执行。`logEvent`（L9）留在最前。
- 测试：新增用例（挂 `tests/unit/direct-tui-runtime-command-production-smoke.test.ts`），mock `openBrowser` 分别返回 false/true，断言 `slackAppInstallCount` 不变/加一。
- 关联方：合同 profile `runtime_catalog_atomic_authority` 中该 token 的 failure 维证据需指向新测试。

### W0.4 `/smallmodel` 模型名校验

- `src/commands/smallmodel/smallmodel.ts` set 路径（L48 前）：先 `const { valid, error: validationError } = await validateModel(model)`（import 自 `src/utils/model/validateModel.ts`，签名 `(model: string) => Promise<{ valid: boolean; error?: string }>`），`!valid` 时 `throw new Error(validationError)`。reset/查询路径不动。
- 测试：mock 校验缝（与 advisor 测试同法，外部权威缝在 `sideQuery`/allowlist），断言非法名不写 settings、合法名照写；写失败仍抛错（既有行为回归）。
- 关联方：production smoke 中 smallmodel 用例、合同该 token failure 维证据。

### W0.5 fail-closed 文案分级

- `crates/crabcode-tui/src/tui_app.rs`：
  1. 保留常量名 `UNAVAILABLE_LOCAL_COMMAND_TOKENS`，内容裁剪为**未移植集**（当前 60 个；后续 Wave 逐个移出）。
  2. 新增：

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RuntimeGateReason { Build, Account, Environment, Session }

/// 与合同 runtimeCatalog（含别名）∪ {"version"} 严格等集，verify 脚本静态断言。
const GATED_RUNTIME_COMMAND_TOKENS: &[(&str, RuntimeGateReason)] = &[
    ("advisor", Account), ("extra-usage", Account), ("install-slack-app", Account),
    ("files", Build), ("version", Build),
    ("terminal-setup", Environment),
    // 其余 runtime token（clear/reset/new/compact/com/compact-history/cost/heapdump/
    // init/insights/local-models/output-style/pr-comments/proxy/release-notes/review/
    // security-review/smallmodel/statusline/vision）→ Session 兜底
];
```

  3. `handle_local_command` 兜底分支（现 L9096–9101）改为：先查 `GATED_RUNTIME_COMMAND_TOKENS`，命中且未广告 → 按 reason 出文案；再查未移植集 → 原文案。文案（zh / en 成对，`fail_closed_status` 扩展为按类别取词）：
     - Build：`{name} 仅特定构建提供，当前构建未启用；命令未发送` / `{name} is only available in specific builds; command was not sent`
     - Account：`{name} 未对当前账户、订阅或实验放行；命令未发送` / `{name} is not enabled for this account, subscription, or experiment; command was not sent`
     - Environment：`{name} 在当前终端环境不适用；命令未发送` / `{name} does not apply to this terminal environment; command was not sent`
     - Session：`{name} 在当前会话未启用；命令未发送` / `{name} is not enabled in this session; command was not sent`
     - 未移植（不变）：`{name} 在纯 TUI 中不可用；命令未发送`
  4. `version` 从未移植集移入 gated 表（合同 owner 不变，见 R3）。
- verify 脚本：静态解析新增 `GATED_RUNTIME_COMMAND_TOKENS`，断言两集合等式：gated = runtimeCatalog 全 token（含 com/reset/new 别名）∪ {version}；未移植 = 全部 failClosed token − {version}。
- 测试：Rust 单测覆盖 5 类文案各至少一个 token；更新现有 fail-closed 断言（约 L27414–27442）；**同步更新合同 evidence 里 pin 旧文案字符串的 markers**。
- 关联方：`session_header.rs` 不读 free-form status（已核实），无 typed header 影响；i18n 双语成对；`--disable-slash-commands` 时 `handle_local_command` 提前返回 None（L8854–8857），分级文案不与禁用语义冲突。

## 5. W1 `/vim` `/brief`（协议缝已双侧存在）

**已就绪的缝（全部核实）**：TS `src/cli/directTuiRetainedCommandActions.ts`：action kinds 含 `retained.vim.toggle`/`retained.brief.toggle`（L21–27），result `retained.vim.updated{editor_mode: normal|vim}`（L102–107）、`retained.brief.updated{enabled, reminder_injected}`（L109–115），dispatch 与 `invokeVim`/`invokeBrief` 已实现（L307 起、L386–406），brief 门控依赖 `isBriefCommandEnabled`/`isBriefEntitled` 已存在。Rust 侧 `retained_command_surface.rs` 明确记录 vim/brief 缺渲染器状态机（`VIM_RENDERER_BLOCKER`）。

改动：

1. **Rust vim 状态机**：新模块 `crates/crabcode-tui/src/vim_composer_mode.rs`。
   - `enum VimMode { Insert, Normal }`；`struct VimComposerState { enabled: bool, mode: VimMode, pending_operator: Option<PendingOp> }`（PendingOp ∈ d/c/g 前缀）。
   - Normal 模式动作集（全部映射到 `crabcode_ratatui_textarea::TextArea` 已有原语，textarea.rs L661–2777，**不改 textarea crate**）：
     - 移动：`h`→move_cursor_left，`l`→right，`j`→down，`k`→up，`0`→beginning_of_line，`$`→end_of_line，`^`→行首非空白（beginning + 扫描），`w`/`b`/`e` 词移动（基于 `word_at_cursor` 与自实现扫描），`gg`→set_cursor(0)，`G`→末尾。
     - 删改：`x`→delete_forward(1)，`dd`→kill_current_line，`dw`→delete_forward_word，`db`→delete_backward_word，`D`→kill_to_end_of_line，`cc`/`S`→kill_current_line+入 Insert，`cw`→delete_forward_word+入 Insert，`C`→kill_to_end_of_line+入 Insert，`s`→x+入 Insert。
     - 粘贴：`p`→yank()。
     - 入 Insert：`i`、`a`（先 right）、`I`（行首）、`A`（行尾）、`o`/`O`（行下/上插空行）。
     - **明确不做**（写进模块文档）：count 前缀、寄存器、visual、ex、`u`/`Ctrl-R`。
   - **Esc 路由裁决**：vim 启用且 composer 聚焦且无 modal/palette 打开时，Insert 态 Esc → Normal；Normal 态 Esc → 保持既有全局 Esc 语义。modal/palette 打开时 modal 优先，不变。
   - 模式指示：`render_composer`（`tui_ui.rs` L5932–6108）在 vim 启用时渲染 `-- NORMAL -- / -- INSERT --` 指示（含 zh/en）。
2. **Rust slash 与 retained 往返**：`RetainedCommandPurpose` 增 `VimToggle`/`BriefToggle`（`retained_command_surface.rs`），`handle_local_command` 增 `"vim"`/`"brief"` 分支发 `SendPrivateRuntimeAction`；`retained.vim.updated` 应用 enabled/mode（开启即 Insert 态），`retained.brief.updated` 写状态文案（含 reminder_injected 事实）；`retained_command_error`（如 brief `not_entitled`/`command_unavailable`）原样呈现为错误终态。
3. **初始状态**：扩展 `retained.identity.snapshot` result schema（TS L87–93 与 Rust 解析、`contracts/renderer-protocol/v1/renderer-contract.json` 三处同步）为**必填**新字段 `editor_mode: 'normal'|'vim'`、`brief_enabled: boolean`（私有协议，双侧同 PR 更新，不留兼容垫）。TS 从 `getGlobalConfig().editorMode` 与 brief 权威取值。再生成：`bun scripts/generate-renderer-capability-contract.mjs --write`，门禁 `bun run check:capabilities`。
4. 合同 8 步 ×2（vim=参考 token interactive 组；brief=扩展 token）。新 profile：`renderer_vim`、`renderer_brief`。cancellation 维：toggle 原子 → `not_applicable`；persistenceRecovery：vim 经 identity snapshot 恢复模式（写测试证据）。
5. 测试：Rust——模式转换矩阵、每个 Normal 动作至少一断言、Esc 路由三场景（无 modal Insert/无 modal Normal/有 modal）、toggle 往返、error 终态、snapshot 恢复；Bun——`tests/unit/direct-tui-retained-command-actions.test.ts` 扩 vim/brief 真实 handler 用例（外部权威缝：global config 读写 mock）。
- 关联方：`accept_selected_command`/补全不受影响（vim 仅作用于 composer 键路由）；`FIXED_LOCAL_COMMAND_COMPLETIONS` 增两行；brief 在默认旗标下 TS 会拒绝（KAIROS 未开）→ 渲染层如实呈现错误，这是诚实终态不是缺陷；`tests/unit/slash-command-route-policy.test.ts` 中 vim/brief 的 fail-closed 断言改为 renderer-owned 断言。

## 6. W2 设置面 `/settings` `/config`

1. **TS 权威动作**（retained 车道，`directTuiRetainedCommandActions.ts` 扩展）：
   - `retained.settings.snapshot` → result `retained.settings.snapshot`：`{ sources: [{source, path, exists, json: string|null}], effective: { small_model: string|null, output_style: string|null } }`。实现用 `getSettingsForSource`/`getSettingsFilePathForSource`（`src/utils/settings/settings.ts` L276–319）。`json` 为整文件序列化，**每 source 截断上限 8192 UTF-16 单元**（presentation 字符串，按 08-20 规则可安全截断并替换孤立 surrogate），截断加 `…(truncated)` 尾标。
   - `retained.settings.update`：`{ key: 'smallModel'|'outputStyle', value: string|null }` → `updateSettingsForSource('userSettings', …)`；`smallModel` 非 null 时先过 `validateModel`；`value: null` 表清除键；写失败 → `retained_command_error{code: authority_failure}`；非法值 → `invalid_argument`。result `retained.settings.updated{key, value}`。
2. **Rust 设置面**：新模块 `crates/crabcode-tui/src/settings_management.rs`，照抄 `model_management.rs` 模式（私有 View 枚举 + Effect + `ActiveModal` 变体）。View：`Overview`（生效值 + source 列表 + 路径）、`SourceJson(source)`（只读滚动）、`EditSmallModel`、`EditOutputStyle`。打开函数 `open_settings_management`（模仿 `open_model_management` L4727–4742）。挂接既有 `CrabcodeKeybindingContext::Settings`（`crabcode_keybindings.rs` L32，当前未挂载表面）。
3. `/settings`、`/config` 都在 `handle_local_command` 打开同一面板。
4. `/output-style` 收尾：`src/commands/output-style/output-style.tsx` L2–5 废弃文案改为指向 `/settings`（`Use /settings to change outputStyle; changes take effect on the next session.`），消除死胡同。
5. 合同 8 步 ×2，新 profile `renderer_settings_surface`（settings、config 同 profile）。desktop 组 12→10。
6. 协议关联方：retained schema 变更 → renderer-contract 再生成 + `check:capabilities`；snapshot 帧受 `NATIVE_TUI_MAX_FRAME_BYTES` 约束（截断规则已定）。
7. 测试：Bun——snapshot/update 真实 handler（fs 权威缝 mock）、截断行为、校验失败终态；Rust——面板打开/导航/编辑提交/错误呈现/Esc 关闭；production smoke 增 settings 用例。

## 7. W3 会话面 `/resume` `/session` `/continue`

**已就绪**：渲染层 `SessionPickerComponent`（`session_picker.rs`，动作 Select/Preview/Rename/LoadMore/Reload/Cancelled）+ `InitialSessionRequest::{ResumePicker,ResumeExact}`（`tui_app.rs` L412–426）；TS 会话列表/加载权威（`sessionStorage-list.ts` 的 `loadSameRepoMessageLogsProgressive`/`loadAllProjectsMessageLogsProgressive`/`enrichLogs`/`loadFullLog`、`sessionLoader.ts`/`conversationRecovery` 的 `loadConversationForResume`）；启动 picker 桥 `directTuiSessionPicker.ts`（setup 阶段，运行中不可复用其车道，但 dependencies 全可复用）。

1. **新私有 StructuredIO control ×2**（TS `structuredIO.ts` + `sdkControlHandlers.ts` 注册；命名沿用 `crabcode_tui_*` 私有前缀；**不进**公开 SDK control union）：
   - `crabcode_tui_session_list`：请求 `{search: string|null, cursor: number|null, all_projects: boolean}` → 响应 `{entries: CrabCodeTuiSessionPickerEntry[], next_cursor: number|null}`（entry 复用 `crabcodeTuiBridgeProtocol.ts` 的 `CrabCodeTuiSessionPickerEntrySchema` 形状）。
   - `crabcode_tui_session_resume`：请求 `{session_id: string|null, latest: boolean}`（二选一：uuid 精确 / latest=true 取同仓最近会话，TS 裁决"最近"，渲染层不扫磁盘）。TS 侧：若存在 in-flight turn → 错误 `session_switch_busy`（诚实拒绝，提示先取消）；否则 `loadConversationForResume` → 切换会话身份 → 响应携带 `{session_id, transcript: SerializedMessage[]}`（投影与 initialize restore 载荷同构）。
2. **Rust**：`SessionPickerComponent` 增运行中入口（新 `ActiveModal` 变体）；数据经上述 list control 分页拉取；Select → resume control；成功响应 → 复用 `reset_renderer_session_projection_state`（L9920–10021，即后端 `/clear` 边界同款清理）+ 用 initialize 同款 transcript 解析路径重建；**composer 草稿清空**（明确行为，写测试）。Rename 动作复用 entry 上的既有 `saveCustomTitle` 权威（经 list control 的 rename 子请求或复用 picker 协议既有 Rename 语义，实施者取现成缝，禁止新造第二套改名逻辑）。
3. **slash 语义**：`/resume`（无参）→ picker；`/resume <uuid>` → 直接 resume；`/resume <文本>` → picker 带 initial_search（与启动 bare/title `--resume` 同语义）；`/continue` → `latest: true` 直接 resume；`/session` → picker 浏览模式（当前会话置顶标注，选当前=关闭）。
4. 合同 8 步 ×3，新 profile `renderer_session_navigation`。session_history 组 11→8。cancellation 维：picker Esc = 真实取消证据；resume 请求本身原子。
5. 协议关联方：新 control schema → `src/types` + renderer-contract 再生成 + `check:capabilities`；帧大小受限 → entries 分页（单页 ≤ 32 条）。
6. 测试：Bun——list 分页/搜索/busy 拒绝/latest 裁决/transcript 载荷同构断言（真实 handler，fs 缝 mock）；Rust——picker 打开/分页/取消/切换后 transcript 重建与会话身份替换/composer 清空；`bun run smoke:tui` 扩一条 resume 冒烟。

## 8. W4 `/permissions` `/plan`

1. **`/plan`**（渲染层映射既有公开 control，零新协议）：`handle_local_command` 增 `"plan"`：无参 → `SendControl set_permission_mode{mode:'plan'}`（schema 在 `controlSchemas.ts` L151–157）；参数 `default|off|exit` → `mode:'default'`；其他参数 → 本地用法提示。成功响应 → 瞬时状态文案（`已进入 plan 模式` / `已退出 plan 模式`）。**无常驻徽标**（R10）。
2. **`/permissions`**：
   - TS retained 动作 ×3：`retained.permissions.snapshot` → `{mode, rules: [{source, behavior: allow|deny|ask, value}]}`（权威：`loadAllPermissionRulesFromDisk` + `AppState.toolPermissionContext`，类型 `src/types/permissions.ts` L16–79）；`retained.permissions.add{behavior, value, source}` → `addPermissionRulesToSettings` + `applyPermissionUpdate`（会话态同步）；`retained.permissions.remove{behavior, value, source}` → `deletePermissionRuleFromSettings` + `applyPermissionUpdate`（`permissionsLoader.ts` 写路径 L230–297）。source 限 `userSettings|projectSettings|localSettings`（用 `getSettingSourceName` 常量）。
   - Rust 面板：新模块 `permissions_management.rs`（模式同 W2）。View：`RuleList`（按 behavior 分组、显 source）、`AddRule`（behavior 选择 + pattern 输入 + source 选择）、`ConfirmRemove`。
3. 合同 8 步 ×2，新 profile `renderer_permissions_surface`、`renderer_plan_mode`。permission 组 9→7。
4. 关联方：`set_permission_mode` 是公开 control，本 Wave 只是渲染层新增调用方，schema 零改动；permissions retained schema → contract 再生成；写失败 → `authority_failure` 真实错误。
5. 测试：Bun——三动作真实 handler（settings fs 缝 mock）、非法 source/pattern 拒绝；Rust——面板增删查、plan 模式切换往返与用法提示。

## 9. W5 `/skills` 清单面

1. TS retained 动作：`retained.skills.snapshot` → `{skills: [{name, origin: 'bundled'|'directory'|'plugin'|'dynamic'|'workflow', description: string|null, path: string|null}]}`。权威：`getBundledSkills()`（`src/skills/bundledSkills.ts` L54–109）、`getSkillDirCommands(cwd)`（`loadSkillsDir.ts` L637+）、`getPluginSkills()`/`getDynamicSkills()`、bundled workflow 面（`getWorkflowCommands`，`createWorkflowCommand.ts` L21–55，`isWorkflowRuntimeEnabled()` 为真才含）。description 截断 512 UTF-16 单元。
2. Rust：`PickerState` 底座列表面板（模仿模型 picker，`picker_surface.rs`）；Enter → 把 `/{name} ` 预填 composer 并关面板（**不代执行**，R11）；面板明确标注 origin。
3. 合同 8 步 ×1，新 profile `renderer_skills_inventory`。account 组 7→6。
4. 关联方：解决审计 §5.3.5（`/skills` fail-closed 但 skill 可用的矛盾）；skill 执行链路零改动。
5. 测试：Bun——snapshot 聚合各 origin（含 DEEP_RESEARCH 开/关差异）；Rust——面板打开/过滤/预填/取消。

## 10. 每 Wave 统一门禁（全部必须绿）

```bash
bun run typecheck
bun test <本 Wave 定向文件>
bun scripts/verify-direct-tui-command-capabilities.mjs --require-complete   # status=verified
bun run check:capabilities            # 凡协议/schema 变更的 Wave（W1/W2/W3/W4/W5）
cargo fmt --manifest-path crates/Cargo.toml --all -- --check
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path crates/Cargo.toml -p crabcode-tui --lib --quiet
cargo test --locked --manifest-path libs/acosmi-memory/Cargo.toml --workspace   # 仅 W0.1
bun run test && bun run check && bun run smoke:tui   # 每 Wave 收尾全量
```

沙箱注意（08-20 已证）：cargo 证据须在完整权限下执行，沙箱把 target 指到不可写缓存不算产品失败；Bun 子进程 stdout flush 有时序，父测试须有界等待完整 JSON 帧（用现成 `tests/helpers/readLastJsonEvidence.ts`）。

## 11. 实施顺序与依赖

1. W0（W0.1 可与 W0.2–W0.5 拆两个 PR：不同 workspace）。
2. W1 → W2 → W3 → W4 → W5 顺序执行；仅 W2 依赖 W0.5（同区域文案改动避免冲突），其余互相独立但按序合入以免合同 invariants 连环冲突。
3. 每 PR 自带合同增量 + 证据 + 门禁绿；禁止跨 Wave 攒大 PR。
4. 全部完成后补一份落地记录到 `audits/`（对齐 08-20 文风），更新分母表与验证记录。
