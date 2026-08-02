//! W-MEMORY-SYNERGY W3 (2026-07-16, RC-5) — 记忆产物输出语言解析。
//!
//! 分工（与 TS 侧 `src/services/memoryTierProxy/language.ts` 互为锚注）：
//!   * **行文语言**归 TS 执行器 —— 全部反向 IPC LLM 调用最终都在
//!     `memoryTierProxy` 执行，那里按同一套解析追加输出语言指令（单点，
//!     覆盖 Tier-1/2/3、想象、watch、进化报告的正文行文）。
//!   * **结构语言**归本模块 —— 进化报告的固定章节骨架等「模板结构」必须
//!     在 Rust 侧选模板（契约 #15-8：Tier prompt 模板只住 orchestrator）。
//!
//! 两侧读同一真源，优先级：
//!   1. env `CRABCODE_MEMORY_LANGUAGE`（zh|en；显式覆盖 / 测试注入）；
//!   2. 全局配置 JSON 的 `memoryLanguage`（zh|en；auto/缺失走下一级）；
//!   3. 同文件 `uiLanguage`（zh-CN→zh，en-US→en）；
//!   4. 默认 zh（与 TS `i18n/config.ts::detectLocale` 的产品默认一致）。
//!
//! 全局配置文件位置 = `<base>` 的**兄弟文件** `.crabcode.json`（TS
//! `utils/config.ts` 的真源：标准布局 `~/.crabcode` ↔ `~/.crabcode.json`；
//! `CRABCODE_HOME` 隔离布局同构为 `<home>/.crabcode` ↔ `<home>/.crabcode.json`）。
//! `CRABCODE_CONFIG_DIR` 全量覆盖布局下按同规则取父目录兄弟文件，属
//! best-effort —— 该布局需要精确控制时用 env 覆盖（第 1 级）。

use std::path::{Path, PathBuf};

/// 记忆产物输出语言（当前两值；扩语种时同步 TS 侧解析与指令文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOutputLanguage {
    Zh,
    En,
}

/// 宽松解析（与 TS `i18n/config.ts::parseEnvLocale` 词表对齐）。
#[must_use]
pub fn parse_language(raw: &str) -> Option<MemoryOutputLanguage> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "zh" | "zh-cn" | "chinese" | "中文" => Some(MemoryOutputLanguage::Zh),
        "en" | "en-us" | "english" => Some(MemoryOutputLanguage::En),
        _ => None,
    }
}

/// `<base>` → 全局配置 JSON 路径（兄弟文件规则，见模块头）。
#[must_use]
pub fn sibling_config_path(base_dir: &Path) -> PathBuf {
    let is_dot_crabcode = base_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".crabcode");
    if is_dot_crabcode {
        if let Some(parent) = base_dir.parent() {
            return parent.join(".crabcode.json");
        }
    }
    base_dir.join(".crabcode.json")
}

/// 纯函数解析核（env 值与配置原文都由调用方喂入 —— 单测不碰进程 env，
/// 防并行测试污染）。
#[must_use]
pub fn resolve_from(env_value: Option<&str>, config_json: Option<&str>) -> MemoryOutputLanguage {
    if let Some(lang) = env_value.and_then(parse_language) {
        return lang;
    }
    if let Some(raw) = config_json {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(lang) = value
                .get("memoryLanguage")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_language)
            {
                return lang;
            }
            if let Some(lang) = value
                .get("uiLanguage")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_language)
            {
                return lang;
            }
        }
    }
    MemoryOutputLanguage::Zh
}

/// 生产入口：env → `<base>` 兄弟配置 → 默认 zh。IO 失败一律降级默认
/// （fail-soft：语言解析绝不阻断做梦/报告链）。
#[must_use]
pub fn resolve_memory_output_language(base_dir: &Path) -> MemoryOutputLanguage {
    let env_value = std::env::var("CRABCODE_MEMORY_LANGUAGE").ok();
    let config_json = std::fs::read_to_string(sibling_config_path(base_dir)).ok();
    resolve_from(env_value.as_deref(), config_json.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_language_matches_ts_word_list() {
        assert_eq!(parse_language("zh-CN"), Some(MemoryOutputLanguage::Zh));
        assert_eq!(parse_language(" chinese "), Some(MemoryOutputLanguage::Zh));
        assert_eq!(parse_language("EN-us"), Some(MemoryOutputLanguage::En));
        assert_eq!(parse_language("english"), Some(MemoryOutputLanguage::En));
        assert_eq!(parse_language("fr"), None);
        assert_eq!(parse_language(""), None);
    }

    #[test]
    fn env_wins_over_config() {
        let config = r#"{"memoryLanguage":"zh","uiLanguage":"zh-CN"}"#;
        assert_eq!(
            resolve_from(Some("en"), Some(config)),
            MemoryOutputLanguage::En
        );
    }

    #[test]
    fn memory_language_beats_ui_language_and_auto_falls_through() {
        assert_eq!(
            resolve_from(
                None,
                Some(r#"{"memoryLanguage":"en","uiLanguage":"zh-CN"}"#)
            ),
            MemoryOutputLanguage::En
        );
        // auto（非法值同理）落到 uiLanguage。
        assert_eq!(
            resolve_from(
                None,
                Some(r#"{"memoryLanguage":"auto","uiLanguage":"en-US"}"#)
            ),
            MemoryOutputLanguage::En
        );
        assert_eq!(
            resolve_from(None, Some(r#"{"uiLanguage":"zh-CN"}"#)),
            MemoryOutputLanguage::Zh
        );
    }

    #[test]
    fn defaults_to_zh_on_missing_or_malformed() {
        assert_eq!(resolve_from(None, None), MemoryOutputLanguage::Zh);
        assert_eq!(
            resolve_from(None, Some("not json")),
            MemoryOutputLanguage::Zh
        );
        assert_eq!(resolve_from(Some("fr"), None), MemoryOutputLanguage::Zh);
    }

    #[test]
    fn sibling_config_path_handles_both_layouts() {
        assert_eq!(
            sibling_config_path(Path::new("/home/u/.crabcode")),
            PathBuf::from("/home/u/.crabcode.json")
        );
        assert_eq!(
            sibling_config_path(Path::new("/custom/dir")),
            PathBuf::from("/custom/dir/.crabcode.json")
        );
    }
}
