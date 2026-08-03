# 安全策略

[English](SECURITY.md)

如发现疑似漏洞，请通过本仓库的 GitHub 私密安全公告功能提交。不要在公开 Issue
中披露利用代码、凭证、真实账户数据或尚未修复的漏洞。

报告请包含受影响提交、平台、复现步骤、影响和验证所需的最少证据。请使用合成
账户，并遮盖 token、Cookie、密钥、日志和个人信息。

目前仅支持 `main` 分支。本源码仓不承诺 SLA，不提供托管服务、签名密钥或第三方
服务权益。上游依赖的漏洞可能还需要同时报告给相应维护者。

## 发布信任边界

正式发行只接受 tagger 为 `crabcode-release@acosmi.com` 的 signed annotated `v*`
tag。SSH Ed25519 公钥由受保护仓库变量 `RELEASE_TAG_SIGNER_SSH_ED25519` 提供；
私钥不得进入源码、安装包或 workflow 日志。仓库管理必须启用 `release-tags`
ruleset（仅 release-owner 角色可创建，禁止删除与强制更新）和带两名必需 reviewer
的 `production-release` environment。

workflow 在进入该 environment 前构建并回放五个平台包，随后创建不可变 draft；
只有八个固定资产全部通过 GitHub build attestation 与服务端 SHA-256 对照后，才会
公开并设为 latest。`release-manifest.digest.json` 只绑定包内逐文件清单，明确不是
公钥签名。
