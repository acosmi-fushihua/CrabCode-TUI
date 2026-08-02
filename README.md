<p align="center">
  <img src=".github/assets/crab-code-logo.png" width="320" alt="CrabCode Logo">
</p>

<h1 align="center">CrabCode TUI</h1>

<p align="center">面向终端的开源编码智能体：Rust 原生界面，TypeScript 智能体运行时，隔离的 Go OAuth 账户桥。</p>

<p align="center"><a href="README.en.md">English</a> · <a href="https://github.com/acosmi/CrabCode-TUI/releases/latest">TUI 下载</a> · <a href="https://acosmi.com/zh/downloads">GUI 下载</a></p>

CrabCode TUI 是 CrabCode 的纯终端开源版本。Rust 进程独占终端、渲染和本地进程生命周期，直接拉起 TypeScript 业务运行时；需要账户 OAuth 登录时，再按需启动隔离的 Go Account Bridge。这里没有桌面/Web GUI、React/Ink 界面、AppServer、应用统一通信层、归档源码或内部项目方案。

## 选择版本与安装

### TUI（本仓库，开源）

macOS / Linux：

```bash
curl -fsSL https://github.com/acosmi/CrabCode-TUI/releases/latest/download/install.sh | sh
```

Windows PowerShell：

```powershell
irm https://github.com/acosmi/CrabCode-TUI/releases/latest/download/install.ps1 | iex
```

