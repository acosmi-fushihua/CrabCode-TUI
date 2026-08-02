//! 进程拓扑 — 定义 supervisor 管理的进程配置模型
//!
//! 本模块定义进程标识、配置、重启策略以及 supervisor 全局配置。
//! 所有进程类型通过 `ProcessId` 开放标识，不再限于 Go/TypeScript 两种。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

// ────────────────────────── 进程标识 ──────────────────────────

/// 进程标识（开放标识，不限于固定枚举）
///
/// 任意字符串均可作为进程标识，由 supervisor 配置自行定义。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessId(pub String);

impl ProcessId {
    /// 创建进程标识
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// 默认 TypeScript 会话层进程标识
    #[must_use]
    pub fn ts_session() -> Self {
        Self("ts-session".to_string())
    }

    /// 原生 CrabCode TUI 前台进程标识。
    ///
    /// 它与 `ts-session` 是不同的生命周期角色：前者拥有终端并在内部启动
    /// 私有 StructuredIO 后端，后者本身就是 legacy TypeScript 前台。
    #[must_use]
    pub fn native_tui() -> Self {
        Self("native-tui".to_string())
    }

    /// Phase 0.5 memory orchestrator 进程标识
    #[must_use]
    pub fn memory_orchestrator() -> Self {
        Self("memory-orchestrator".to_string())
    }

    /// 默认 Sandbox Worker 进程标识（P3-A 迁入 supervisor topology）
    #[must_use]
    pub fn sandbox_worker() -> Self {
        Self("sandbox-worker".to_string())
    }

    /// 获取标识字符串引用
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for ProcessId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for ProcessId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ────────────────────────── 进程行为策略 ──────────────────────────

/// 进程 I/O 策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StdioPolicy {
    /// 继承 supervisor 的终端 I/O（TUI 进程：stdin+stdout 继承，stderr 静默）
    InheritTerminal,
    /// 全静默（后台进程：stdin+stdout+stderr 全部重定向到 null）
    Silent,
    /// 捕获 stdout/stderr 转发到 tracing subscriber。
    /// stdin=null, stdout=piped, stderr=piped;supervisor 侧 per-line `tracing::warn!`
    /// 带 `child_id` + `stream` + `message` 字段。
    ///
    /// PR-C / Bug 7 修复: 原 Silent 策略把三流全丢 /dev/null,子进程启动期
    /// panic/abort 按设计零输出,supervisor `OnFailure` 重启循环彻底不可诊断。
    /// Captured 在不继承终端的前提下保留可观测性,适用于后台服务。
    Captured,
}

/// IPC 通信类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcType {
    /// 通过 UDS (Unix) / Named Pipe (Windows)
    Socket,
    /// 通过 stdin/stdout 管道
    Stdio,
    /// 无 IPC
    None,
}

/// 进程组策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessGroupPolicy {
    /// 留在 supervisor 的前台进程组（TUI 进程需要终端访问）
    Foreground,
    /// 创建独立进程组（后台服务，便于整组终止）
    Background,
}

// ────────────────────────── 重启策略 ──────────────────────────

/// 重启策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// 总是重启（无论退出码），立即重启
    Immediate,
    /// 仅在异常退出时重启（exit code != 0）
    OnFailure,
    /// `指数退避重启（base_ms` `起始延迟，max_ms` 最大延迟）
    ExponentialBackoff { base_ms: u64, max_ms: u64 },
    /// 最多重启 N 次（窗口期内，超过则放弃）
    MaxRetries(u32),
    /// 不重启
    Never,
}

// ────────────────────────── 进程配置 ──────────────────────────

