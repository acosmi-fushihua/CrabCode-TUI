//! 跨语言 wire 合同闸门（W-SANDBOX-ENFORCED-DEADCODE PR-1）。
//!
//! `tests/fixtures/sandbox-exec-config-v1.json`（**仓库根**下，不是本 crate 下）
//! 是 TS 与 Rust 之间唯一的共享样本：
//!
//! - TS 侧 `tests/unit/sandbox-exec-config.test.ts` 断言
//!   `buildSandboxExecConfig` 真实产物的**字段集合**与该文件逐字一致；
//! - 本文件断言同一份字节能被 [`SandboxExecConfigV1`] **严格**反序列化
//!   （`deny_unknown_fields` + 全字段必填）并映射成执行计划。
//!
//! 两侧合起来盖住唯一真正危险的漂移形态：**一边加了字段、另一边没跟**。
//! TS 多写一个键 ⇒ 本文件红（未知字段）；TS 少写一个键 ⇒ 本文件红（缺字段）；
//! 只改 fixture 而不改任一侧 ⇒ 对侧红。
//!
//! 改 schema 必须**同 PR 三处同步**：
//!   1. `src/utils/sandbox/sandboxExecConfig.ts::SandboxExecConfigV1`
//!   2. `tests/fixtures/sandbox-exec-config-v1.json`
//!   3. `crates/acosmi-sandbox/src/exec_config.rs::SandboxExecConfigV1`
//!
//! fixture 里的路径是 `<CWD>` 这类占位符而非真路径：同一份字节要在三平台被读，
//! 任何真绝对路径都只能在一个平台上成立。所以本文件**刻意不跑**
//! `SandboxExecPlan::validate`（它要求 cwd 是本平台的绝对路径）——真值校验由
//! `exec_config.rs` 的单测用 `std::env::temp_dir()` 覆盖。

use std::path::PathBuf;

use acosmi_sandbox::config::{BackendPreference, NetworkPolicy, SecurityLevel};
use acosmi_sandbox::exec_config::{FidelityLevel, SandboxExecConfigV1};

/// 从本 crate 走到仓库根：`crates/acosmi-sandbox` → `crates` → 仓库根。
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("sandbox-exec-config-v1.json")
}

fn fixture_text() -> String {
    let path = fixture_path();
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture {} must be readable: {e}", path.display()))
}

#[test]
fn fixture_deserializes_strictly() {
    // 严格解析：未知字段 / 缺字段都会在这里失败。
    let parsed = SandboxExecConfigV1::parse(&fixture_text())
        .unwrap_or_else(|e| panic!("fixture must be a valid v1 document: {e}"));

    assert_eq!(parsed.config_version, 1);
    assert_eq!(parsed.security_level, SecurityLevel::L1Allowlist);
    assert_eq!(parsed.network.policy, NetworkPolicy::Restricted);
    assert_eq!(parsed.fidelity.level, FidelityLevel::Partial);
    assert_eq!(parsed.fidelity.unenforced, vec!["network.allowedDomains"]);
}

#[test]
fn fixture_maps_into_a_plan_without_losing_any_rule() {
    let parsed = SandboxExecConfigV1::parse(&fixture_text()).expect("fixture parses");
    let filesystem_before = parsed.filesystem.clone();
    let network_before = parsed.network.clone();

    let plan = parsed
        .into_plan("/bin/bash".to_string(), vec!["-c".into(), "echo hi".into()])
        .expect("fixture maps");

    // 四表逐字带到——这正是事故里被丢掉的那批规则。
    assert_eq!(plan.filesystem, filesystem_before);
    // 网络规则全量带到（`base.network_policy` 只是其中的档位那一维）。
    assert_eq!(plan.network, network_before);
    assert_eq!(plan.base.network_policy, Some(network_before.policy));

    // 命令来自 argv 而不是文件（配置文件里根本没有 command 字段）。
    assert_eq!(plan.base.command, "/bin/bash");
    assert_eq!(
        plan.base.args,
        vec!["-c".to_string(), "echo hi".to_string()]
    );

    // per-command helper 永不退化成容器。
    assert_eq!(plan.base.backend, BackendPreference::Native);
    // env 走进程继承通道，不进配置文件。
    assert!(plan.base.env_vars.is_empty());
    // fs 访问由四表在 PR-2 生成内核规则，不走 bind mount。
    assert!(plan.base.mounts.is_empty());
}

#[test]
fn an_extra_key_in_the_fixture_would_be_caught() {
    // 反向证明本闸门真的会红：给 fixture 加一个 Rust 不认识的键必须失败。
    // （不改磁盘上的 fixture，只在内存里加。）
    let mut doc: serde_json::Value =
        serde_json::from_str(&fixture_text()).expect("fixture is JSON");
    doc.as_object_mut()
        .expect("fixture is an object")
        .insert("futureRule".into(), serde_json::json!(["x"]));
    let json = serde_json::to_string(&doc).expect("serialize");
    assert!(
        SandboxExecConfigV1::parse(&json).is_err(),
        "a field TS added but Rust never learned about must fail loudly"
    );
}
