# CrabCode TUI

[English](README.md)

CrabCode TUI 是一个仅面向终端的开源编码智能体。原生 Rust 界面独占终端，
由其直接拉起的 TypeScript 运行时负责智能体与工具生命周期。本仓库明确不包含
桌面或 Web GUI、React/Ink 渲染器、AppServer、应用统一通信层、归档实现以及
内部规划材料。

## 仓库包含什么

| 模块 | 源码位置 |
| --- | --- |
| 原生终端界面与启动器 | `crates/crabcode-tui`、`crates/crabcode-cli` |
| 智能体与工具直连运行时 | `src`，入口为 `src/entrypoints/tuiRuntime.ts` |
| 定时任务侧车 | `crates/crabcode-cron` 及其 Rust 依赖闭包 |
| 记忆与搜索侧车 | `libs/acosmi-memory`、`libs/acosmi-se` |
| Account Bridge | `components/oauthapi-llm` |
| 终端与图表相关修订依赖 | `third_party` 及渲染相关 crate |

仓库边界可以直接执行：`bun run check:boundary` 会拒绝非 TUI 产品面、额外的
crate、脚本和工作流、不可达 TypeScript 源码、GUI 依赖、归档/二进制文件以及
内部智能体项目文件。

## 环境要求

- Bun 1.3.11 或更高版本
- Rust 1.88 或更高版本
- `components/oauthapi-llm/go.mod` 声明的 Go 版本
- Git 与受支持的终端

仓库不附带服务凭证、OAuth token、签名密钥或托管服务权限。请只配置你有权
使用的账户与服务端点。
可选的 Antigravity OAuth 提供方在运行时需要
`CRABCODE_ANTIGRAVITY_OAUTH_CLIENT_SECRET`；构建、测试及使用其他提供方均不
需要该变量。禁止将其提交到仓库。

## 从源码构建

```bash
git clone https://github.com/acosmi/CrabCode-TUI.git
cd CrabCode-TUI
bun install --frozen-lockfile
bun run build
```

也可以分别构建：

```bash
bun run build:ts
bun run build:rust
bun run build:memory
bun run build:account-bridge
```

源码开发时，可先构建 TypeScript 运行时，再通过显式的测试/开发入口启动 Rust
TUI：

```bash
bun run build:ts
CRABCODE_TUI_RUNTIME_SCRIPT="$PWD/dist/tui-runtime/index.js" \
CRABCODE_TUI_BUN="$(command -v bun)" \
cargo run --manifest-path crates/Cargo.toml \
  -p crabcode-tui --features terminal-lifecycle-tests
```

正式启动器使用封闭的同级文件布局，并执行更严格的版本树校验；上面的命令仅供
源码开发。

## 测试

```bash
bun run check
bun run test
bun run test:rust
bun run test:memory
bun run test:search
bun run test:account-bridge
bun run smoke:tui
```

`bun run ci` 会执行本地完整校验。部分平台专项测试需要对应操作系统或外部沙箱
设施。

## 仓库边界原则

- 产品源码必须能从原生 TUI 或必要侧车到达。
- 不接收 GUI、AppServer、应用统一通信、归档源码、迁移证据、内部方案或智能体
  指令文件。
- 构建产物和本地审计材料必须保持未跟踪状态。
- 新产品面应拆到独立仓库，不能隐藏在本仓库的旁路分支中。

许可边界见 [OPEN_SOURCE.zh-CN.md](OPEN_SOURCE.zh-CN.md)，贡献方式见
[CONTRIBUTING.zh-CN.md](CONTRIBUTING.zh-CN.md)，安全问题提交方式见
[SECURITY.zh-CN.md](SECURITY.zh-CN.md)。

## 许可证

CrabCode TUI 原创代码采用 MIT 许可证。仓库内保留的衍生代码和 vendored 组件
继续适用其相邻位置标明的 Apache-2.0、MIT 或其他许可证。详见
[LICENSE](LICENSE)、[OPEN_SOURCE.zh-CN.md](OPEN_SOURCE.zh-CN.md) 与
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