/// 进程配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessConfig {
    /// 进程标识
    pub id: ProcessId,
    /// 可执行文件路径
    pub binary: String,
    /// 仅供当前进程启动使用的原生平台可执行文件路径。
    ///
    /// `binary` 保持既有可序列化 Unicode 配置契约；原生 TUI 等由当前进程
    /// 直接构造的拓扑用本字段承载 Unix 非 UTF-8 路径，避免 display/lossy
    /// 转换改变实际路径字节。存在时，进程启动必须优先使用本字段。
    #[serde(skip)]
    pub binary_os: Option<OsString>,
    /// 启动参数
    #[serde(default)]
    pub args: Vec<String>,
    /// 额外环境变量
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// 仅供当前进程启动使用的原生平台环境变量。
    ///
    /// `env` 保持原有可序列化 Unicode 配置契约；本字段只承载无法无损表示为
    /// `String` 的 shell 环境。它不得进入 TOML/JSON，也不得用 lossy 转换生成。
    #[serde(skip)]
    pub env_os: HashMap<OsString, OsString>,
    /// 是否继承 supervisor 自身的完整环境。
    ///
    /// 默认保持历史行为。安全边界前的原生 TUI 启动拓扑会关闭继承，并在
    /// `env` 中显式传入经过过滤的 shell 环境，防止陈旧 bootstrap/settings
    /// 信封越过 workspace trust 边界。该开关只属于进程内原生拓扑，不进入
    /// 既有 TOML/JSON 配置 schema；反序列化始终恢复历史默认值。
    #[serde(skip, default = "default_inherit_parent_env")]
    pub inherit_parent_env: bool,
    /// 工作目录（None 表示继承 supervisor 的工作目录）
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// 重启策略
    pub restart_policy: RestartPolicy,
    /// 健康检查间隔
    #[serde(with = "humantime_serde_compat")]
    pub health_check_interval: Duration,
    /// I/O 策略
    pub stdio_policy: StdioPolicy,
    /// IPC 通信类型
    pub ipc_type: IpcType,
    /// 进程组策略
    pub process_group: ProcessGroupPolicy,
    /// 启动依赖（在这些进程启动后才启动本进程）
    #[serde(default)]
    pub depends_on: Vec<ProcessId>,
}

const fn default_inherit_parent_env() -> bool {
    true
}

// ────────────────────────── Supervisor 全局配置 ──────────────────────────

/// Supervisor 全局配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorConfig {
    /// 进程配置列表（按启动顺序排列）
    pub processes: Vec<ProcessConfig>,
    /// 优雅关闭超时（超时后强制 kill）
    #[serde(with = "humantime_serde_compat")]
    pub shutdown_timeout: Duration,
    /// 重启计数窗口期内最大重启次数（超过则放弃重启）
    pub max_restart_count: u32,
    /// 重启计数窗口期
    #[serde(with = "humantime_serde_compat")]
    pub restart_window: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            processes: Vec::new(),
            shutdown_timeout: Duration::from_secs(10),
            max_restart_count: 5,
            restart_window: Duration::from_secs(60),
        }
    }
}

impl SupervisorConfig {
    /// 创建默认的开发环境配置
    ///
    /// M6.1 (2026-05-03)：Go 终下线，supervisor 仅管 TS 会话进程。
    /// 历史 hub 子进程已归档至 archive/2026-05-03-go-final-sweep/。
    #[must_use]
    pub fn development() -> Self {
        let ts_config = ProcessConfig {
            id: ProcessId::ts_session(),
            binary: "node".to_string(),
            binary_os: None,
            args: vec!["dist/index.js".to_string()],
            env: HashMap::new(),
            env_os: HashMap::new(),
            inherit_parent_env: true,
            cwd: None,
            restart_policy: RestartPolicy::OnFailure,
            health_check_interval: Duration::from_secs(5),
            stdio_policy: StdioPolicy::InheritTerminal,
            ipc_type: IpcType::None,
            process_group: ProcessGroupPolicy::Foreground,
            depends_on: vec![],
        };

        Self {
            processes: vec![ts_config],
            shutdown_timeout: Duration::from_secs(10),
            max_restart_count: 5,
            restart_window: Duration::from_secs(60),
        }
    }

    /// 返回停止顺序的进程配置（逆序）
    #[must_use]
    pub fn shutdown_order(&self) -> Vec<&ProcessConfig> {
        self.processes.iter().rev().collect()
    }

