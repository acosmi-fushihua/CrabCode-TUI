//! `sandbox-exec` 配置文件（v1）的解析与映射
//! （W-SANDBOX-ENFORCED-DEADCODE PR-1）。
//!
//! 这是「配置保真管道」的 Rust 半边。TS 半边是
//! `src/utils/sandbox/sandboxExecConfig.ts::buildSandboxExecConfig`：它把
//! settings 派生出的全量隔离规则写成一个 0600 JSON；本模块把那份 JSON 变成
//! 一个 [`SandboxExecPlan`]，helper（`crabcode-cli::sandbox_exec`）据此施加隔离。
//!
//! ## 为什么必须 fail-closed 到偏执
//!
//! 事故根因（SoT §1.5）不是「配置传错了」，是「配置**根本没传**，而两边都
//! 若无其事」：TS 派生了 fs 四表和域名规则，Rust 端写死 `mounts:vec![] +
//! network_policy:None`，中间没有任何一环会喊。所以本模块的每个入口都按
//! 「宁可拒绝启动，也不要跑一个规则不全的沙箱」设计：
//!
//! - `#[serde(deny_unknown_fields)]` —— TS 加了字段而 Rust 没跟 ⇒ 立刻红，
//!   而不是把新规则默默丢掉。
//! - **全字段必填、零 `Option`、零 `#[serde(default)]`** —— 这是 E1
//!   「可选安全字段链式失活」的根治面。`Option<T>` + `unwrap_or_default()` 会把
//!   「配置里少了一条规则」翻译成「本来就没这条规则」，而这两件事在安全上
//!   天差地别。
//! - `config_version` 精确相等 —— 版本兼容矩阵本身就是漂移面；两半同 release
//!   齐发，没有兼容需求。
//!
//! ## 为什么 fs 四表不塞进 `SandboxConfig`
//!
//! [`SandboxConfig`] 表达文件系统访问的手段只有 `workspace` + `mounts`
//! （[`MountSpec`](crate::config::MountSpec) = host→sandbox 的 ro/rw 绑定）。
//! 它**表达不了** deny 表，也表达不了 glob / `~` / `.` 这类模式串。硬塞进
//! `mounts` 就是有损映射——而有损映射正是本立项要根治的东西。
//!
//! 所以四表原样挂在 [`SandboxExecPlan::filesystem`] 上逐字带到 PR-2：landlock
//! 规则集与 seatbelt profile 段在那里生成（`platform::apply_sandbox_to_self`
//! 的调用侧）。本 PR **不改任何隔离实现**，只保证规则完整抵达。
//!
//! 同理，把四表加进 `SandboxConfig` 本体也被否掉：那会波及 58 处结构体字面量
//! （17 个文件，含另外两个 crate 的测试），而本 PR 是一条配置管道，不该顺手做
//! 一次跨 crate 的结构体迁移。PR-2 若确实需要，它自带那次改动的验收面。
//!
//! ## 路径是**模式**不是路径
//!
//! 四表里的条目直接来自 CrabCode 的权限规则与 `sandbox.filesystem.*` 设置，
//! 形态包括 `.`（相对 cwd）、`~/x`、`/abs/**` 等。TS 侧**刻意不做**归一化
//! （现状如此，改它是行为变更、不属本 PR），所以本模块也只做「非空 + 无 NUL」
//! 这类与形态无关的校验；模式→内核规则的归一化是 PR-2 的职责，`cwd` 字段就是
//! 为解析 `.` 而在场的。

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{
    BackendPreference, NetworkPolicy, OutputFormat, ResourceLimits, SandboxConfig, SecurityLevel,
};
use crate::error::SandboxError;

/// 配置文件 wire 版本。与 TS 侧
/// `sandboxExecConfig.ts::SANDBOX_EXEC_CONFIG_VERSION` 是同一个数。
pub const SANDBOX_EXEC_CONFIG_VERSION: u32 = 1;

