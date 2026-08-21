# TUI 命令健康审计、决策与落地记录（2026-08-20）

## 1. 最终结论

“TUI 中大部分命令无法使用”不是单点解析器回归，而是四类问题叠加：

1. **产品能力主动收缩**：参考集合 96 个 token 中仅 18 个由 Rust renderer 拥有、24 个由 TypeScript runtime catalog 拥有、54 个明确 fail-closed；加上本项目 8 个扩展 token 后，完整分母为 104，即 19 个 renderer-local、25 个 runtime-catalog、60 个 fail-closed。因此“多数不可用”的第一层事实是尚未移植，不是随机失灵。
2. **25 个 runtime token 的共同物理输入缺陷**：逐字输入 `/compact` 等精确 token 后，旧实现第一次 Enter 只补尾空格，第二次 Enter 才提交。用户观察到的“命令没反应”由此集中放大。
3. **发现、执行与刷新事实分叉**：禁用开关只清启动目录；插件、skill、认证和 MCP 刷新可以重新装回命令。catalog token owner、实际 dispatcher、live MCP inventory、settings/auth producer 也存在不同步窗口。
4. **证据门禁曾把共享路径冒充命令业务证据**：旧 runtime-general matrix 用替换 handler 伪造 19 个命令的成功、失败和取消；校验器又只看部分测试文件退出码，存在零测试、skip 或陈旧 fixture 假绿空间。

本次已修复可由现有权威链路证明正确的缺陷，并把没有实现的 60 个 token 保持为诚实 fail-closed。没有用统一转发、假成功或新增宽泛协议来伪造“全量 parity”。

## 2. 远端与工作树基线

- 仓库：`CrabCode-TUI`
- 分支：`main`，跟踪 `upstream/main`
- 开始时执行：`git pull --ff-only upstream main`，结果 `Already up to date`
- 交付前再次执行：`git fetch upstream main`
- `HEAD`、`upstream/main`、`FETCH_HEAD`：均为 `8b19ca34099158cfcf63a23c1bde3a4ad57fbda6`
- `git rev-list --left-right --count HEAD...upstream/main`：`0 0`

所以本轮没有可拉取的新远程提交。邻接 `../CrabCode` 是参考仓库，不是当前仓库的远端提交来源。

开始前已经存在的 `README.md`、`README.en.md`、`scripts/install.sh` 本地改动属于用户，本轮未覆盖、未回退，也不作为本次实现成果申领。

## 3. 健康定义与完整分母

本轮不把“目录里有同名字符串”当成健康。一个 token 只有满足以下一种事实才算健康：

- 有唯一执行 owner，发现、解析、提交、成功、失败、取消、终态、呈现和 TUI transcript 恢复与该 owner 一致；或
- 明确不可用，并在 renderer 边界 fail-closed：不误发给 backend、不退化成普通模型 prompt、不产生假成功。

最终合同覆盖：

| 项目 | 数量 |
|---|---:|
| 参考 token | 96（18 renderer / 24 runtime / 54 fail-closed） |
| 本项目扩展 token | 8 |
| 完整 token 分母 | 104（19 renderer / 25 runtime / 60 fail-closed） |
| 生命周期 profile | 21 |
| 每 profile 维度 | 9 |
| coverage cells | 936 |

最终严格门禁结果为 706 verified、230 明确 not-applicable、0 shared-only、0 unverified。N/A 表示该 owner 根本没有相应阶段，例如原子 handler 没有 renderer-owned 中途取消点；不是缺测的别名。

## 4. 前置代码审计与根因

### 4.1 精确 runtime token 被 palette 抢占

Rust `handle_key_event` 在 palette 打开时优先消费 Enter；旧 `accept_selected_command` 对 runtime-owned entry 无条件只写回 `/{name} `。即使 composer 已逐字等于 `/compact`，首次 Enter 也不会产生 `HostAction::SendUser`。

提交历史表明该策略从首发提交 `d69170a18f20f4eb3a28bbd50a02be279ed1934c` 即存在，不是近期远端回归。旧测试还把“双 Enter”固化为预期，而 strict aggregate 没调用物理 Enter 测试。