    /// 返回启动顺序的进程 ID 列表
    ///
    /// 如果进程之间没有 `depends_on` 依赖，则按配置文件中的顺序。
    /// 如果有依赖关系，则使用拓扑排序。
    pub fn start_order(&self) -> Vec<ProcessId> {
        let has_deps = self.processes.iter().any(|p| !p.depends_on.is_empty());
        if !has_deps {
            return self.processes.iter().map(|c| c.id.clone()).collect();
        }
        match topological_sort(&self.processes) {
            Ok(order) => order,
            Err(e) => {
                tracing::error!(error = %e, "拓扑排序失败，回退到配置顺序");
                self.processes.iter().map(|c| c.id.clone()).collect()
            }
        }
    }

    /// 从 TOML 字符串解析配置
    pub fn from_toml(toml_str: &str) -> Result<Self, ConfigError> {
        toml::from_str(toml_str).map_err(ConfigError::TomlParse)
    }

    /// 从文件加载配置
    ///
    /// 加载优先级：
    /// 1. 环境变量 `ACOSMI_SUPERVISOR_CONFIG` 指定的路径
    /// 2. `~/.config/acosmi/processes.toml`
    /// 3. 返回 None（使用内嵌默认配置）
    pub fn load_from_file() -> Result<Option<Self>, ConfigError> {
        // 优先级 1：环境变量
        if let Ok(path) = std::env::var("ACOSMI_SUPERVISOR_CONFIG") {
            let path = PathBuf::from(path);
            if path.exists() {
                let content = std::fs::read_to_string(&path)
                    .map_err(|e| ConfigError::FileRead(path.clone(), e))?;
                let config = Self::from_toml(&content)?;
                config.validate()?;
                tracing::info!(path = %path.display(), "从环境变量指定路径加载 supervisor 配置");
                return Ok(Some(config));
            }
            tracing::warn!(path = %path.display(), "ACOSMI_SUPERVISOR_CONFIG 指定的文件不存在");
        }

        // 优先级 2：默认配置文件路径
        if let Some(config_dir) = dirs::config_dir() {
            let path = config_dir.join("acosmi").join("processes.toml");
            if path.exists() {
                let content = std::fs::read_to_string(&path)
                    .map_err(|e| ConfigError::FileRead(path.clone(), e))?;
                let config = Self::from_toml(&content)?;
                config.validate()?;
                tracing::info!(path = %path.display(), "从默认路径加载 supervisor 配置");
                return Ok(Some(config));
            }
        }

        // 优先级 3：无配置文件
        Ok(None)
    }

    /// 验证配置的一致性
    pub fn validate(&self) -> Result<(), ConfigError> {
        // 检查进程 ID 非空
        for process in &self.processes {
            if process.id.as_str().is_empty() {
                return Err(ConfigError::EmptyProcessId);
            }
        }

        // 检查进程 ID 唯一性
        let mut seen = std::collections::HashSet::new();
        for process in &self.processes {
            if !seen.insert(&process.id) {
                return Err(ConfigError::DuplicateProcessId(process.id.clone()));
            }
        }

        // 检查 depends_on 引用的进程是否存在
        let all_ids: std::collections::HashSet<&ProcessId> =
            self.processes.iter().map(|p| &p.id).collect();
        for process in &self.processes {
            for dep in &process.depends_on {
                if !all_ids.contains(dep) {
                    return Err(ConfigError::UnknownDependency {
                        process: process.id.clone(),
                        dependency: dep.clone(),
                    });
                }
            }
        }

        // 检查循环依赖
        topological_sort(&self.processes)?;

        Ok(())
    }
}

// ────────────────────────── 配置错误 ──────────────────────────

/// 配置错误类型
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// TOML 解析错误
    #[error("TOML 解析错误: {0}")]
    TomlParse(#[from] toml::de::Error),

    /// 文件读取错误
    #[error("读取配置文件 {0} 失败: {1}")]
    FileRead(PathBuf, std::io::Error),

    /// 重复的进程 ID
    #[error("重复的进程 ID: {0}")]
    DuplicateProcessId(ProcessId),

    /// 未知的依赖进程
    #[error("进程 {process} 依赖不存在的进程 {dependency}")]
    UnknownDependency {
        process: ProcessId,
        dependency: ProcessId,
    },

    /// 空进程 ID
    #[error("进程 ID 不能为空")]
    EmptyProcessId,

    /// 循环依赖
    #[error("检测到循环依赖: {0}")]
    CyclicDependency(String),
}

// ────────────────────────── 拓扑排序 ──────────────────────────