/// 隔离保真度等级。判定规则在 TS 侧（见 `sandboxExecConfig.ts::SandboxFidelity`）；
/// Rust 只负责原样携带——保真度是**策略**，策略单脑在 TS（SoT §2.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FidelityLevel {
    /// 配置里的每条规则都能被内核强制。
    Full,
    /// 存在「答应了但强制不了」的规则（如 Linux 的 allow 子树内 deny 挖洞，或
    /// 代理没起来 / 平台锁不住出口时的域名规则）。判定全在 TS 侧 fidelity。
    Partial,
}

/// 保真度报告。helper 不消费它（它不做策略判断），但它必须在场：配置文件是这条
/// 链路上唯一的证据载体，把「这次隔离缺了哪几条」记在里面，事后取证才有东西可读。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FidelityReport {
    pub level: FidelityLevel,
    /// 承诺了但当前无法强制的规则名（点分路径，对齐 wire 字段名）。
    pub unenforced: Vec<String>,
}

/// 文件系统四表。条目是**模式串**，见模块头〈路径是模式不是路径〉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FsRules {
    pub allow_read: Vec<String>,
    pub allow_write: Vec<String>,
    pub deny_read: Vec<String>,
    pub deny_write: Vec<String>,
}

/// 网络规则。`policy` 是内核真能强制的三档；域名两表由 PR-8 的 loopback 过滤代理
/// 判定 —— `http_proxy_port` 非零（代理在跑）**且**内核把 TCP 出口锁死在该端口上
/// （macOS Seatbelt `localhost:<port>` / Linux Landlock `ConnectTcp` 单端口，均由
/// `platform::verify_egress_locked_to_proxy` canary 实证）时才算强制；否则 TS 侧
/// fidelity 如实标 `partial`（**不假装强制**是本立项铁律）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NetworkRules {
    pub policy: NetworkPolicy,
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
    pub allow_unix_sockets: Vec<String>,
    pub allow_all_unix_sockets: bool,
    pub allow_local_binding: bool,
    /// 0 = 未分配（无域名规则 / 代理没起来）；代理在跑时由
    /// `sandboxNetworkProxy.ts` 在运行时填入其 loopback 端口。
    pub http_proxy_port: u16,
    /// 恒 0。SOCKS 面未实现（HTTP CONNECT 覆盖 curl/git/gh/npm/pip/bun）。
    pub socks_proxy_port: u16,
}

/// 用户显式要求的**放宽**开关。两者都是「更弱」方向，所以必须显式下传——
/// 默认值在这里不是中性的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WeakerFlags {
    pub nested_sandbox: bool,
    pub network_isolation: bool,
}

/// v1 配置文件的**完整**结构。全字段必填（见模块头）。
///
/// 注意这里**没有** `command`/`args`：命令来自 argv（`sandbox-exec … -- prog args`），
/// 不来自文件。这是刻意的分工——配置文件描述「隔离成什么样」，argv 描述
/// 「跑什么」。两者混在一个通道里会让 helper 面对「文件说跑 A、argv 说跑 B」
/// 这种没有正确答案的局面。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SandboxExecConfigV1 {
    pub config_version: u32,
    pub fidelity: FidelityReport,
    pub security_level: SecurityLevel,
    /// 本条命令的工作目录（绝对路径）。fs 模式里的 `.` 以它为基准。
    pub cwd: PathBuf,
    /// 沙箱内 `TMPDIR` 指向的目录。TS 侧已保证它存在且在 `allow_write` 内。
    pub tmp_dir: PathBuf,
    pub filesystem: FsRules,
    pub network: NetworkRules,
    pub weaker: WeakerFlags,
}

/// 映射产物：`SandboxConfig` 能表达的部分 + 它表达不了、必须原样带走的部分。
///
/// PR-2 的执行路径消费这个结构：`base` 交给既有的
/// `platform::apply_sandbox_to_self`，`filesystem` / `network` / `weaker`
/// 交给那一层新增的规则生成器。
#[derive(Debug, Clone)]
pub struct SandboxExecPlan {
    /// 既有内核配置能表达的部分（档位 / 命令 / 工作目录 / 网络档 / 资源限额）。
    pub base: SandboxConfig,
    /// 四表原样（`SandboxConfig` 表达不了，见模块头）。
    pub filesystem: FsRules,
    /// 网络规则全量（`base.network_policy` 只是其中的档位那一维）。
    pub network: NetworkRules,
    pub weaker: WeakerFlags,
    pub tmp_dir: PathBuf,
    pub fidelity: FidelityReport,
}