`argumentHint` 只是展示元数据，不是参数必填合同。参数合法性属于现有 backend parser/handler；renderer 用它阻止提交既不能验证参数，也没有安全增益。

### 4.2 slash 禁用事实不持久

旧 `--disable-slash-commands` 只清空初始目录，后续 loader 仍可发现命令，每轮执行还会合并 `AppState.mcp.commands`。插件安装、skill watcher、reload、login/logout 或 MCP 晚连接都可能重新恢复执行能力。

### 4.3 catalog owner 与真实 dispatcher 不一致

真实 dispatcher 按数组顺序 first-wins，并接受 canonical、alias，以及部分历史 `userFacingName`。旧 projector 先过滤 model-only，再只投 canonical/alias，导致目录可能把 token 广告给后一个 visible command，但实际派发被前置 model-only alias 或 legacy friendly token 抢占。

真实 inventory 中存在 UUID canonical + 友好 skill token，也存在 `mcp__server__prompt` + `server:prompt (MCP)`。目录与 dispatcher 必须共享同一 routable-name 事实，但 display-only `skillInterface.displayName` 不能被提升为路由 token。

### 4.4 live MCP inventory 与目录分叉

旧 direct runtime 把启动 MCP 命令快照混入 `currentCommands`：晚连接命令能执行却不可发现，断开后的初始命令又可能继续残留；plugin/skill/auth refresh 还会用 base registry 覆盖目录。

process MCP dynamic reconciler 只同步 clients/tools，不同步 prompt commands；add 缺失、remove/replace 残留。它从空 state 启动，第一次 plugin diff 也不能正确识别启动 server 的 removal。resources、loaded-from-MCP skill、scope/provenance和 normalized wire namespace同样存在陈旧或串 owner 风险。

### 4.5 MCP owner、策略与并发不闭合

实际代码存在三套不同 desired authority：public `mcp_set_servers`、plugin/settings inventory、启动固定 owner（CLI、IDE、Acosmi）。旧实现把 public/plugin 共用一个 desired set，任一方全量替换都会删除另一方。raw name 不同但 wire namespace 相同（如 `foo bar` 与 `foo.bar`）时，tools/prompts 的 prefix cleanup 还会跨 owner 擦除。

进一步实测发现：

- logical desired 与 executable state 混在一起，会让 disabled/policy-blocked owner被拒绝的 SDK 请求意外复活或删除；
- late IDE 在 mutation lane 外连接，可能与 public/plugin/settings 同时抢 namespace；
- settings 在 lane 外抓取 public desired，会把排队提交的 v2 回滚成旧 v1；
- 启动前已持久禁用的 public desired 虽不连接，但若未同步内存禁用事实，随后 reconnect 可越权复活；
- reconnect/toggle/auth/clear/OAuth 若不在同一 lane 内重读 owner、policy、disabled 与 endpoint authority，会提交旧世代结果；
- clear-auth 若用全局同名 fresh resolver，可能把另一个 owner 的配置写回当前 owner。

这些都由真实 StructuredIO control loop 场景复现后才实施修复，没有依据静态猜测改状态机。

### 4.6 direct SDK MCP 的能力边界

Native direct TUI 的 Rust host 不支持 reverse SDK `mcp_message`。因此 direct route 不能诚实地创建 SDK-transport MCP client，也不能假装 reconnect/toggle/auth 成功。

最终决策是：**direct TUI 对 `type: 'sdk'` 的启动、initialize、mcp_set、reconnect、toggle、authenticate、clear-auth 和 management-only inventory 全部在任何持久化、cleanup、连接或 AppState mutation 前 fail-closed**。process transport 继续闭环；standard SDK 保留既有 SDK transport 能力。

这取代了早期“给 direct SDK transport 同步 prompts”的方案；最终代码与本文均不再声称 direct native 支持 SDK MCP。

### 4.7 settings、auth 与 reverse publication 时序

settings watcher 过去只更新 AppState，不清命令 memoization、不刷新 catalog，也不重验 fixed/public/plugin MCP policy。startup remote settings 的 internal write 又可能不发 watcher 通知。

