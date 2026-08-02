//! 命令家族路由 — 根据 `CommandFamily` 选择执行策略
//!
//! 每个命令家族有不同的默认超时、沙箱需求、shell 包装需求和输出限制。
//! 本模块负责将 `CommandFamily` 映射到具体的 FamilyConfig，并解析可执行文件路径。

use std::path::PathBuf;

use acosmi_types::exec_types::CommandFamily;

/// 命令家族执行配置
#[derive(Debug, Clone)]
pub struct FamilyConfig {
    /// 默认超时时间（毫秒）
    pub default_timeout_ms: u64,
    /// 是否要求沙箱执行
    pub require_sandbox: bool,
    /// 是否需要通过 shell 包装执行
    pub shell_wrap: bool,
    /// 最大输出字节数
    pub max_output_bytes: usize,
}

/// 10 MB 输出限制
const OUTPUT_10MB: usize = 10 * 1024 * 1024;
/// 1 MB 输出限制
const OUTPUT_1MB: usize = 1024 * 1024;
/// 5 MB 输出限制
const OUTPUT_5MB: usize = 5 * 1024 * 1024;

/// 根据命令家族获取默认执行配置
///
/// 各家族策略：
/// - Git: 30s 超时，无沙箱，无 shell 包装，10MB 输出
/// - Gh: 60s 超时，无沙箱，无 shell 包装，5MB 输出
/// - Ssh: 30s 超时，无沙箱，无 shell 包装，1MB 输出
/// - Tmux: 10s 超时，无沙箱，无 shell 包装，1MB 输出
/// - Rg: 30s 超时，无沙箱，无 shell 包装，10MB 输出
/// - Npm/Pnpm: 120s 超时，需要沙箱，无 shell 包装，5MB 输出
/// - Shell: 120s 超时，需要沙箱，需要 shell 包装，1MB 输出
/// - `PlatformTool`: 60s 超时，需要沙箱，无 shell 包装，5MB 输出
/// - `LspServer`: 0（无超时），无沙箱，无 shell 包装，10MB 输出
/// - `McpServer`: 0（无超时），无沙箱，无 shell 包装，10MB 输出
/// - Other: 30s 超时，需要沙箱，无 shell 包装，1MB 输出
#[must_use]
pub const fn get_family_config(family: &CommandFamily) -> FamilyConfig {
    match family {
        CommandFamily::Git => FamilyConfig {
            default_timeout_ms: 30_000,
            require_sandbox: false,
            shell_wrap: false,
            max_output_bytes: OUTPUT_10MB,
        },
        CommandFamily::Gh => FamilyConfig {
            default_timeout_ms: 60_000,
            require_sandbox: false,
            shell_wrap: false,
            max_output_bytes: OUTPUT_5MB,
        },
        CommandFamily::Ssh => FamilyConfig {
            default_timeout_ms: 30_000,
            require_sandbox: false,
            shell_wrap: false,
            max_output_bytes: OUTPUT_1MB,
        },
        CommandFamily::Tmux => FamilyConfig {
            default_timeout_ms: 10_000,
            require_sandbox: false,
            shell_wrap: false,
            max_output_bytes: OUTPUT_1MB,
        },
        CommandFamily::Rg => FamilyConfig {
            default_timeout_ms: 30_000,
            require_sandbox: false,
            shell_wrap: false,
            max_output_bytes: OUTPUT_10MB,
        },
        CommandFamily::Npm | CommandFamily::Pnpm => FamilyConfig {
            default_timeout_ms: 120_000,
            require_sandbox: true,
            shell_wrap: false,
            max_output_bytes: OUTPUT_5MB,
        },
        CommandFamily::Shell => FamilyConfig {
            default_timeout_ms: 120_000,
            require_sandbox: true,
            shell_wrap: true,
            max_output_bytes: OUTPUT_1MB,
        },
        CommandFamily::PlatformTool => FamilyConfig {
            default_timeout_ms: 60_000,
            require_sandbox: true,
            shell_wrap: false,
            max_output_bytes: OUTPUT_5MB,
        },
        CommandFamily::LspServer => FamilyConfig {
            default_timeout_ms: 0, // 长驻进程，无默认超时
            require_sandbox: false,
            shell_wrap: false,
            max_output_bytes: OUTPUT_10MB,
        },
        CommandFamily::McpServer => FamilyConfig {
            default_timeout_ms: 0, // 长驻进程，无默认超时
            require_sandbox: false,
            shell_wrap: false,
            max_output_bytes: OUTPUT_10MB,
        },
        CommandFamily::Other(_) => FamilyConfig {
            default_timeout_ms: 30_000,
            require_sandbox: true,
            shell_wrap: false,
            max_output_bytes: OUTPUT_1MB,
        },
    }
}

