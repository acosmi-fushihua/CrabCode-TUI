# Changelog / 更新日志

## v1.0.33 — 2026-08-11

- Forward-fixes the failed, unpublished `v1.0.32` tag without moving,
  deleting, or rerunning it. All source and native preflights passed; Windows
  then compiled, assembled, and verified its archive before the first packaged
  replay connected to a different Memory named-pipe hash from the runtime.
- Removes that duplicated endpoint derivation from the release harness. The
  private native replay now returns the exact launcher-selected Memory IPC
  endpoint, and the package test validates and consumes that runtime authority.
  This keeps Windows 8.3 aliases such as `RUNNER~1` from diverging from Rust's
  canonical long path such as `runneradmin`.
- Pins every packaged Bun runtime to 1.3.14 after a local archive replay proved
  that 1.3.11 cannot parse the explicit-resource-management syntax in the
  current minified runtime. The upgrade covers both Mac arm64 and Windows
  before the sole hosted native job is triggered.
- Reduces hosted release allocation to one Windows job after a cheap signed
  source gate. Both macOS archives are reproducibly cross-checked, replayed,
  and signed on the audited local Apple Silicon release host, published first,
  and the attested Windows archive is appended after its single hosted job
  succeeds. Linux remains outside the public release scope.

---

- 对失败且未公开的 `v1.0.32` 标签执行前滚修复，不移动、不删除、不重跑。源码与
  原生预检均已通过；Windows 完成编译、装配与归档校验后，第一次成品回放因测试
  脚本计算出的 Memory 命名管道哈希与运行时实际地址不同而失败。
- 删除发布测试夹具中重复实现的端点推导。私有原生回放现在直接返回 launcher 已
  选定的 Memory IPC 地址，成品测试只校验并使用该运行时权威值，避免 Windows
  `RUNNER~1` 等 8.3 短路径与 Rust 规范化后的 `runneradmin` 长路径产生不同哈希。
- 本地真实归档回放证明 Bun 1.3.11 无法解析当前压缩运行时中的显式资源管理语法，
  因此将所有发布包内 Bun 统一锁定到 1.3.14；升级同时覆盖 Mac arm64 与 Windows，
  并在唯一托管原生任务触发前完成验证。
- 托管发版资源收敛为低成本签名源码门禁之后的唯一 Windows job。两个 macOS 包
  均在已审计的本地 Apple Silicon 发版机上完成可复现装配、成品回放与签名并先行
  发布；Windows 包在唯一托管任务通过后追加。Linux 仍不属于公开发版范围。

---

## v1.0.32 — 2026-08-11

- Forward-fixes the failed, unpublished `v1.0.31` tag without moving,
  deleting, or rerunning it. Its signed-source and both native preflights
  passed; arm64 macOS then completed the native build before assembly rejected
  a stale Account Bridge lock pin, fail-fast cancelled the other two builds,
  and no Release or asset was published.
- Binds assembly and the embedded runtime to the official `acosmi/crabcode`
  `v1.0.29` Account Bridge `7.2.71-crabcode.9` assets, exact archive and lock
  hashes, production Ed25519 trust root, eligibility trust root, and the
  same-team Developer ID plus Apple notarization evidence actually carried by
  macOS, without requiring the broad library-validation disabling entitlement.
- Adds a cheap source-stage supply-chain gate that downloads and fully verifies
  the three public Account Bridge archives on Ubuntu before any macOS or
  Windows preflight or native build is allocated. It checks the official
  checksum manifest, archive hashes, exact repository lock, signed provenance,
  SBOM, license inventory, platform evidence, component version, and protocol.
- Archives the local CrabCode TUI logo and social-poster source/export set under
  `.github/assets/social/`; these repository-only materials are excluded from
  the compiled release closure.
- Public release scope remains macOS arm64, macOS x64, and Windows x64 only.

---

- 对失败且未公开的 `v1.0.31` 标签执行前滚修复，不移动、不删除、不重跑。其签名
  源码与两项原生预检均已通过；macOS arm64 完成原生构建后，装配因过期的 Account
  Bridge lock 固定值而拒绝继续，fail-fast 取消另外两项构建，没有发布 Release 或资产。