login/logout/reload 在 correlated response 与 reverse catalog refresh 之间也有窗口；单 microtask 延迟不足以覆盖 async owner。若 reverse ACK lane失败，Rust 目录可能长期陈旧。

### 4.8 private catalog wire 与 UTF-16 边界

旧 projector 不约束 private protocol 的 512/16384/4096 UTF-16 limits 和 4096 entries。单个过长动态 row会拒绝整次 refresh。未配对 UTF-16 surrogate 在 JS schema中可通过，但 Rust `serde_json` 无法等值解析，可破坏整个控制帧。

identity token 不能截断；不合法 token 必须 omit。description/hint 是 presentation，可替换孤立 surrogate并安全截断。schema还必须二次校验，防止绕过 projector。

### 4.9 命令业务证据不诚实及实际 handler 缺陷

旧 runtime-general matrix 绑定真实 command object 后替换其 handler，用固定 marker模拟成功、失败与取消，再把这份共享 executor 证据标成 19 个命令的业务生命周期证据。真实模块 import、参数、外部 authority 和副作用可全部损坏而 strict 仍绿。

真实 handler smoke 后又发现：

- `advisor`、`smallmodel` 忽略 settings 写入错误，仍宣称 set/reset 成功；`advisor` 还先改 AppState，造成内存/磁盘分叉；
- `/insights` 不接收 abort context，旧 matrix 的取消是替换 handler 主动抛错伪造的；
- `/init` 会读 onboarding filesystem/config，并可能写 project config，不能归为无失败面的纯 prompt builder。

### 4.10 verifier 与 fixture 可靠性

- 普通 gate 曾强依赖可选邻接仓当前 HEAD/脏工作树；正确对象应是 immutable pinned Git blob。
- Rust aggregate/terminal runner只用 substring filter和退出码时，0 tests 或 ignored 也可能通过。
- Bun evidence只看文件 exit 0时，声明测试可被 skip、todo 或无关测试顶数。
- Bun 1.3.x 子进程 stdout pipe/文件 flush存在本机可复现时序，父测试必须在 cleanup 前有界等待完整 newline JSON frame。

## 5. 决策：正当性与过度工程

### 5.1 通过并实施

- 仅当 composer 原文精确等于当前 runtime entry 的 `/{name}` 且为普通 Enter时，复用既有事务提交；partial、Tab、带参数输入保持原语义。
- 参数错误交给现有 backend handler，不在 Rust 新造语法验证。
- slash enable/disable 成为 route-owned fact，统一覆盖 loader、初始 inventory、每轮 MCP 合并和 catalog publisher。
- direct catalog 使用真实 routable invocation identity、backend first-wins、stable canonical-name dedupe和 renderer-private保留集合。
- standard catalog/unknown friendly MCP 保持既有语义；direct private projector/unknown policy独立。
- process MCP 在 direct route同步 prompts，按 fixed/public/plugin 分 owner，并按 raw name + normalized wire namespace fail-closed。
- logical desired 与 executable state分离；disabled/policy-blocked owner继续占 namespace但不连接，可在权威允许后按最新配置恢复。
- settings/control/OAuth/clear-auth/late IDE统一进入单一 FIFO mutation lane，并在 lane 内重读 authority。
- direct SDK transport全入口明确 fail-closed；不产生副作用。
- auth/reload/logout使用 publisher hold，先排 correlated response，再发布完整 live catalog。
- catalog逐 row隔离 wire limits和无效 UTF-16，坏 row不拖垮整表。
- 普通 reference gate读 pinned Git blob；显式 live模式才要求 pinned HEAD + clean worktree并动态执行。
- strict runner验证精确测试名、恰好一次 pass、零 ignore/skip/todo，并提供 missing/skip mutation seams。
- 19 个 production definitions真实 lazy-load；实际 handler只在网络、浏览器、shell、模型、持久化等外部 authority边界 mock。
- 修复 `advisor`/`smallmodel` 写入错误终态，给 `/insights` 增加真实阶段边界取消。

### 5.2 明确拒绝

