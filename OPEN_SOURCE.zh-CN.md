# 开源范围与许可证说明

[English](OPEN_SOURCE.md)

## 源码范围

本仓库是 CrabCode 终端产品的源码发行仓，只包含原生 TUI、直连 TypeScript
后端以及必要本地侧车所需的代码。下列内容明确不属于本仓库，禁止提交：

- 桌面端、浏览器窗口、移动端或 Web GUI 源码；
- React/Ink 应用渲染器；
- AppServer 与应用统一通信实现；
- 归档、已替代、仅迁移使用或研究性质的代码；
- 内部审计、路线图、实施方案、提示词和智能体项目指令文件；
- 凭证、生产配置、签名材料与构建产物。

该边界由 `scripts/repository-boundary.mjs` 和 CI 强制执行。

## 许可证分布

除文件或组件另有明确声明外，根目录的 [MIT 许可证](LICENSE)适用于 CrabCode
TUI 原创代码。特别说明如下：

- `crates/acosmi-util-absolute-path` 改编自 OpenAI Codex，并按源码头部标识
  继续适用 Apache-2.0。
- `crates/crabcode-markdown*`、`crates/crabcode-mermaid`、
  `crates/crabcode-pager-render`、`crates/crabcode-ratatui-inline` 和
  `crates/crabcode-ratatui-textarea` 含公开 xAI/Ratatui 源码的衍生部分；其
  Apache-2.0/MIT 声明、来源和修改记录与源码相邻保留。
- `libs/acosmi-memory` 与 `libs/acosmi-se` 在 Cargo 工作区/组件中声明
  Apache-2.0。
- `components/oauthapi-llm` 是 MIT 许可的衍生组件，附带独立 `LICENSE` 与
  `NOTICE`。
- `third_party` 中的组件保留各自许可证和修改说明。
- registry 与 npm 依赖继续适用其发布者声明的许可证，具体版本由相应锁文件
  固定。

不同许可证并存时，以组件自身许可证为准。仓库根 MIT 许可证不会覆盖或取消
上游版权、NOTICE、专利、署名或源码分发义务。

## 许可证不包含什么

源码许可证不提供 API 访问权、付费服务权益、第三方账户权限、签名密钥、托管
基础设施、技术支持或商标许可。使用者须自行遵守所连接服务和模型的条款。

CrabCode 的 GO 订阅会员是线上产品权益，不是 Go 编程语言，也不是 MIT 许可证
的一部分。注册赠送的 6 个月会员、邀请好友所得重置次数，以及 DeepSeek-V4、
Mino、Qwen 3.7 快速版等模型的可用性、地区、额度与规则，均以线上服务届时
展示及其服务条款为准。复制、修改或分发本仓库源码不会自动创建账户、会员或
模型调用额度。

## 再分发

再分发源码时，应保留根许可证、本说明以及所有适用的组件许可证和 NOTICE。
分发二进制时，还必须为实际打包的精确依赖版本和平台工件收集完整许可材料。
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 是署名索引，不能替代对完整
依赖闭包的审查。官方 GitHub Release 会随每个平台包附带逐文件哈希、构建材料
清单及实际进入二进制闭包的 JavaScript、Rust、Go 和原生第三方许可材料。