- 将装配流程与内嵌 runtime 统一绑定到官方 `acosmi/crabcode v1.0.29` 中的 Account
  Bridge `7.2.71-crabcode.9`：固定三平台归档哈希、lock 哈希、生产 Ed25519 信任根、
  eligibility 信任根，并严格接受 macOS 资产实际携带的同团队 Developer ID 与 Apple
  公证证据，不再要求扩大动态库加载范围的 library-validation 禁用 entitlement。
- 新增低成本源码阶段供应链门禁：在分配任何 macOS、Windows 预检或原生构建机前，
  先在 Ubuntu 下载并完整验证三个公开 Account Bridge 归档，覆盖官方 checksum 清单、
  归档哈希、仓库 lock 逐字一致、签名 provenance、SBOM、许可证清单、平台签名证据、
  组件版本和协议版本。静态漂移不再等到原生编译完成后才暴露。
- 将本地 CrabCode TUI 标识与社媒海报的源文件、导出文件归档到
  `.github/assets/social/`；这些仅供仓库使用的素材不会进入编译发布闭包。
- 公开发布范围不变：仍仅提供 macOS arm64、macOS x64 与 Windows x64。

---

## v1.0.31 — 2026-08-11

- Forward-fixes the failed, unpublished `v1.0.30` tag without moving,
  deleting, or rerunning it. The corrected release identity passed; its sole
  canonical workflow then stopped before allocating the three-platform build
  matrix because two release consumers still expected the retired `turns`
  smoke-report field. No Release or asset was published.
- Replaces both stale field checks with one shared, unit-tested runtime-smoke
  assertion over `rendererContext`, `initialize`, `costTurns`, `endSession`,
  and `exitCode`. The native Intel preflight and every packaged-platform replay
  now consume the same report contract, preventing another post-build failure.
- Product behavior and public scope remain unchanged: macOS arm64, macOS x64,
  and Windows x64 are the only release archives.

---

- 对失败且未公开的 `v1.0.30` 标签执行前滚修复，不移动、不删除、不重跑。修正后
  的发布身份绑定已通过；其唯一企业 workflow 随后因两个发布消费者仍读取已废弃
  的 `turns` 冒烟报告字段，在分配三平台构建矩阵前停止，没有发布 Release 或资产。
- 将两处旧字段判断统一替换为一个带单测的 runtime-smoke 断言，共同校验
  `rendererContext`、`initialize`、`costTurns`、`endSession` 与 `exitCode`。
  原生 Intel 预检和每个平台的打包回放现共享同一报告契约，避免在构建后再次失败。
- 产品行为与公开范围不变：发布包仍仅包含 macOS arm64、macOS x64 与 Windows x64。

---

## v1.0.30 — 2026-08-11

- Forward-fixes the failed, unpublished `v1.0.29` tag without moving,
  deleting, or rerunning it. Its single workflow passed signed-source,
  Windows process-observer, command-lifecycle, terminal-lifecycle, and source
  boundary gates, then stopped before allocating the three-platform build
  matrix; no Release or asset was published.
- Completes the x64 macOS preflight's synthetic `release-materials.json` with
  the exact release `version` and `buildId`. The preflight now also binds both
  values to the built runtime metadata before exercising the pinned Bun
  runtime, preserving the fail-closed identity check that blocked `v1.0.29`.
- Product behavior and release scope are otherwise unchanged: the public
  matrix still contains only macOS arm64, macOS x64, and Windows x64.

---

- 对失败且未公开的 `v1.0.29` 标签执行前滚修复，不移动、不删除、不重跑。
  其唯一一次 workflow 已通过签名源码、Windows 进程观察、命令生命周期、终端
  生命周期与源码边界门禁，随后在分配三平台构建矩阵前停止，没有发布 Release
  或任何资产。
- 为 x64 macOS 预检临时生成的 `release-materials.json` 补齐精确的发布
  `version` 与 `buildId`，并在执行固定 Bun runtime 前将两者与已构建 runtime
  元数据绑定；`v1.0.29` 所触发的 fail-closed 身份校验保持不变。
- 产品行为与发布范围均无其他变化：公开矩阵仍仅包含 macOS arm64、macOS x64
  与 Windows x64。

---

## v1.0.29 — 2026-08-11

- Forward-fixes the failed, unpublished `v1.0.28` tag without moving,
  deleting, rerunning, or publishing it. The Windows observer preflight now
  uses a minimal native Rust launcher with explicit inherited-stream ownership,
  bounded finalization, deterministic descendant cleanup, and all eight
  fail-closed process-contract assertions intact.