- 不批量转发或“启用”60 个 fail-closed token；它们需要各自 UI、权限、取消和恢复状态机。
- 不新增 `requiresArguments`；布尔元数据无法表达现有自定义解析器。
- 不为 runtime 命令再造第二套 dispatcher。
- 不删除 legacy `userFacingName` 路由，也不让 visible owner越过前置 model-only first-wins owner。
- 不把 display-only `skillInterface.displayName` 变成调用 token。
- 不给 direct native伪造 SDK MCP reverse host、异步假成功或 session tombstone协议。
- 不给 standard SDK暗中扩张 direct-only catalog/MCP prompt语义。
- 不新增 catalog generation/ACK协议；现有 correlated response + publisher hold足以闭合时序。
- 不靠更新 reference pin、忽略 hash、动态 import邻接脏工作树来“修绿”。
- 不引入 AST runner或为每个 Rust marker启动独立进程；精确 test path + aggregate membership + mutation seam已足够。
- 不把外部 OS/网络/存储内部状态恢复冒充 renderer transcript recovery。

## 6. 实施路径

| 范围 | 核心路径 |
|---|---|
| 精确 Enter、disabled与可执行 Rust evidence | `crates/crabcode-tui/src/tui_app.rs` |
| CLI flag wiring evidence | `crates/crabcode-tui/src/terminal.rs` |
| direct/standard catalog、invocation identity、UTF-16/wire limits | `src/cli/commandCatalogProjection.ts`, `src/cli/directTuiCommandCatalogRefresh.ts`, `src/types/command.ts` |
| route禁用、live inventory、auth/settings publisher | `src/cli/print/slashCommandRoutePolicy.ts`, `src/cli/print/queryExecutionCore.ts`, `src/cli/print/sdkControlHandlers.ts` |
| MCP startup/owner/reconcile/auth | `src/cli/tuiRuntimeBootstrap.ts`, `src/cli/print/mcpServerOwnership.ts`, `src/cli/print/mcpServerManagement.ts`, `src/services/mcp/mcpAuthClearRuntime.ts` |
| unknown friendly MCP direct-only策略 | `src/QueryEngine.ts`, `src/utils/processUserInput/processSlashCommandCore.ts`, `src/utils/processUserInput/processUserInputCore.ts` |
| 真实 handler修复 | `src/commands/advisor.ts`, `src/commands/smallmodel/smallmodel.ts`, `src/commands/insights/index.ts` |
| 生命周期合同与严格 runner | `contracts/direct-tui-command-capabilities/v1/command-capabilities.json`, `scripts/verify-direct-tui-command-capabilities.mjs` |
| 真实 StructuredIO MCP evidence | `tests/fixtures/direct-tui-mcp-control-runtime.ts`, `tests/unit/direct-tui-mcp-control-runtime.test.ts` |
| 19 命令 production smoke | `tests/fixtures/direct-tui-runtime-command-production-smoke.ts`, `tests/unit/direct-tui-runtime-command-production-smoke.test.ts` |
| 子进程证据可靠读取 | `tests/helpers/readLastJsonEvidence.ts` 及相关 fixture父测试 |

## 7. 关联影响复核

- **canonical/alias/legacy token**：实际 dispatcher与direct目录共享 routable-name helper；first-wins不变，display-only名称仍不可调用。
- **renderer private**：`bug|feedback|logout|reload-plugins` 的 canonical/alias/friendly collision在backend inventory前排除。
- **runtime collision**：非保留 local command仍允许 runtime first-wins；精确 `/help` collision发送 backend，不误开本地 help。
- **attachments**：精确 slash + 图片继续走 `submit_composer_with_priority`；发送失败恢复、附件事务和TS text-last解析顺序保持。
- **partial/Tab/参数**：partial与Tab只补全；精确裸 token提交给backend权威校验；有参数输入按普通提交路径。
- **slash disabled**：后续 plugin/skill/auth/MCP刷新不能恢复目录或执行；维持既有 `Unknown skill`终态。
- **catalog**：direct strict projector与standard legacy projector分开；standard friendly MCP未知输入仍保留历史 fallthrough。
- **MCP inventory**：direct current registry只存base，live AppState commands在读边界合并并stable-name去重；disconnect/reload不会留下初始快照。
- **MCP process owner**：fixed/public/plugin desired互不删除；inactive logical owner仍保留namespace；晚IDE也在mutation lane内重新admit。
- **MCP disable/policy**：启动、运行时settings、bare模式、public更新与reconnect/OAuth均重验权威；拒绝的SDK/跨owner/policy输入不会清别人的disable marker。
- **MCP cleanup**：remove/replace使用connect-free eviction，避免cache miss为了“清理”新建连接；clients/tools/prompts/MCP skill/resources旧投影按owner清理。reconcile只清陈旧resource snapshot，不伪造资源fetch。
- **direct SDK MCP**：任何入口均错误终态、零mutation；standard SDK既有transport路径不套该guard。
- **standard route**：catalog、unknown-friendly-MCP和SDK transport语义不扩张；共享remove路径改为connect-free eviction属于资源正确性修复，不新增能力。
- **auth/settings**：correlated response携带当下完整目录；async producer在release hold后再reverse publish；startup remote settings即使无watcher事件也会显式进入refresh。
- **业务命令**：advisor成功持久化后才commit AppState；smallmodel失败不再假成功；insights在phase/batch边界真实取消；init onboarding写失败产生真实error terminal。
- **协议/权限**：没有新增公开wire subtype、自动批准权限或第二执行器。

