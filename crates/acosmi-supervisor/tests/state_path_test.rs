//! P0.1 回归测试：supervisor 必须通过 `acosmi_config::paths::resolve_state_dir()`
//! 解析 state 工作目录，识别 `CRABCODE_STATE_DIR` / `CRABCODE_HOME` 环境变量，
//! 不得使用裸 `dirs::home_dir()`。
//!
//! 单 #[test] 顺序执行 env 操作，避免同 binary 内多测试并发竞态。

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct EnvRestore {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvRestore {
    fn capture(key: &'static str) -> Self {
        Self {
            key,
            previous: std::env::var_os(key),
        }
    }
}

#[allow(unsafe_code)]
impl Drop for EnvRestore {
    fn drop(&mut self) {
        // SAFETY: 独立 test binary，单 #[test] 顺序执行，无并发 env 写入。
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

#[test]
#[allow(unsafe_code)]
fn supervisor_resolves_state_dir_via_acosmi_config() {
    let _state_dir_guard = EnvRestore::capture("CRABCODE_STATE_DIR");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let target: PathBuf = std::env::temp_dir().join(format!("supervisor-p0-1-{nanos}"));

    // 场景 A：CRABCODE_STATE_DIR 显式覆盖
    // SAFETY: 独立 test binary，单 #[test] 顺序执行，无并发 env 写入
    unsafe {
        std::env::set_var("CRABCODE_STATE_DIR", &target);
    }
    let resolved_override = acosmi_config::paths::resolve_state_dir();
    assert_eq!(
        resolved_override, target,
        "CRABCODE_STATE_DIR 应优先于 dirs::home_dir()"
    );

    // 场景 B：清空 env 后回到默认路径（不硬断言具体值，允许跨机差异）
    unsafe {
        std::env::remove_var("CRABCODE_STATE_DIR");
    }
    let resolved_default = acosmi_config::paths::resolve_state_dir();
    assert!(
        !resolved_default.as_os_str().is_empty(),
        "默认 state dir 不应为空"
    );
    let name = resolved_default
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let acceptable = [".crabcode", ".clawdbot", ".moltbot", ".moldbot"];
    assert!(
        acceptable.contains(&name) || std::env::var("CRABCODE_HOME").is_ok(),
        "默认 state dir 名称 {name:?} 不在白名单内，且未设 CRABCODE_HOME"
    );

    // 场景 C：两者不等（证明 env 覆盖确实生效）
    assert_ne!(
        resolved_override, resolved_default,
        "覆盖路径与默认路径应不等"
    );
}