- Completes the authoritative direct-TUI command lifecycle. Runtime-provided
  catalogs can refresh in place, hidden and gated commands stay hidden,
  renderer/backend dispatch ownership is explicit, and logout, plugin reload,
  compact, context, local-model, and structured terminal results close cleanly.
- Adds the native multi-question `AskUserQuestion` experience with deterministic
  IDs, single/multi-select validation, recommended choices, bounded free text,
  previews, keyboard/mouse navigation, and telemetry-safe answers.
- Strengthens the direct permission bridge across TypeScript and Rust with
  allow-once, allow-for-session, persistent, and deny decisions; filesystem,
  shell, and PowerShell mutation checks now preserve configuration-home and
  workspace boundaries instead of broadening them implicitly.
- Hardens interrupt, compaction, context analysis, transcript persistence, and
  progress recovery so terminal state resumes deterministically after local
  commands, failed controls, and session-memory compaction.
- Rebinds the generated renderer contract to 378 reviewed capabilities, adds
  exact runtime-source and command-capability gates, exercises terminal
  lifecycle tests in CI/release preflight, and exposes the underlying Landlock
  reason in fail-closed diagnostics.
- Publishes only three native archives: macOS arm64, macOS x64, and Windows x64.
  Canonical-repository gating, release-only push suppression, concurrency
  cancellation, a three-platform incident matrix, and one artifact upload per
  build prevent duplicate personal/corporate runs and empty uploads.

---

- 对失败且未公开的 `v1.0.28` 标签执行前滚修复，不移动、不删除、不重跑，也不
  发布该标签。Windows observer 前置门禁改用最小原生 Rust launcher，显式持有继承
  流、限定最终化期限、确定性清理后代进程，并完整保留八项 fail-closed 契约断言。
- 补齐权威 direct-TUI 命令全生命周期：运行时命令目录可原位刷新，隐藏或受门禁
  命令不会泄漏，renderer/backend 分派归属明确，logout、插件重载、compact、
  context、local-model 与结构化终端结果均能闭环。
- 新增原生多问题 `AskUserQuestion` 交互，覆盖确定性 ID、单选/多选校验、推荐项、
  有界自由文本、预览、键鼠导航，以及不泄漏答案内容的遥测。
- 加固 TypeScript 与 Rust 之间的权限桥，统一单次允许、会话允许、持久允许和拒绝；
  文件系统、Shell 与 PowerShell 变更检测不会再隐式放宽配置目录和工作区边界。
- 加固中断、压缩、上下文分析、转录持久化与进度恢复，使本地命令、控制失败和
  session-memory 压缩之后的终端状态能够确定性续接。
- 将生成的 renderer 契约重新绑定到 378 项已审能力，新增 runtime 源码绑定与命令
  能力门禁，在 CI/发版预检中执行终端生命周期测试，并在 Landlock fail-closed
  诊断中保留底层失败原因。
- 正式发布仅生成三个原生包：macOS arm64、macOS x64 与 Windows x64。通过企业
  主仓门禁、release-only 推送抑制、并发取消、三平台事件回放矩阵和每构建一次
  artifact 上传，避免个人/企业仓重复执行及空上传。

---

## v1.0.28 — 2026-08-03

- Forward-fixes the failed, unpublished `v1.0.27` tag without moving,
  deleting, or rerunning it. Its only workflow run failed closed on the first
  Windows replay and published no Release or asset.
- Retains the successful launcher observation and its Windows process/pipe
  ownership until Memory promotion and package-process exit are complete.
  This preserves the stable Memory coordinator across the intervening runtime
  replay without restoring the former unbounded stdout/stderr wait.
- Finalizes deferred pipes only after the complete package lifecycle. EOF must
  then arrive within a separate deadline, and any late stdout/stderr data is
  rejected; failure cleanup cancels the lease and terminates only verified
  package-owned processes.
- Adds Memory log-tail and process-inventory evidence to lifecycle failures,
  plus a fast Windows preflight that proves deferred ownership before the
  five-platform build matrix is allocated.
- Product runtime, renderer protocol, SDK `2.15.0`, Gateway, Memory IPC,
  installers, package layout, and user configuration remain unchanged.

---

- 对失败且未公开的 `v1.0.27` 标签执行前滚修复，不移动、不删除、不重跑。
  其唯一一次 workflow 在 Windows 第一轮快速失败，没有发布 Release 或资产。
