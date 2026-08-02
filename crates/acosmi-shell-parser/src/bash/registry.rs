//! 命令规格注册表
//!
//! 等价于 TS registry.ts + specs/*.ts

use crate::{Argument, CmdOption, CommandSpec};

/// 获取所有内置命令规格
#[must_use]
pub fn builtin_specs() -> Vec<CommandSpec> {
    vec![
        alias_spec(),
        timeout_spec(),
        sleep_spec(),
        nohup_spec(),
        time_spec(),
        srun_spec(),
        pyright_spec(),
    ]
}

/// 根据命令名查找规格
#[must_use]
pub fn find_spec(name: &str) -> Option<CommandSpec> {
    builtin_specs().into_iter().find(|s| s.name == name)
}

fn alias_spec() -> CommandSpec {
    CommandSpec {
        name: "alias".to_string(),
        description: Some("Create or list command aliases".to_string()),
        subcommands: None,
        args: Some(vec![Argument {
            name: Some("definition".to_string()),
            description: Some("Alias definition (name=value)".to_string()),
            is_variadic: true,
            is_optional: true,
            ..Default::default()
        }]),
        options: None,
    }
}

fn timeout_spec() -> CommandSpec {
    CommandSpec {
        name: "timeout".to_string(),
        description: Some("Run command with time limit".to_string()),
        subcommands: None,
        args: Some(vec![
            Argument {
                name: Some("duration".to_string()),
                description: Some("Time limit".to_string()),
                ..Default::default()
            },
            Argument {
                name: Some("command".to_string()),
                description: Some("Command to run".to_string()),
                is_command: true,
                ..Default::default()
            },
        ]),
        options: None,
    }
}

fn sleep_spec() -> CommandSpec {
    CommandSpec {
        name: "sleep".to_string(),
        description: Some("Delay for specified time".to_string()),
        subcommands: None,
        args: Some(vec![Argument {
            name: Some("duration".to_string()),
            description: Some("Duration to sleep".to_string()),
            ..Default::default()
        }]),
        options: None,
    }
}

fn nohup_spec() -> CommandSpec {
    CommandSpec {
        name: "nohup".to_string(),
        description: Some("Run command immune to hangups".to_string()),
        subcommands: None,
        args: Some(vec![Argument {
            name: Some("command".to_string()),
            description: Some("Command to run".to_string()),
            is_command: true,
            ..Default::default()
        }]),
        options: None,
    }
}

fn time_spec() -> CommandSpec {
    CommandSpec {
        name: "time".to_string(),
        description: Some("Time a command".to_string()),
        subcommands: None,
        args: Some(vec![Argument {
            name: Some("command".to_string()),
            description: Some("Command to time".to_string()),
            is_command: true,
            ..Default::default()
        }]),
        options: None,
    }
}

fn srun_spec() -> CommandSpec {
    CommandSpec {
        name: "srun".to_string(),
        description: Some("SLURM cluster runner".to_string()),
        subcommands: None,
        args: Some(vec![Argument {
            name: Some("command".to_string()),
            description: Some("Command to run".to_string()),
            is_command: true,
            ..Default::default()
        }]),
        options: Some(vec![
            CmdOption {
                name: vec!["-n".to_string(), "--ntasks".to_string()],
                description: Some("Number of tasks".to_string()),
                args: Some(vec![Argument {
                    name: Some("count".to_string()),
                    ..Default::default()
                }]),
                is_required: false,
            },
            CmdOption {
                name: vec!["-N".to_string(), "--nodes".to_string()],
                description: Some("Number of nodes".to_string()),
                args: Some(vec![Argument {
                    name: Some("count".to_string()),
                    ..Default::default()
                }]),
                is_required: false,
            },
        ]),
    }
}

fn pyright_spec() -> CommandSpec {
    CommandSpec {
        name: "pyright".to_string(),
        description: Some("Python type checker".to_string()),
        subcommands: None,
        args: Some(vec![Argument {
            name: Some("files".to_string()),
            description: Some("Files to check".to_string()),
            is_variadic: true,
            is_optional: true,
            ..Default::default()
        }]),
        options: Some(vec![
            CmdOption {
                name: vec!["--help".to_string()],
                description: Some("Show help".to_string()),
                args: None,
                is_required: false,
            },
            CmdOption {
                name: vec!["--version".to_string()],
                description: Some("Show version".to_string()),
                args: None,
                is_required: false,
            },
            CmdOption {
                name: vec!["--watch".to_string(), "-w".to_string()],
                description: Some("Watch mode".to_string()),
                args: None,
                is_required: false,
            },
            CmdOption {
                name: vec!["--project".to_string(), "-p".to_string()],
                description: Some("Project directory".to_string()),
                args: Some(vec![Argument {
                    name: Some("path".to_string()),
                    ..Default::default()
                }]),
                is_required: false,
            },
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_specs() {
        let specs = builtin_specs();
        assert_eq!(specs.len(), 7);
        assert!(specs.iter().any(|s| s.name == "timeout"));
    }

    #[test]
    fn test_find_spec() {
        assert!(find_spec("timeout").is_some());
        assert!(find_spec("nonexistent").is_none());
    }
}