/// 对进程配置进行拓扑排序（Kahn 算法）
///
/// 返回按依赖关系排序后的进程 ID 列表。
/// 如果存在循环依赖，返回错误。
fn topological_sort(processes: &[ProcessConfig]) -> Result<Vec<ProcessId>, ConfigError> {
    let mut in_degree: HashMap<&ProcessId, usize> = HashMap::new();
    let mut adj: HashMap<&ProcessId, Vec<&ProcessId>> = HashMap::new();

    // 初始化
    for p in processes {
        let _ = in_degree.entry(&p.id).or_insert(0);
        let _ = adj.entry(&p.id).or_default();
    }

    // 构建图：depends_on[dep] → p （dep 完成后 p 才能启动）
    for p in processes {
        for dep in &p.depends_on {
            adj.entry(dep).or_default().push(&p.id);
            *in_degree.entry(&p.id).or_insert(0) += 1;
        }
    }

    // Kahn 算法
    let mut queue: std::collections::VecDeque<&ProcessId> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(id, _)| *id)
        .collect();

    // 稳定排序：按 processes 列表中的原始顺序处理入度为 0 的节点
    let id_order: HashMap<&ProcessId, usize> = processes
        .iter()
        .enumerate()
        .map(|(i, p)| (&p.id, i))
        .collect();
    let mut queue_vec: Vec<&ProcessId> = queue.drain(..).collect();
    queue_vec.sort_by_key(|id| id_order.get(id).copied().unwrap_or(usize::MAX));
    queue.extend(queue_vec);

    let mut result = Vec::with_capacity(processes.len());

    while let Some(node) = queue.pop_front() {
        result.push(node.clone());

        if let Some(neighbors) = adj.get(node) {
            // 收集并排序邻居以保证稳定输出
            let mut sorted_neighbors: Vec<&&ProcessId> = neighbors.iter().collect();
            sorted_neighbors.sort_by_key(|id| id_order.get(*id).copied().unwrap_or(usize::MAX));

            for neighbor in sorted_neighbors {
                if let Some(deg) = in_degree.get_mut(neighbor) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(neighbor);
                    }
                }
            }
        }
    }

    if result.len() != processes.len() {
        // 找出参与循环的进程
        let sorted_set: std::collections::HashSet<&ProcessId> = result.iter().collect();
        let cycle_members: Vec<String> = processes
            .iter()
            .filter(|p| !sorted_set.contains(&p.id))
            .map(|p| p.id.0.clone())
            .collect();
        return Err(ConfigError::CyclicDependency(cycle_members.join(" → ")));
    }

    Ok(result)
}

/// Duration 的 serde 兼容模块（以秒为单位序列化）
mod humantime_serde_compat {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        duration.as_secs_f64().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = f64::deserialize(deserializer)?;
        if !secs.is_finite() || secs < 0.0 {
            return Err(serde::de::Error::custom("duration 必须为非负有限数值"));
        }
        Ok(Duration::from_secs_f64(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_id_equality() {
        let a = ProcessId::new("ts-session");
        let b = ProcessId::ts_session();
        assert_eq!(a, b);
    }

    #[test]
    fn test_process_id_display() {
        let id = ProcessId::new("my-service");
        assert_eq!(format!("{id}"), "my-service");
    }

    #[test]
    fn test_process_id_accepts_arbitrary_strings() {
        let id = ProcessId::new("custom-worker-3");
        assert_eq!(id.as_str(), "custom-worker-3");
    }

    #[test]
    fn test_default_config() {
        let config = SupervisorConfig::default();
        assert!(config.processes.is_empty());
        assert_eq!(config.shutdown_timeout, Duration::from_secs(10));
        assert_eq!(config.max_restart_count, 5);
    }

    #[test]
    fn test_development_config() {
        // M6.1: development() 仅 ts-session 单进程。
        let config = SupervisorConfig::development();
        assert_eq!(config.processes.len(), 1);
        assert_eq!(config.processes[0].id, ProcessId::ts_session());
        assert!(config.processes[0].depends_on.is_empty());
    }

    #[test]
    fn test_shutdown_order() {
        let config = SupervisorConfig::development();
        let order = config.shutdown_order();
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].id, ProcessId::ts_session());
    }