- launcher 成功返回完整契约后，无论 stdout/stderr 是否已 EOF，都将 Windows
  进程观察租约保留到 Memory 晋升确认与包进程退出完成；若后代仍持管道则同时保留
  pending readers。由此跨过中间 runtime 回放继续保持稳定 Memory coordinator，
  同时不恢复旧版无界等待。
- 完整包生命周期结束后才收束延迟管道；此时必须在独立期限内得到 EOF，任何晚到的
  stdout/stderr 都会失败。异常路径取消租约，并只终止经包路径所有权验证的进程。
- Memory 生命周期错误新增日志尾部和进程清单证据；新增快速 Windows 前置门禁，先
  证明延迟所有权语义，再分配五平台构建矩阵，避免浪费 CI。
- 产品 runtime、renderer 协议、SDK `2.15.0`、Gateway、Memory IPC、安装器、包布局
  和用户配置均不改变。

---

## v1.0.27 — 2026-08-03

- Forward-fixes the failed, unpublished `v1.0.26` tag without moving, deleting,
  rerunning, or creating a Release for it. The single `v1.0.26` workflow run
  passed signed-source verification, the native Intel preflight, and all 100
  package replays on four platforms; Windows was cancelled at its 180-minute
  job limit before publishing any asset.
- Separates child execution deadlines from stdout/stderr drain deadlines in the
  release-package observer. The verifier can no longer block forever after a
  parent exits or is terminated while pipe EOF remains unavailable.
- On execution timeout, terminates the complete Windows process tree with
  `taskkill /T /F`, bounds the final pipe drain, then fails with captured output
  instead of waiting indefinitely.
- Permits a bounded open-pipe recovery only for the packaged launcher and only
  after exit code zero, a complete newline-terminated JSON contract, and empty
  stderr.
  The subsequent Memory identity, promotion, package-process exit, and isolation
  checks remain mandatory; every other open-pipe condition is fail-closed.
- Adds real parent/descendant process regressions for authorized inherited
  handles, unauthorized handles, and timeout cleanup, plus per-iteration start
  and completion evidence. Product runtime, protocol, SDK `2.15.0`, Gateway,
  installers, configuration, sessions, and package layout remain unchanged.

---

- 对失败且未公开的 `v1.0.26` 标签执行前滚修复；不移动、不删除、不重跑，也不为其
  补建 Release。唯一一次 `v1.0.26` 流水线已通过签名源码校验、原生 Intel 预检，
  并在四个平台跑满各 100 次正式包回放；Windows 在任何资产发布前达到 180 分钟
  作业上限并被取消。
- 将发布包观察器中的“子进程执行期限”和“stdout/stderr 排空期限”彻底分离。父进程
  已退出或被终止但管道 EOF 仍不可用时，验证器也不再无限阻塞。
- 执行超时时，Windows 使用 `taskkill /T /F` 终止完整进程树，再以独立期限排空并
  取消管道，最后携带已捕获输出失败，不再无界等待。
- 仅对正式包 launcher 接纳有界的未关闭管道恢复，而且必须同时满足退出码为零、
  stdout 是完整换行结尾 JSON 契约、stderr 为空；后续 Memory 身份、promotion、包内
  进程退出和隔离检查仍全部强制执行，其他未关闭管道仍全部 fail-closed。
- 新增真实父/后代进程回归，覆盖授权继承句柄、未授权句柄与超时回收，并逐轮记录
  开始/完成证据。产品 runtime、协议、SDK `2.15.0`、Gateway、安装器、配置、会话和
  包布局均不改变。

## v1.0.26 — 2026-08-03

- Forward-fixes the failed, unpublished `v1.0.25` tag without moving, deleting,
  rerunning, or creating Releases for `v1.0.23` through `v1.0.25`.
- Corrects the release-failure attribution. The runtime produced and flushed its
  initialize response; the smoke observer dropped it by starting a new
  `stdout.read()` after every one-second `Promise.race` timeout while the old
  read remained alive and consumed the next chunk.
- Reuses exactly one pending stream read across poll timeouts. A focused
  regression test forces three consecutive timeouts, proves that `read()` was
  called once, and then verifies delivery of the delayed frame and recovery
  after a rejected read.
- Reverted every diagnostic-only runtime experiment after proving the observer
  defect. Product protocol, TypeScript runtime, Rust renderer, Gateway, SDK
  `2.15.0`, configuration, sessions, and package layout remain unchanged.