/// 解析可执行文件的完整路径
///
/// 优先使用 `program` 参数指定的路径。如果 `program` 不是绝对路径，
/// 则在系统 PATH 中查找对应的可执行文件。
///
/// 对于特定家族，会尝试使用已知的默认程序名：
/// - Git -> "git"
/// - Gh -> "gh"
/// - Ssh -> "ssh"
/// - Rg -> "rg"
/// - Npm -> "npm"
/// - Pnpm -> "pnpm"
/// - Tmux -> "tmux"
pub fn resolve_program(family: &CommandFamily, program: &str) -> Result<PathBuf, ResolveError> {
    let path = PathBuf::from(program);

    // 如果已经是绝对路径，直接检查是否存在
    if path.is_absolute() {
        if path.exists() {
            return Ok(path);
        }
        return Err(ResolveError::NotFound {
            program: program.to_string(),
        });
    }

    // 在 PATH 中查找
    if let Ok(found) = which::which(program) {
        return Ok(found);
    }

    // 如果 program 为空或查找失败，尝试用家族默认程序名
    let default_name = family_default_program(family);
    if let Some(name) = default_name
        && name != program
        && let Ok(found) = which::which(name)
    {
        return Ok(found);
    }

    Err(ResolveError::NotFound {
        program: program.to_string(),
    })
}

/// 根据命令家族返回默认的程序名
const fn family_default_program(family: &CommandFamily) -> Option<&'static str> {
    match family {
        CommandFamily::Git => Some("git"),
        CommandFamily::Gh => Some("gh"),
        CommandFamily::Ssh => Some("ssh"),
        CommandFamily::Tmux => Some("tmux"),
        CommandFamily::Rg => Some("rg"),
        CommandFamily::Npm => Some("npm"),
        CommandFamily::Pnpm => Some("pnpm"),
        CommandFamily::Shell
        | CommandFamily::PlatformTool
        | CommandFamily::LspServer
        | CommandFamily::McpServer
        | CommandFamily::Other(_) => None,
    }
}

/// 程序路径解析错误
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// 在 PATH 中未找到可执行文件
    #[error("可执行文件未找到: {program}")]
    NotFound { program: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_family_config() {
        let config = get_family_config(&CommandFamily::Git);
        assert_eq!(config.default_timeout_ms, 30_000);
        assert!(!config.require_sandbox);
        assert!(!config.shell_wrap);
        assert_eq!(config.max_output_bytes, OUTPUT_10MB);
    }

    #[test]
    fn test_shell_family_config() {
        let config = get_family_config(&CommandFamily::Shell);
        assert_eq!(config.default_timeout_ms, 120_000);
        assert!(config.require_sandbox);
        assert!(config.shell_wrap);
        assert_eq!(config.max_output_bytes, OUTPUT_1MB);
    }

    #[test]
    fn test_other_family_config() {
        let config = get_family_config(&CommandFamily::Other("custom".into()));
        assert!(config.require_sandbox);
        assert_eq!(config.default_timeout_ms, 30_000);
    }

    #[test]
    fn test_resolve_nonexistent_program() {
        let result = resolve_program(
            &CommandFamily::Other("x".into()),
            "__nonexistent_program_xyz__",
        );
        assert!(result.is_err());
    }
}