    #[test]
    fn test_stdio_policy_config() {
        let config = SupervisorConfig::development();
        assert_eq!(
            config.processes[0].stdio_policy,
            StdioPolicy::InheritTerminal
        );
    }

    /// PR-C / Bug 7: Captured 变体序列化走 snake_case,与 InheritTerminal/Silent 对齐。
    #[test]
    fn test_stdio_policy_captured_serde_round_trip() {
        let captured = StdioPolicy::Captured;
        let json = serde_json::to_string(&captured).expect("serialize");
        assert_eq!(json, r#""captured""#);
        let back: StdioPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, StdioPolicy::Captured);
    }

    /// PR-C: TOML 也能正确解析 "captured"。
    #[test]
    fn test_stdio_policy_captured_toml_parse() {
        let toml_str = r#"
            shutdown_timeout = 10.0
            max_restart_count = 5
            restart_window = 60.0

            [[processes]]
            id = "p1"
            binary = "/bin/echo"
            args = []
            restart_policy = "on_failure"
            health_check_interval = 5.0
            stdio_policy = "captured"
            ipc_type = "none"
            process_group = "background"
        "#;
        let config = SupervisorConfig::from_toml(toml_str).expect("TOML 解析");
        assert_eq!(config.processes[0].stdio_policy, StdioPolicy::Captured);
        assert!(
            config.processes[0].inherit_parent_env,
            "legacy TOML without inherit_parent_env must preserve environment inheritance"
        );
        assert!(config.processes[0].binary_os.is_none());
    }

    #[test]
    fn legacy_json_process_config_defaults_to_parent_environment_inheritance() {
        let process: ProcessConfig = serde_json::from_value(serde_json::json!({
            "id": "legacy",
            "binary": "echo",
            "args": [],
            "env": {},
            "cwd": null,
            "restart_policy": "never",
            "health_check_interval": 0.0,
            "stdio_policy": "silent",
            "ipc_type": "none",
            "process_group": "background",
            "depends_on": []
        }))
        .expect("legacy JSON process config");
        assert!(process.inherit_parent_env);
        assert!(process.binary_os.is_none());
        assert!(process.env_os.is_empty());
    }

    #[test]
    fn native_process_fields_never_enter_serialized_config() {
        let mut process = SupervisorConfig::development().processes.remove(0);
        process.binary_os = Some(OsString::from("NATIVE_PROGRAM"));
        let _ = process
            .env_os
            .insert(OsString::from("NATIVE_ONLY"), OsString::from("value"));
        process.inherit_parent_env = false;

        let serialized = serde_json::to_value(&process).expect("serialize process config");
        assert!(serialized.get("binary_os").is_none());
        assert!(serialized.get("env_os").is_none());
        assert!(serialized.get("inherit_parent_env").is_none());
        let restored: ProcessConfig =
            serde_json::from_value(serialized).expect("deserialize process config");
        assert!(restored.binary_os.is_none());
        assert!(restored.env_os.is_empty());
        assert!(
            restored.inherit_parent_env,
            "deserialized public config must retain the historical inheritance default"
        );
    }

    #[test]
    fn test_process_group_policy() {
        let config = SupervisorConfig::development();
        assert_eq!(
            config.processes[0].process_group,
            ProcessGroupPolicy::Foreground
        );
    }

    #[test]
    fn test_ipc_type_config() {
        let config = SupervisorConfig::development();
        assert_eq!(config.processes[0].ipc_type, IpcType::None);
    }

    // ── S4-C2: 依赖拓扑排序测试 ──

    fn make_process(id: &str, deps: Vec<&str>) -> ProcessConfig {
        ProcessConfig {
            id: ProcessId::new(id),
            binary: "echo".to_string(),
            binary_os: None,
            args: vec![],
            env: HashMap::new(),
            env_os: HashMap::new(),
            inherit_parent_env: true,
            cwd: None,
            restart_policy: RestartPolicy::OnFailure,
            health_check_interval: Duration::from_secs(5),
            stdio_policy: StdioPolicy::Silent,
            ipc_type: IpcType::None,
            process_group: ProcessGroupPolicy::Background,
            depends_on: deps.into_iter().map(ProcessId::new).collect(),
        }
    }