- Preserves the native Intel preflight ahead of the five-platform matrix and the
  reviewed x64 macOS Bun `1.3.14` baseline identity. The final minimal fix
  completed `100/100` supplemental Rosetta lifecycles with zero process or
  temporary-directory leakage; native Intel CI remains the release authority.

---

- 对失败且未公开的 `v1.0.25` 标签执行前滚修复；不移动、不删除、不重跑
  `v1.0.23` 至 `v1.0.25`，也不为这些失败标签补建 Release。
- 纠正发布失败归因：runtime 实际已经生成并刷新 initialize 响应；smoke 观察器每次
  一秒 `Promise.race` 超时后又启动新的 `stdout.read()`，旧读取仍存活并吞掉下一块
  数据，导致观察端永久丢帧。
- 轮询超时后复用同一个未决读取，始终只允许一个 `read()`。专项回归测试强制连续
  三次超时，证明只调用一次读取，随后验证延迟帧不丢失以及读取失败后可恢复。
- 根因成立后撤回全部仅用于排障的 runtime 实验改动。产品协议、TypeScript runtime、
  Rust renderer、Gateway、SDK `2.15.0`、配置、会话和包布局均不改变。
- 保留五平台矩阵之前的原生 Intel 预检，以及已审查的 x64 macOS Bun `1.3.14`
  baseline 身份。最终最小修复完成 Rosetta 补充控制 `100/100`，进程与临时目录零
  泄漏；正式发布授权仍只来自原生 Intel CI。

## v1.0.25 — 2026-08-03

- This signed release attempt failed its native Intel preflight and has no
  GitHub Release or public assets. `v1.0.26` later proved that the preflight
  observer, not runtime module evaluation, dropped delayed stdout frames.
- Forward-fixes the failed, unpublished `v1.0.24` tag without moving or deleting
  that signed historical record. `v1.0.24` has no GitHub Release or public
  assets and must not be installed.
- Pins only the x64 macOS package to the reviewed Bun `1.3.14` baseline archive,
  including exact URL, archive size/SHA-256, executable size/SHA-256, license
  URL/SHA-256, and package-local release-material authority. All other platforms
  retain Bun `1.3.11`, avoiding an unnecessary cross-platform runtime change.
- Completes entry-module evaluation synchronously instead of suspending it with
  top-level await. The runtime promise still owns the full lifecycle and a
  rejected bootstrap remains process-fatal after stderr is flushed.
- Adds a native Intel macOS preflight that executes the complete bundled runtime
  through renderer context, initialize, two successor turns, and end-session
  before the five-platform build matrix is allocated. Ten consecutive preflight
  lifecycles are mandatory, and matrix fail-fast is enabled to conserve CI.
- Adds opt-in bounded macOS process sampling when the runtime lifecycle times out,
  preserving actionable module/runtime evidence without changing normal product
  behavior or successful smoke output.
- Removes hard-coded “current stable version” prose from both READMEs. The
  install command now follows only the public GitHub `latest` Release, preventing
  documentation from advertising a tag whose assets were never published.

---

- 此次已签名发布未通过原生 Intel 预检，没有 GitHub Release 或公开资产。
  `v1.0.26` 后续证明是预检观察器丢弃延迟 stdout 帧，并非 runtime 模块求值停滞。
- 对失败且未公开的 `v1.0.24` 标签执行前滚修复，不移动或删除该已签名历史记录。
  `v1.0.24` 没有 GitHub Release 或公开资产，不得安装。
- 仅将 x64 macOS 发布包切换到经审查的 Bun `1.3.14` baseline 归档，同时固定精确
  URL、归档大小/SHA-256、可执行文件大小/SHA-256、许可证 URL/SHA-256，以及包内
  发布材料授权；其他平台继续使用 Bun `1.3.11`，避免无必要的跨平台运行时变更。
- 入口模块改为同步完成求值，不再以 top-level await 悬挂模块；runtime promise
  仍拥有完整生命周期，bootstrap reject 在 stderr 刷出后仍以进程级失败结束。
- 新增原生 Intel macOS 发布预检：在分配五平台构建矩阵前，完整执行 renderer
  context、initialize、连续两轮与 end-session 生命周期，连续十次均成功才放行；
  同时启用矩阵 fail-fast，控制 CI 消耗。