## 8. 本机验证（未使用 CI）

环境：Bun `1.3.14`。所有结果均来自最终工作树的本机执行。

| 验证 | 结果 |
|---|---|
| 远端最终复核 | `HEAD == upstream/main == FETCH_HEAD`，ahead/behind `0/0` |
| `cargo fmt --manifest-path crates/Cargo.toml --all -- --check` | 通过 |
| `CARGO_INCREMENTAL=0 cargo test --locked --manifest-path crates/Cargo.toml -p crabcode-tui --lib --quiet` | 1306 passed，0 failed，0 ignored |
| 12 个关键 TS 文件联合定向矩阵 | 132 passed，0 failed，1127 assertions |
| 真实 StructuredIO MCP control | 8 passed，0 failed，74 assertions |
| production command smoke | 7 passed，0 failed，62 assertions |
| `bun run typecheck` | 通过 |
| `bun run format:check` | 1383 files checked，0 errors |
| lifecycle contract self-test | 4 passed，0 failed，46 assertions；含Rust/Bun missing、skip、zero-test mutation seams |
| strict verifier report | 104 tokens、21 profiles、936 cells；706 verified、230 N/A、0 shared-only、0 unverified；status verified |
| `bun run test` | 100/100 test files PASS |
| `bun run check` | build、4154-input runtime binding、strict command gate、typecheck、378-capability contract、repository boundary全部通过 |
| `bun run smoke:tui` | initialize成功；catalog refresh ACK；`/context`成功；compact空历史真实error；cost 2/2；end-session成功；stderr empty；exit 0 |
| `git diff --check` | 通过 |

`build:ts` 唯一提示是当前开发环境没有注入四个可选 Account Bridge 非密配置，因此仅该可选 connector入口会明确报未配置；Acosmi SDK直连OAuth与本次命令链不依赖这些值。该提示没有被误记为已验证能力。

全量第一次运行曾发现一条测试仍匹配旧源码 `getCrabCodeMcpConfigs(dynamicMcpConfig)`；生产代码已改用策略允许集。修正陈旧marker后，该文件19/19通过，随后整个100文件测试集从头重跑通过。

## 9. 剩余边界

- 60 个 fail-closed token仍是明确产品覆盖缺口。它们已进入104分母和严格合同，但功能没有被伪装成已移植。
- direct native TUI仍不支持SDK-transport MCP；若未来要支持，必须先实现Rust host可服务的reverse `mcp_message`生命周期，再开放相关入口。
- 原子外部authority命令没有renderer-owned中途取消协议，合同标N/A；外部OS、网络、浏览器、模型和存储内部恢复不属于TUI transcript owner。
- process MCP reconcile会清理陈旧resource snapshot，但不主动伪造resource fetch；资源重新出现仍由已有reconnect/fetch authority负责。
- 普通reference gate的`snapshot_only`表示校验immutable pinned blob而非动态执行参考仓；只有显式required模式、pinned HEAD且clean worktree时才允许live verify。
