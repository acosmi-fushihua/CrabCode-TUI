//! `crabcode sandbox-probe` —— 沙箱执行后端能力探测（W-SANDBOX-ENFORCED-DEADCODE PR-0 骨架）
//!
//! 唯一职责：把「这台机器上、这个构建里，沙箱执行后端到底接没接线」写成一行
//! JSON 打到 stdout，然后 exit 0。**探测本身永不失败**——不可用是一个被报告
//! 的事实（`available:false` + `reason`），不是一个错误码。
//!
//! 唯一消费者 = TS 侧 `src/utils/sandbox/enforcedBackendProbe.ts::probeEnforcedBackend`，
//! 其判据逐字为 `version === 1 && backends.bash.available === true`；任何
//! 不满足（含 spawn 失败 / 超时 / JSON 解析失败 / 版本不符）一律 **fail-closed
//! 判 not-wired**——宁可降级成「没有沙箱」并如实披露，也不能假装有沙箱。
//!
//! **PR-2 起报的是真探测结果**（`acosmi_sandbox::platform::probe_exec_backend`）：
//! Linux 向内核要一个 landlock ruleset（只 create、绝不 `restrict_self`——
//! 施加沙箱不可逆，探测进程还要接着打印 JSON 并正常退出）、macOS 看 Seatbelt
//! 实现存在性、**Windows（PR-3 起）在一个空临时目录里真跑一次沙箱 spawn 并
//! 计时**，越过 500 ms 性能门即报不可用。
//!
//! 探测通过 ≠ 运行期一定成功：真失败由 125 协议在**那一条命令**上诚实上报并把
//! 会话翻成降级。探测挡的是「这台机器根本没有这个能力」，不是预演一次执行。
//!
//! 契约：`version` / `platform` / `backends.bash.{available,reason}` 四个字段
//! 与 TS 判据是同一份合同。增删改字段必须**同 PR** 同步
//! `enforcedBackendProbe.ts` 的解析与两侧测试。

use anyhow::Result;
use serde_json::{Value, json};

/// 探测报告的 wire 版本。TS 侧精确相等校验；不匹配即 fail-closed 判 not-wired。
pub const SANDBOX_PROBE_VERSION: u32 = 1;

/// 把 `std::env::consts::OS` 映射成 TS 侧 `process.platform` 的写法，
/// 让两侧对「当前平台」的称呼逐字一致（TS 永远看到 win32/darwin/linux）。
/// 其它 OS 原样透传——诚实报告未知平台，好过硬塞进三选一。
fn probe_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

/// 构造探测报告。抽成纯函数以便单测直接断言，无需 spawn 进程。
pub fn sandbox_probe_report() -> Value {
    // bash 后端 = Unix 的 `sandbox-exec` 自沙箱 + `execvp`（PR-2）/
    // Windows 的 spawn-child + Job（PR-3，未接线）。
    let backend = acosmi_sandbox::platform::probe_exec_backend();
    let bash = if backend.available {
        // 可用时**不带** reason 字段：TS 侧只在 `available !== true` 时读它，
        // 多写一个恒被忽略的字段就是给未来的读者留一个歧义。
        json!({ "available": true })
    } else {
        json!({
            "available": false,
            "reason": backend.reason.unwrap_or("unspecified"),
        })
    };

    json!({
        "version": SANDBOX_PROBE_VERSION,
        "platform": probe_platform(),
        "backends": { "bash": bash }
    })
}

/// 执行 `crabcode sandbox-probe`：一行 JSON 到 stdout，exit 0。
///
/// stdout 必须**只有**这一行——TS 侧直接 `JSON.parse(stdout)`。调用点因此
/// 安排在 `init_tracing` 之前（见 `main.rs::try_dispatch_sandbox_probe_before_public_parse`），
/// 日志一个字节都不会混进来。
pub fn run_sandbox_probe() -> Result<()> {
    println!("{}", serde_json::to_string(&sandbox_probe_report())?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_serializes_to_parseable_single_line_json() {
        let text = serde_json::to_string(&sandbox_probe_report()).expect("serialize");
        assert!(!text.contains('\n'), "probe payload must be a single line");
        let parsed: Value = serde_json::from_str(&text).expect("stdout payload must parse");
        assert_eq!(parsed["version"], json!(SANDBOX_PROBE_VERSION));
    }

    #[test]
    fn windows_unavailability_can_only_be_one_of_the_declared_reasons() {
        // Windows 探测自 PR-3 起**真跑一次沙箱 spawn** 并计时，所以本机结果取决
        // 于机器状态，不能断言成定值。能断言的是那条分期门本身：不可用时给出的
        // 理由必须是我们声明过的两个之一 —— 一个新冒出来的 slug 会原样出现在
        // 用户看到的 `backend-unavailable:<reason>` 里，而 TS 侧没有它的文案。
        if !cfg!(windows) {
            return;
        }
        let report = sandbox_probe_report();
        let bash = &report["backends"]["bash"];
        if bash["available"] == json!(true) {
            return;
        }
        let reason = bash["reason"].as_str().expect("reason");
        assert!(
            reason == acosmi_sandbox::platform::PROBE_REASON_WINDOWS_SPAWN_FAILED
                || reason == acosmi_sandbox::platform::PROBE_REASON_WINDOWS_TOO_SLOW,
            "undeclared windows probe reason: {reason}"
        );
    }

    #[test]
    fn the_bash_backend_entry_always_answers_the_ts_predicate() {
        // TS 判据逐字是 `version === 1 && backends.bash.available === true`
        // （`enforcedBackendProbe.ts::interpretProbePayload`）。无论本机探测出
        // 什么，payload 都必须让那条判据有确定答案：`available` 恒是布尔，
        // 且不可用时必须带一个单 token 的 reason —— TS 会把它原样拼进
        // `backend-unavailable:<reason>` 展示给用户。
        let report = sandbox_probe_report();
        let bash = &report["backends"]["bash"];
        let available = bash["available"]
            .as_bool()
            .expect("backends.bash.available must be a boolean");
        if available {
            assert!(
                bash.get("reason").is_none(),
                "a wired backend must not carry a reason"
            );
        } else {
            let reason = bash["reason"]
                .as_str()
                .expect("an unavailable backend must name its reason");
            assert!(!reason.is_empty());
            assert!(
                !reason.contains(char::is_whitespace),
                "reason slug `{reason}` must be a single token"
            );
        }
    }

    #[test]
    fn platform_is_reported_in_the_typescript_spelling() {
        let report = sandbox_probe_report();
        let platform = report["platform"].as_str().expect("platform is a string");
        // 本机 OS 的映射必须逐字命中 TS 侧的 process.platform 写法。
        let expected = match std::env::consts::OS {
            "windows" => "win32",
            "macos" => "darwin",
            other => other,
        };
        assert_eq!(platform, expected);
        // 反向守卫：Rust 自己的拼写绝不能泄漏给 TS。
        assert_ne!(platform, "windows");
        assert_ne!(platform, "macos");
    }

    #[test]
    fn backends_map_carries_exactly_the_declared_keys() {
        // 后端集合是合同的一部分：多报一个 TS 不认识的后端等于悄悄扩面。
        let report = sandbox_probe_report();
        let backends = report["backends"]
            .as_object()
            .expect("backends is an object");
        assert_eq!(backends.len(), 1);
        assert!(backends.contains_key("bash"));
    }
}