- runtime 生命周期超时时可选择采集有界 macOS 进程样本，为模块/运行时故障保留
  可执行证据；正常产品行为和成功 smoke 输出均不改变。
- 中英文 README 不再硬编码“当前稳定版本”。安装命令只跟随公开 GitHub `latest`
  Release，杜绝文档提前宣传尚未产出资产的标签。

## v1.0.24 — 2026-08-03

- This signed release attempt failed on GitHub's native Intel macOS runner and
  was deliberately cancelled before the remaining matrix consumed more CI.
  It has no GitHub Release or public assets and is retained only as immutable
  failure evidence.
- Both Bun `1.3.11` standard and baseline executables could launch. This was
  initially attributed to the complete module graph stalling before the
  renderer handshake; `v1.0.26` disproved that attribution and identified
  overlapping timed-out smoke reads as the frame-loss mechanism.
- Forward-fixed the earlier unpublished `v1.0.23` release candidate without
  moving or deleting its signed tag.
- Classified only Bun's exact no-AVX compatibility warning, and only when the
  package-local release materials bind the expected platform, version, asset
  URL, and SHA-256. Every other stderr byte still fails the runtime smoke.
- Removed the unused `ffmpeg-static` dependency and its install-time network
  fetch. Media rendering continues to use the existing explicit system FFmpeg
  contract.
- Removed the Windows replay's process-per-system-PID inventory loop. One CIM
  snapshot is now name-prefiltered before canonical package-ownership checks;
  all 100 incident replays remain, with platform-specific CI budgets and
  progress every ten iterations.
- Corrected release evidence terminology: repeated assembly proves byte-level
  determinism for one compiled product closure, not independent-build
  reproducibility.

---

- 此次已签名发布在 GitHub 原生 Intel macOS runner 上失败；为避免其余矩阵继续
  消耗 CI，失败后已主动取消。该版本没有 GitHub Release 或公开资产，仅作为不可变
  的失败证据保留。
- Bun `1.3.11` 的 standard 与 baseline 可执行文件本身都能启动。当时曾归因为完整
  模块图在 renderer 握手前停滞；`v1.0.26` 已推翻该归因，并确认是 smoke 中多个
  已超时但仍存活的读取竞争并吞掉协议帧。
- 对更早的未公开 `v1.0.23` 候选版执行前滚修复，不移动或删除其已签名标签。
- 仅当包内发布材料同时绑定预期平台、版本、资产 URL 与 SHA-256 时，才分类接纳
  Bun 唯一一条精确的无 AVX 兼容提示；其他任何 stderr 字节仍使 runtime smoke 失败。
- 移除从未被源码引用、安装时还会额外联网取包的 `ffmpeg-static` 死依赖；媒体渲染
  继续遵循原有的显式系统 FFmpeg 契约。
- 移除 Windows 回放中“每个系统 PID 再启动一个 PowerShell”的进程盘点循环；改为
  单次 CIM 快照先按名称过滤，再做 canonical 包归属核验。每平台 100 次事故回放
  全部保留，另按平台设置 CI 时限并每十次输出进度。
- 纠正发布证据表述：重复组装只能证明同一编译闭包的字节级确定性，不能冒充独立
  构建之间的完整可复现性。

## v1.0.23 — 2026-08-02

- Fixed the P1 `sources: []` crash across Gateway, `@acosmi/sdk-ts 2.15.0`,
  TypeScript normalization, and the independent Rust renderer boundary.
  Empty sources are now a presentation no-op; malformed optional provenance is
  recoverable and cannot stop the runtime.
- Added a generated, versioned renderer event policy. Known broken turn
  lifecycle events interrupt only the correlated turn; only framing,
  correlation, control, permission, trust, and secret-boundary failures retain
  global fail-closed authority.
- Made runtime shutdown idempotent and preserved the primary renderer failure
  above cleanup errors.
- Replaced process-global diagnostics with explicit injection, bounded
  content-free shape metadata, strict raw redaction, and temporary test roots.
- Added exact 63-byte incident replay, two-turn package smoke, Memory promotion
  cleanup, real-state sentinels, and a five-platform 100-run CI/release matrix.
- Hardened overlong Unix state roots with private `0700` short supervisor
  namespaces and `0600` sockets while preserving every representable legacy
  path. Memory shutdown now removes only its exact socket inode and its own
  empty short namespace, preserving replacements and non-empty directories.