    #[test]
    fn test_topological_sort_no_deps() {
        let procs = vec![
            make_process("a", vec![]),
            make_process("b", vec![]),
            make_process("c", vec![]),
        ];
        let order = topological_sort(&procs).expect("应成功排序");
        // 无依赖时保持原始顺序
        assert_eq!(
            order,
            vec![
                ProcessId::new("a"),
                ProcessId::new("b"),
                ProcessId::new("c"),
            ]
        );
    }

    #[test]
    fn test_topological_sort_linear_chain() {
        // a → b → c（c 依赖 b，b 依赖 a）
        let procs = vec![
            make_process("c", vec!["b"]),
            make_process("b", vec!["a"]),
            make_process("a", vec![]),
        ];
        let order = topological_sort(&procs).expect("应成功排序");
        assert_eq!(
            order,
            vec![
                ProcessId::new("a"),
                ProcessId::new("b"),
                ProcessId::new("c"),
            ]
        );
    }

    #[test]
    fn test_topological_sort_diamond() {
        // a → b, a → c, b → d, c → d
        let procs = vec![
            make_process("a", vec![]),
            make_process("b", vec!["a"]),
            make_process("c", vec!["a"]),
            make_process("d", vec!["b", "c"]),
        ];
        let order = topological_sort(&procs).expect("应成功排序");
        // a 必须在 b,c 之前；b,c 必须在 d 之前
        let pos = |id: &str| order.iter().position(|p| p.0 == id).expect("应存在");
        assert!(pos("a") < pos("b"));
        assert!(pos("a") < pos("c"));
        assert!(pos("b") < pos("d"));
        assert!(pos("c") < pos("d"));
    }