也可以在 [GitHub Releases](https://github.com/acosmi/CrabCode-TUI/releases/latest) 下载对应平台的完整包。正式包内含 `crabcode`、原生 TUI、Bun、记忆与定时任务侧车、ripgrep、浏览器后端、图像原生库和 Account Bridge，不要求用户另行安装 Rust、Bun 或 Go。安装器会先校验发布级 SHA-256，再校验包内逐文件清单；支持 macOS/Linux 的 arm64、x64 和 Windows x64。

### GUI（独立产品，不在本仓库开源）

需要桌面图形界面时，请从 [CrabCode GUI 官方下载页](https://acosmi.com/zh/downloads) 获取。GUI 的源码、构建工程、应用通信实现和安装包不属于本仓库，也不会以隐藏目录、归档文件或历史分支的方式混入纯 TUI 开源边界。

## 账户、GO 会员与模型

安装后可通过账户入口完成 OAuth 登录。注册账户即赠送 **6 个月 GO 订阅会员**；邀请好友可获得**重置次数**。GO 会员当前可使用 **DeepSeek-V4、Mino、Qwen 3.7 快速版**。

这里的 **GO 是 CrabCode 的产品会员名称，不是 Go 编程语言**。赠送资格、可用地区、模型上下线、额度、重置规则和服务条款以登录时的线上服务实际展示为准；仓库的 MIT 源码许可证不授予订阅、模型额度、第三方 API、托管服务或商标权益。

## 当前架构

| 层 | 当前实现 | 职责 |
| --- | --- | --- |
| 核心底层 | Rust | 终端所有权、输入/渲染、启动器、进程监督、沙箱基础、记忆、搜索、定时任务及本地生命周期 |
| 业务层 | TypeScript | 智能体编排、会话、工具调用、权限决策、模型与账户业务逻辑；编译为单一 TUI 运行时 bundle |
| 账户接入 | Go | 独立、loopback-only 的 Account Bridge，处理 OAuth 凭据与提供方协议；不通过 FFI 嵌入 Rust/TS 进程 |

```text
Terminal
  └─ Rust crabcode launcher / native TUI
       ├─ Rust memory, search and cron sidecars
       └─ private structured stdio
            └─ TypeScript agent runtime (bundled Bun)
                 └─ Go Account Bridge（仅在 OAuth 账户流程需要时）
```

Rust TUI 与 TypeScript 之间使用进程私有的结构化标准输入/输出协议，不经过 GUI/AppServer 或所谓“统一应用通信层”。Account Bridge 只监听受限回环地址，发布包会在登录前验证其版本、平台、来源签名、插件、SBOM 和第三方许可材料。

## OAuth 开源项目鸣谢

账户接入侧车 `components/oauthapi-llm` 基于 MIT 许可的 [`router-for-me/CLIProxyAPI`](https://github.com/router-for-me/CLIProxyAPI) 二次开发，固定来源为 [`v7.2.71`](https://github.com/router-for-me/CLIProxyAPI/releases/tag/v7.2.71) / commit [`5b7f2361ee27d195f6514dde08656f6e4773a9a4`](https://github.com/router-for-me/CLIProxyAPI/commit/5b7f2361ee27d195f6514dde08656f6e4773a9a4)。感谢原项目及其贡献者为 OAuth 登录与提供方协议适配打下基础。

CrabCode 的改动包括白标、回环面收敛、固定账户路由、地区/连接器策略验证、凭据加固、固定插件及发行验证。该衍生组件不是 Router-For.ME 官方发行版，也不代表任何模型服务商背书。完整来源、修改说明和许可证见 [`components/oauthapi-llm/NOTICE`](components/oauthapi-llm/NOTICE)、[`UPSTREAM.lock`](components/oauthapi-llm/UPSTREAM.lock) 与 [`LICENSE`](components/oauthapi-llm/LICENSE)。

## 全仓 Rust 目标

维护者的长期目标是让产品运行时最终实现为**全仓 Rust**。现阶段不是“已经全 Rust”：核心底层已是 Rust，业务层仍为 TypeScript，OAuth 账户桥仍为 Go。

迁移遵循这些原则：

- 新增底层、跨层协议和高可靠能力优先用 Rust；
- 先固定行为、协议、状态机与安全边界，再逐段替换 TypeScript/Go 实现；
- OAuth 迁移必须在凭据隔离、提供方兼容、签名验证和故障恢复达到同等或更高水平后进行；
- 只有功能等价、回归测试和回滚路径均完成，才会移除 Bun 或 Go 运行依赖。

因此当前 TypeScript 与 Go 代码是受支持的正式过渡实现，不是归档代码；路线目标也不会被误写成当前事实。

## 仓库包含什么

| 模块 | 源码位置 |
| --- | --- |
| 原生终端界面与纯 TUI 启动器 | `crates/crabcode-tui`、`crates/crabcode-cli` |
| 智能体与工具直连运行时 | `src`，唯一入口为 `src/entrypoints/tuiRuntime.ts` |
| 定时任务侧车 | `crates/crabcode-cron` 及其 Rust 依赖闭包 |
| 记忆与搜索 | `libs/acosmi-memory`、`libs/acosmi-se` |
| OAuth Account Bridge | `components/oauthapi-llm` |
| 修订/固定的终端、图表和平台依赖 | `third_party` 与相关渲染 crate |
| 构建、验证和发行 | `scripts`、`.github/workflows` |

`bun run check:boundary` 会失败关闭地拒绝 GUI/AppServer/Ink 路径、额外 crate/脚本/工作流、不可达 TypeScript 源码、归档与二进制工件、内部计划和 `AGENTS.md`/`CLAUDE.md` 等项目指令文件。

## 从源码构建

预编译包的普通用户不需要以下工具。源码开发需要 Bun 1.3.11+、Rust 1.88+、`components/oauthapi-llm/go.mod` 指定的 Go 版本和 Git：

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

开发态启动方式：

```bash
bun run build:ts
CRABCODE_TUI_RUNTIME_SCRIPT="$PWD/dist/tui-runtime/index.js" \
CRABCODE_TUI_BUN="$(command -v bun)" \
cargo run --manifest-path crates/Cargo.toml \
  -p crabcode-tui --features terminal-lifecycle-tests
```

正式启动器只接受同一不可变版本目录中的闭合运行时布局；上面的环境变量仅在测试/开发 feature 下生效。

## 验证

```bash
bun run check
bun run test
bun run test:rust
bun run test:memory
bun run test:search
bun run test:account-bridge
bun run smoke:tui
```

`bun run ci` 执行完整本地校验。发行工作流还会在五个原生平台构建、验证 Account Bridge 签名、生成逐文件清单、收集依赖许可、验证安装布局并为发布资产生成 SHA-256 与 GitHub 构建来源证明。

## 开源与许可证

CrabCode TUI 原创代码采用 [MIT License](LICENSE)。仓库内的衍生与 vendored 组件继续适用其各自的 Apache-2.0、MIT 或其他许可证；发布包内附精确依赖版本的许可材料和来源清单。详见 [开源范围与许可证说明](OPEN_SOURCE.zh-CN.md)、[第三方声明](THIRD_PARTY_NOTICES.md)、[贡献指南](CONTRIBUTING.zh-CN.md) 与 [安全策略](SECURITY.zh-CN.md)。