- Added offline installer + stable-generation launcher replay to every package
  gate, including zero process/socket/short-namespace leakage. macOS package
  process inventory now prefilters candidates before canonical `lsof` checks.
- Made release assets immutable, verified all eight assets with GitHub build
  attestations before publishing a draft, and renamed the package-local hash
  binding to the honest `release-manifest.digest.json` name.
- Replaced the recommended mutable bootstrap pipeline with a version-pinned,
  attestation-verified local-asset install flow. Legacy pipelines remain only
  for compatibility and display an unauthenticated-bootstrap warning.

---

- 全链路修复 P1 `sources: []` 停机：Gateway、`@acosmi/sdk-ts 2.15.0`、
  TypeScript 归一层与 Rust 独立防线统一语义。空来源成为展示级 no-op；损坏的可选
  provenance 只做兼容降级，不能终止 runtime。
- 新增由契约生成的版本化事件处置表。已知 turn 生命周期损坏只中断关联本轮；只有
  framing、关联身份、control、权限、信任与秘密边界损坏仍拥有全局 fail-closed 权限。
- shutdown 改为幂等，并确保 renderer 首要错误不会被 cleanup 次生错误覆盖。
- renderer diagnostics 改为显式注入、有界无内容 shape metadata、严格 raw 脱敏与
  临时测试状态根，不再污染真实 `~/.crabcode`。
- 新增精确 63 字节事故回放、连续两轮 package smoke、Memory promotion 回收、真实
  状态 sentinel，以及五平台每平台 100 次 CI/发行门禁。
- 过长 Unix 状态根改用私有 `0700` supervisor 短命名空间与 `0600` socket，所有内核
  可表示的历史路径保持不变。Memory 退出只删除自己绑定的精确 inode 和自身空短目录，
  replacement inode 与非空目录均保留。
- 每个平台 package 门禁新增离线 installer 与稳定版本 launcher 回放，并硬性验证进程、
  socket、短命名空间零泄漏；macOS 进程盘点先预筛候选，再以 canonical `lsof` 核权。
- 发布资产禁止覆盖；draft 发布前验证八个资产的 GitHub build attestation；包内普通
  hash 绑定诚实改名为 `release-manifest.digest.json`。
- 推荐安装改为固定版本、attestation 验证后的本地资产流程；旧 pipeline 仅为兼容保留，
  并明确提示 bootstrap 来源未认证。

## v1.0.22 — 2026-08-02

- Established a source-only, open-source baseline for the native CrabCode TUI.
- Removed GUI, AppServer, shared-application communication, archived source,
  migration evidence, internal plans, and agent project files.
- Reduced Rust and TypeScript trees to the TUI and required-sidecar closure.
- Added bilingual project, contribution, security, and license documentation.
- Added a fail-closed repository boundary check and public CI.
- Made Simplified Chinese the default project introduction and added the
  project logo, architecture, OAuth upstream acknowledgment, all-Rust roadmap,
  GUI download, and clearly separated GO membership/service terms.
- Added native five-platform release archives, fail-closed installers,
  SHA-256 and per-file manifests, dependency license collection, signed
  Account Bridge verification, and GitHub build provenance.
- Repaired the macOS Account Bridge code seals, added a native plugin-load
  smoke test and codesign gate, and replaced inaccurate notarization metadata
  with release-scoped Ed25519 provenance plus explicit ad-hoc seal evidence.

---

- 建立 CrabCode 原生 TUI 的纯源码开源基线。
- 移除 GUI、AppServer、应用统一通信、归档源码、迁移证据、内部方案和智能体项目
  文件。
- 将 Rust 与 TypeScript 源码收敛到 TUI 及必要侧车闭包。
- 补齐中英文项目介绍、贡献、安全与许可证说明。
- 新增失败关闭的仓库边界检查与公开 CI。
- 默认介绍改为简体中文，并补充项目 Logo、当前架构、OAuth 上游鸣谢、全仓 Rust
  目标、GUI 下载与明确区分的 GO 会员/线上服务条款。
- 新增五平台原生发行包、失败关闭安装器、SHA-256 与逐文件清单、依赖许可收集、
  Account Bridge 签名验证和 GitHub 构建来源证明。
- 修复 macOS Account Bridge 代码封印，补充原生插件加载冒烟测试与 codesign 闸门；
  以发行版专用 Ed25519 来源签名和明确的 ad-hoc 封印证据替代不准确的公证声明。