    #[test]
    fn test_topological_sort_cycle_detection() {
        // a → b → c → a（循环）
        let procs = vec![
            make_process("a", vec!["c"]),
            make_process("b", vec!["a"]),
            make_process("c", vec!["b"]),
        ];
        let result = topological_sort(&procs);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ConfigError::CyclicDependency(_)));
    }

    #[test]
    fn test_topological_sort_three_plus_processes() {
        // 5 个进程，多层依赖
        let procs = vec![
            make_process("db", vec![]),
            make_process("cache", vec![]),
            make_process("api", vec!["db", "cache"]),
            make_process("worker", vec!["db"]),
            make_process("frontend", vec!["api"]),
        ];
        let order = topological_sort(&procs).expect("应成功排序");
        assert_eq!(order.len(), 5);
        let pos = |id: &str| order.iter().position(|p| p.0 == id).expect("应存在");
        assert!(pos("db") < pos("api"));
        assert!(pos("cache") < pos("api"));
        assert!(pos("db") < pos("worker"));
        assert!(pos("api") < pos("frontend"));
    }

    #[test]
    fn test_development_config_topo_sort() {
        // M6.1: development() 单进程拓扑，无依赖。
        let config = SupervisorConfig::development();
        let order = config.start_order();
        assert_eq!(order.len(), 1);
        assert_eq!(order[0], ProcessId::ts_session());
    }

    // ── S4-C2: 配置验证测试 ──

    #[test]
    fn test_validate_duplicate_process_id() {
        let config = SupervisorConfig {
            processes: vec![
                make_process("same-id", vec![]),
                make_process("same-id", vec![]),
            ],
            ..SupervisorConfig::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::DuplicateProcessId(_)
        ));
    }

    #[test]
    fn test_validate_unknown_dependency() {
        let config = SupervisorConfig {
            processes: vec![make_process("a", vec!["nonexistent"])],
            ..SupervisorConfig::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::UnknownDependency { .. }
        ));
    }

    #[test]
    fn test_validate_cyclic_dependency() {
        let config = SupervisorConfig {
            processes: vec![make_process("a", vec!["b"]), make_process("b", vec!["a"])],
            ..SupervisorConfig::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::CyclicDependency(_)
        ));
    }

    #[test]
    fn test_validate_valid_config() {
        let config = SupervisorConfig::development();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_process_id() {
        let config = SupervisorConfig {
            processes: vec![make_process("", vec![])],
            ..SupervisorConfig::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::EmptyProcessId));
    }

    // ── S4-C2: TOML 配置解析测试 ──

    #[test]
    fn test_from_toml_basic() {
        let toml_str = r#"
            shutdown_timeout = 15.0
            max_restart_count = 3
            restart_window = 120.0

            [[processes]]
            id = "my-backend"
            binary = "/usr/bin/my-app"
            args = ["serve", "--port", "8080"]
            restart_policy = "on_failure"
            health_check_interval = 10.0
            stdio_policy = "silent"
            ipc_type = "socket"
            process_group = "background"

            [[processes]]
            id = "my-frontend"
            binary = "node"
            args = ["dist/app.js"]
            restart_policy = "on_failure"
            health_check_interval = 5.0
            stdio_policy = "inherit_terminal"
            ipc_type = "none"
            process_group = "foreground"
            depends_on = ["my-backend"]
        "#;
        let config = SupervisorConfig::from_toml(toml_str).expect("TOML 解析应成功");
        assert_eq!(config.processes.len(), 2);
        assert_eq!(config.processes[0].id, ProcessId::new("my-backend"));
        assert_eq!(config.processes[1].id, ProcessId::new("my-frontend"));
        assert_eq!(
            config.processes[1].depends_on,
            vec![ProcessId::new("my-backend")]
        );
        assert_eq!(config.shutdown_timeout, Duration::from_secs(15));
        assert_eq!(config.max_restart_count, 3);
    }

    #[test]
    fn test_from_toml_three_processes() {
        let toml_str = r#"
            shutdown_timeout = 10.0
            max_restart_count = 5
            restart_window = 60.0

            [[processes]]
            id = "db"
            binary = "postgres"
            args = []
            restart_policy = "immediate"
            health_check_interval = 5.0
            stdio_policy = "silent"
            ipc_type = "none"
            process_group = "background"

            [[processes]]
            id = "api"
            binary = "api-server"
            args = ["--port", "3000"]
            restart_policy = "on_failure"
            health_check_interval = 5.0
            stdio_policy = "silent"
            ipc_type = "socket"
            process_group = "background"
            depends_on = ["db"]

            [[processes]]
            id = "worker"
            binary = "worker"
            args = []
            restart_policy = { max_retries = 3 }
            health_check_interval = 10.0
            stdio_policy = "silent"
            ipc_type = "none"
            process_group = "background"
            depends_on = ["db", "api"]
        "#;
        let config = SupervisorConfig::from_toml(toml_str).expect("TOML 解析应成功");
        assert_eq!(config.processes.len(), 3);

        let order = config.start_order();
        let pos = |id: &str| order.iter().position(|p| p.0 == id).expect("应存在");
        assert!(pos("db") < pos("api"));
        assert!(pos("db") < pos("worker"));
        assert!(pos("api") < pos("worker"));
    }

    #[test]
    fn test_from_toml_restart_policies() {
        let toml_str = r#"
            shutdown_timeout = 10.0
            max_restart_count = 5
            restart_window = 60.0

            [[processes]]
            id = "p1"
            binary = "a"
            args = []
            restart_policy = "immediate"
            health_check_interval = 5.0
            stdio_policy = "silent"
            ipc_type = "none"
            process_group = "background"

            [[processes]]
            id = "p2"
            binary = "b"
            args = []
            restart_policy = "never"
            health_check_interval = 5.0
            stdio_policy = "silent"
            ipc_type = "none"
            process_group = "background"

            [[processes]]
            id = "p3"
            binary = "c"
            args = []
            restart_policy = { exponential_backoff = { base_ms = 100, max_ms = 30000 } }
            health_check_interval = 5.0
            stdio_policy = "silent"
            ipc_type = "none"
            process_group = "background"
        "#;
        let config = SupervisorConfig::from_toml(toml_str).expect("TOML 解析应成功");
        assert_eq!(config.processes[0].restart_policy, RestartPolicy::Immediate);
        assert_eq!(config.processes[1].restart_policy, RestartPolicy::Never);
        assert_eq!(
            config.processes[2].restart_policy,
            RestartPolicy::ExponentialBackoff {
                base_ms: 100,
                max_ms: 30000
            }
        );
    }
}
