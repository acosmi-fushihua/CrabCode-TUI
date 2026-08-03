# Changelog / 更新日志

## v1.0.24 — 2026-08-03

- Forward-fixed the unpublished `v1.0.23` release candidate without moving or
  deleting its signed tag.
- Replaced the x64 macOS package runtime with Bun's pinned baseline build. The
  standard x64 build requires AVX and stalled before the renderer handshake on
  GitHub's Intel macOS runner under Rosetta.
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

- 对未公开发布的 `v1.0.23` 候选版执行前滚修复，不移动或删除其已签名标签。
- x64 macOS 发布包改用固定版本和哈希的 Bun baseline 构建。标准 x64 构建要求
  AVX，在 GitHub Intel macOS runner 的 Rosetta 环境中会在 renderer 握手前停滞。
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
