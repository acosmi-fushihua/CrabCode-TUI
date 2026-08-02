# 贡献指南

[English](CONTRIBUTING.md)

欢迎为终端产品贡献代码。所有变更都必须处于纯 TUI 仓库边界内。

1. 建立范围明确的分支，不夹带无关生成文件。
2. 使用 `bun install --frozen-lockfile` 安装依赖。
3. 实施最小且完整的改动，并在对应责任层补充测试。
4. 执行 `bun run check` 以及相关 TypeScript、Rust、Go、记忆和搜索测试；范围
   较大的变更应执行 `bun run ci`。
5. 若新增 vendored 代码，在 PR 中说明行为、兼容性、安全影响、许可证与来源。

不要加入 GUI/AppServer 源码、归档实现、内部方案、智能体指令文件、秘密、生产
端点或二进制工件。不能仅为了塞入新产品面而削弱
`scripts/repository-boundary.mjs`；不属于 TUI 的能力应拆到独立仓库。

提交贡献即表示你同意按被修改文件适用的许可证提供该贡献，并保留全部上游
声明。