impl SandboxExecConfigV1 {
    /// 严格解析一份配置文件内容。
    ///
    /// 未知字段 / 缺字段 / 类型不符一律 `Err`——见模块头〈为什么必须 fail-closed〉。
    pub fn parse(json: &str) -> Result<Self, SandboxError> {
        let parsed: Self = serde_json::from_str(json).map_err(|e| SandboxError::InvalidConfig {
            message: format!("sandbox-exec config is not a valid v1 document: {e}"),
        })?;
        if parsed.config_version != SANDBOX_EXEC_CONFIG_VERSION {
            return Err(SandboxError::InvalidConfig {
                message: format!(
                    "sandbox-exec config version {} is not supported (this build speaks exactly {})",
                    parsed.config_version, SANDBOX_EXEC_CONFIG_VERSION
                ),
            });
        }
        Ok(parsed)
    }

    /// 映射成执行计划。`program` / `args` 来自 argv 的 `--` 之后。
    ///
    /// **本函数是无损的**：配置里的每一项要么落进 `base`，要么原样挂在 plan 上。
    /// 将来若出现真的表达不了的项，必须在此处 `Err` 或 `tracing::warn!` 点名，
    /// 不得静默丢弃。
    pub fn into_plan(
        self,
        program: String,
        args: Vec<String>,
    ) -> Result<SandboxExecPlan, SandboxError> {
        if program.is_empty() {
            return Err(SandboxError::InvalidConfig {
                message: "sandbox-exec requires a program to run after `--`".into(),
            });
        }

        let base = SandboxConfig {
            security_level: self.security_level,
            command: program,
            args,
            // 工作目录即隔离的锚点。`SandboxConfig::validate` 会要求它是绝对路径
            // 且不含 `..`。
            workspace: self.cwd,
            // 刻意留空：本链路不做 bind mount，fs 访问由 `filesystem` 四表在
            // PR-2 生成 landlock / seatbelt 规则。见模块头。
            mounts: Vec::new(),
            // 超时属于宿主：TS 侧 `Shell.ts::exec` 用 abort signal + tree-kill 管
            // 生命周期，helper 里再设一个是第二个真源。内存 / CPU / PID 限额
            // settings 未暴露，保持与事故前 supervisor 相同的「不限」语义。
            resource_limits: ResourceLimits::default(),
            network_policy: Some(self.network.policy),
            // env 走进程继承通道（spawn 时给 helper，`execvp` 后原样带给命令），
            // 不进配置文件——两个 env 真源没有正确的合并规则。
            env_vars: HashMap::new(),
            // 本链路不产生 `SandboxOutput`（helper `execvp` 之后进程就是命令本身，
            // stdout/stderr 直通宿主）。该字段在此路径上不被读取。
            format: OutputFormat::Json,
            // 显式 Native：per-command 起 Docker 是荒谬的，而 `Auto` 会在原生
            // 后端不可用时**悄悄**换成容器。声明 Native ⇒ 不可用就是不可用。
            backend: BackendPreference::Native,
        };

        Ok(SandboxExecPlan {
            base,
            filesystem: self.filesystem,
            network: self.network,
            weaker: self.weaker,
            tmp_dir: self.tmp_dir,
            fidelity: self.fidelity,
        })
    }
}

impl SandboxExecPlan {
    /// 施加隔离**之前**的最后一道校验。
    ///
    /// 复用 [`SandboxConfig::validate`]（命令非空 / 工作目录绝对且无 `..` /
    /// 无 NUL / 网络档不得弱化安全档 / 无危险 env），再补四表自身的形态校验。
    ///
    /// 刻意**不**在这里拒绝 `FidelityLevel::Partial`：保真度不足是一个要如实
    /// 上报、由 TS 策略层（PR-5 的 auto-allow 宽免）消费的事实，不是启动错误。
    /// 在这里拒绝会把「沙箱能力弱一点」升级成「命令跑不了」。
    pub fn validate(&self) -> Result<(), SandboxError> {
        self.base.validate()?;

        for (table, patterns) in [
            ("allowRead", &self.filesystem.allow_read),
            ("allowWrite", &self.filesystem.allow_write),
            ("denyRead", &self.filesystem.deny_read),
            ("denyWrite", &self.filesystem.deny_write),
        ] {
            for (i, pattern) in patterns.iter().enumerate() {
                if pattern.is_empty() {
                    return Err(SandboxError::InvalidConfig {
                        message: format!("filesystem.{table}[{i}] is an empty pattern"),
                    });
                }
                if pattern.contains('\0') {
                    return Err(SandboxError::InvalidConfig {
                        message: format!("filesystem.{table}[{i}] contains a null byte"),
                    });
                }
            }
        }

        if self.tmp_dir.as_os_str().is_empty() {
            return Err(SandboxError::InvalidConfig {
                message: "tmpDir must not be empty (it backs TMPDIR inside the sandbox)".into(),
            });
        }
        if !self.tmp_dir.is_absolute() {
            return Err(SandboxError::InvalidConfig {
                message: "tmpDir must be an absolute path".into(),
            });
        }
        if self
            .tmp_dir
            .components()
            .any(|component| component == std::path::Component::ParentDir)
        {
            return Err(SandboxError::InvalidConfig {
                message: "tmpDir must not contain `..` traversal".into(),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// 一份合法 v1 文档。**故意用字面量而不是构造器序列化**——这样 TS 侧改了
    /// wire 名字而 Rust 没跟时，这里会红，而不是两边一起漂走。
    fn valid_json() -> String {
        let cwd = std::env::temp_dir().join("ws");
        let tmp = std::env::temp_dir().join("crabcode-tmp");
        format!(
            r#"{{
              "configVersion": 1,
              "fidelity": {{ "level": "partial", "unenforced": ["network.allowedDomains"] }},
              "securityLevel": "allowlist",
              "cwd": {cwd:?},
              "tmpDir": {tmp:?},
              "filesystem": {{
                "allowRead": ["/usr/lib"],
                "allowWrite": [".", "/tmp/crabcode-501"],
                "denyRead": ["~/.aws/**"],
                "denyWrite": ["/etc/hosts"]
              }},
              "network": {{
                "policy": "restricted",
                "allowedDomains": ["example.com"],
                "deniedDomains": [],
                "allowUnixSockets": ["/var/run/docker.sock"],
                "allowAllUnixSockets": false,
                "allowLocalBinding": true,
                "httpProxyPort": 0,
                "socksProxyPort": 0
              }},
              "weaker": {{ "nestedSandbox": true, "networkIsolation": false }}
            }}"#,
            cwd = cwd.to_string_lossy(),
            tmp = tmp.to_string_lossy(),
        )
    }

    #[test]
    fn valid_v1_document_maps_field_by_field() {
        let parsed = SandboxExecConfigV1::parse(&valid_json()).unwrap();
        let plan = parsed
            .into_plan("bash".to_string(), vec!["-c".into(), "echo hi".into()])
            .unwrap();

        // ── base（SandboxConfig 能表达的部分）────────────────────────────
        assert_eq!(plan.base.security_level, SecurityLevel::L1Allowlist);
        assert_eq!(plan.base.command, "bash");
        assert_eq!(
            plan.base.args,
            vec!["-c".to_string(), "echo hi".to_string()]
        );
        assert_eq!(plan.base.workspace, std::env::temp_dir().join("ws"));
        assert!(plan.base.mounts.is_empty());
        assert_eq!(plan.base.network_policy, Some(NetworkPolicy::Restricted));
        assert!(plan.base.env_vars.is_empty());
        assert_eq!(plan.base.backend, BackendPreference::Native);
        assert_eq!(plan.base.format, OutputFormat::Json);
        assert_eq!(plan.base.resource_limits.timeout_secs, None);
        assert_eq!(plan.base.resource_limits.memory_bytes, 0);
        assert_eq!(plan.base.resource_limits.cpu_millicores, 0);
        assert_eq!(plan.base.resource_limits.max_pids, 0);

        // ── 四表逐字带到（这正是事故里丢掉的那批规则）──────────────────
        assert_eq!(plan.filesystem.allow_read, vec!["/usr/lib".to_string()]);
        assert_eq!(
            plan.filesystem.allow_write,
            vec![".".to_string(), "/tmp/crabcode-501".to_string()]
        );
        assert_eq!(plan.filesystem.deny_read, vec!["~/.aws/**".to_string()]);
        assert_eq!(plan.filesystem.deny_write, vec!["/etc/hosts".to_string()]);

        // ── network 全量（policy 只是其中一维）─────────────────────────
        assert_eq!(plan.network.policy, NetworkPolicy::Restricted);
        assert_eq!(
            plan.network.allowed_domains,
            vec!["example.com".to_string()]
        );
        assert!(plan.network.denied_domains.is_empty());
        assert_eq!(
            plan.network.allow_unix_sockets,
            vec!["/var/run/docker.sock".to_string()]
        );
        assert!(!plan.network.allow_all_unix_sockets);
        assert!(plan.network.allow_local_binding);
        assert_eq!(plan.network.http_proxy_port, 0);
        assert_eq!(plan.network.socks_proxy_port, 0);

        // ── 放宽开关 / tmp / 保真度 ─────────────────────────────────────
        assert!(plan.weaker.nested_sandbox);
        assert!(!plan.weaker.network_isolation);
        assert_eq!(plan.tmp_dir, std::env::temp_dir().join("crabcode-tmp"));
        assert_eq!(plan.fidelity.level, FidelityLevel::Partial);
        assert_eq!(
            plan.fidelity.unenforced,
            vec!["network.allowedDomains".to_string()]
        );

        plan.validate().unwrap();
    }

    #[test]
    fn unknown_field_is_rejected() {
        // TS 加了字段、Rust 没跟 ⇒ 必须当场红。默默忽略等于新规则永不生效。
        let json = valid_json().replace(
            r#""configVersion": 1,"#,
            r#""configVersion": 1, "futureRule": ["x"],"#,
        );
        let err = SandboxExecConfigV1::parse(&json).unwrap_err();
        assert!(
            format!("{err}").contains("futureRule"),
            "error must name the offending field, got: {err}"
        );
    }

    #[test]
    fn every_top_level_field_is_mandatory() {
        // E1 根治面的执行证据：逐个删字段，每一个都必须让解析失败。
        for field in [
            "configVersion",
            "fidelity",
            "securityLevel",
            "cwd",
            "tmpDir",
            "filesystem",
            "network",
            "weaker",
        ] {
            let mut doc: serde_json::Value = serde_json::from_str(&valid_json()).unwrap();
            doc.as_object_mut().unwrap().remove(field).unwrap();
            let json = serde_json::to_string(&doc).unwrap();
            assert!(
                SandboxExecConfigV1::parse(&json).is_err(),
                "missing `{field}` must be rejected — an absent rule is not an empty rule"
            );
        }
    }

    #[test]
    fn every_nested_security_field_is_mandatory() {
        for (parent, field) in [
            ("filesystem", "allowRead"),
            ("filesystem", "allowWrite"),
            ("filesystem", "denyRead"),
            ("filesystem", "denyWrite"),
            ("network", "policy"),
            ("network", "allowedDomains"),
            ("network", "deniedDomains"),
            ("network", "allowUnixSockets"),
            ("network", "allowAllUnixSockets"),
            ("network", "allowLocalBinding"),
            ("network", "httpProxyPort"),
            ("network", "socksProxyPort"),
            ("weaker", "nestedSandbox"),
            ("weaker", "networkIsolation"),
            ("fidelity", "level"),
            ("fidelity", "unenforced"),
        ] {
            let mut doc: serde_json::Value = serde_json::from_str(&valid_json()).unwrap();
            doc.get_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field)
                .unwrap();
            let json = serde_json::to_string(&doc).unwrap();
            assert!(
                SandboxExecConfigV1::parse(&json).is_err(),
                "missing `{parent}.{field}` must be rejected"
            );
        }
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let json = valid_json().replace(r#""configVersion": 1"#, r#""configVersion": 2"#);
        let err = SandboxExecConfigV1::parse(&json).unwrap_err();
        assert!(format!("{err}").contains("version 2"), "got: {err}");
    }

    #[test]
    fn empty_program_is_rejected() {
        let parsed = SandboxExecConfigV1::parse(&valid_json()).unwrap();
        assert!(parsed.into_plan(String::new(), vec![]).is_err());
    }

    #[test]
    fn relative_cwd_is_rejected_by_validate() {
        // helper 在施加隔离前跑 validate；相对 cwd 会让整套路径规则失去锚点。
        let mut doc: serde_json::Value = serde_json::from_str(&valid_json()).unwrap();
        doc["cwd"] = serde_json::Value::String("relative/ws".into());
        let parsed = SandboxExecConfigV1::parse(&serde_json::to_string(&doc).unwrap()).unwrap();
        let plan = parsed.into_plan("bash".into(), vec![]).unwrap();
        assert!(plan.validate().is_err());
    }

    #[test]
    fn relative_or_traversing_tmp_dir_is_rejected_by_validate() {
        for tmp_dir in ["relative/tmp", "/tmp/../escape"] {
            let mut doc: serde_json::Value = serde_json::from_str(&valid_json()).unwrap();
            doc["tmpDir"] = serde_json::Value::String(tmp_dir.into());
            let parsed = SandboxExecConfigV1::parse(&serde_json::to_string(&doc).unwrap()).unwrap();
            let plan = parsed.into_plan("bash".into(), vec![]).unwrap();
            assert!(plan.validate().is_err(), "tmpDir {tmp_dir:?} must fail");
        }
    }

    #[test]
    fn empty_or_nul_bearing_fs_pattern_is_rejected_by_validate() {
        for bad in ["", "/etc/\0passwd"] {
            let mut doc: serde_json::Value = serde_json::from_str(&valid_json()).unwrap();
            doc["filesystem"]["denyWrite"] = serde_json::json!([bad]);
            let parsed = SandboxExecConfigV1::parse(&serde_json::to_string(&doc).unwrap()).unwrap();
            let plan = parsed.into_plan("bash".into(), vec![]).unwrap();
            assert!(
                plan.validate().is_err(),
                "denyWrite pattern {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn network_policy_may_not_weaken_the_security_level() {
        // `SandboxConfig::validate` 的既有守卫必须在这条链路上真的生效。
        let mut doc: serde_json::Value = serde_json::from_str(&valid_json()).unwrap();
        doc["securityLevel"] = serde_json::Value::String("deny".into());
        doc["network"]["policy"] = serde_json::Value::String("host".into());
        let parsed = SandboxExecConfigV1::parse(&serde_json::to_string(&doc).unwrap()).unwrap();
        let plan = parsed.into_plan("bash".into(), vec![]).unwrap();
        assert!(plan.validate().is_err());
    }

    #[test]
    fn partial_fidelity_is_not_a_startup_error() {
        // 保真度不足要如实上报给 TS 策略层，不是「命令跑不了」。
        let parsed = SandboxExecConfigV1::parse(&valid_json()).unwrap();
        let plan = parsed.into_plan("bash".into(), vec![]).unwrap();
        assert_eq!(plan.fidelity.level, FidelityLevel::Partial);
        plan.validate().unwrap();
    }

    #[test]
    fn garbage_input_is_rejected_without_panicking() {
        for bad in ["", "null", "[]", "{", r#"{"configVersion":"1"}"#] {
            assert!(SandboxExecConfigV1::parse(bad).is_err(), "input {bad:?}");
        }
    }
}
