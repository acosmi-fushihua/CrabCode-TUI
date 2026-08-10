//! Renderer-owned `/usage` and plugin-management modal state.
//!
//! Source evidence (fixed CrabCode direct-TUI product commit
//! `2358212c2df2018816058c8a03b1ac3d324e74e0`):
//! `src/components/Settings/Usage.tsx` and
//! `src/commands/plugin/{PluginSettings,DiscoverPlugins,BrowseMarketplace,
//! ManagePlugins,ManageMarketplaces,PluginErrors,AddMarketplace,
//! ValidatePlugin,PluginTrustWarning}.tsx`.
//!
//! This module ports their renderer lifecycle onto the closed, process-private
//! 14-action/15-result direct-runtime lane.  It deliberately does not recreate
//! facts absent from that lane: plugin configuration/MCP schemas, secrets,
//! filesystem sizes, homepage URLs, raw loader errors, and organization-plan
//! policy details remain owned by their existing authorities.  No alternate
//! host transport, graphical surface, backend mutation, public protocol, or
//! process-exit behavior lives here.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::tui_app::UiLanguage;

const MAX_INPUT_CODE_UNITS: usize = 4096;
const MAX_INPUT_STORAGE_BYTES: usize = MAX_INPUT_CODE_UNITS * 4;
const MAX_SELECTOR_BYTES: usize = 512;
const MAX_MARKETPLACE_NAME_BYTES: usize = 256;
const MAX_WIRE_TEXT_CODE_UNITS: usize = 1024;
const MAX_PLUGIN_ROWS: usize = 512;
const MAX_PLUGIN_DIAGNOSTICS: usize = 256;
const MAX_MARKETPLACE_ROWS: usize = 128;
const MAX_INSTALLATIONS_PER_PLUGIN: usize = 16;
const MAX_TAGS_PER_PLUGIN: usize = 32;
const MAX_REVERSE_DEPENDENTS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PluginInstallScope {
    User,
    Project,
    Local,
}

impl PluginInstallScope {
    const ALL: [Self; 3] = [Self::User, Self::Project, Self::Local];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }

    fn label(self, language: UiLanguage) -> &'static str {
        match self {
            Self::User => language.text("用户", "User"),
            Self::Project => language.text("项目", "Project"),
            Self::Local => language.text("本地项目", "Local project"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PluginScope {
    User,
    Project,
    Local,
    Managed,
}

impl PluginScope {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
            Self::Managed => "managed",
        }
    }

    fn installable(self) -> Option<PluginInstallScope> {
        match self {
            Self::User => Some(PluginInstallScope::User),
            Self::Project => Some(PluginInstallScope::Project),
            Self::Local => Some(PluginInstallScope::Local),
            Self::Managed => None,
        }
    }

    fn label(self, language: UiLanguage) -> &'static str {
        match self {
            Self::User => language.text("用户", "User"),
            Self::Project => language.text("项目", "Project"),
            Self::Local => language.text("本地项目", "Local project"),
            Self::Managed => language.text("组织托管", "Managed"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PluginConfiguredScope {
    User,
    Project,
    Local,
    Managed,
    Flag,
    Builtin,
}

impl PluginConfiguredScope {
    fn label(self, language: UiLanguage) -> &'static str {
        match self {
            Self::User => language.text("用户设置", "User settings"),
            Self::Project => language.text("项目设置", "Project settings"),
            Self::Local => language.text("本地设置", "Local settings"),
            Self::Managed => language.text("组织托管", "Managed"),
            Self::Flag => language.text("命令行参数", "Command-line flag"),
            Self::Builtin => language.text("内置", "Built in"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum MarketplaceSourceKind {
    #[serde(rename = "url")]
    Url,
    #[serde(rename = "github")]
    Github,
    #[serde(rename = "git")]
    Git,
    #[serde(rename = "npm")]
    Npm,
    #[serde(rename = "file")]
    File,
    #[serde(rename = "directory")]
    Directory,
    #[serde(rename = "hostPattern")]
    HostPattern,
    #[serde(rename = "pathPattern")]
    PathPattern,
    #[serde(rename = "settings")]
    Settings,
}

impl MarketplaceSourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Url => "url",
            Self::Github => "github",
            Self::Git => "git",
            Self::Npm => "npm",
            Self::File => "file",
            Self::Directory => "directory",
            Self::HostPattern => "hostPattern",
            Self::PathPattern => "pathPattern",
            Self::Settings => "settings",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PluginDiscoverEmptyReason {
    GitNotInstalled,
    AllBlockedByPolicy,
    PolicyRestrictsSources,
    AllMarketplacesFailed,
    NoMarketplacesConfigured,
    AllPluginsInstalled,
}

impl PluginDiscoverEmptyReason {
    fn lines(self, language: UiLanguage) -> [&'static str; 2] {
        match self {
            Self::GitNotInstalled => [
                language.text(
                    "安装插件市场需要 Git。",
                    "Git is required to install marketplaces.",
                ),
                language.text(
                    "请安装 git 后重启 CrabCode。",
                    "Please install git and restart CrabCode.",
                ),
            ],
            Self::AllBlockedByPolicy => [
                language.text(
                    "你的组织策略不允许添加任何外部插件市场。",
                    "Your organization policy does not allow external plugin marketplaces.",
                ),
                language.text("请联系你的管理员。", "Contact your administrator."),
            ],
            Self::PolicyRestrictsSources => [
                language.text(
                    "你的组织限制了可添加的插件市场。",
                    "Your organization restricts which plugin marketplaces can be added.",
                ),
                language.text(
                    "切换到「插件市场」标签页查看允许的来源。",
                    "Switch to the Marketplaces tab to see allowed sources.",
                ),
            ],
            Self::AllMarketplacesFailed => [
                language.text("加载插件市场数据失败。", "Failed to load marketplace data."),
                language.text("请检查你的网络连接。", "Check your network connection."),
            ],
            Self::NoMarketplacesConfigured => [
                language.text("暂无可用插件。", "No plugins available."),
                language.text(
                    "请先在「插件市场」标签页添加一个插件市场。",
                    "Add a marketplace first using the Marketplaces tab.",
                ),
            ],
            Self::AllPluginsInstalled => [
                language.text(
                    "所有可用插件均已安装。",
                    "All available plugins are already installed.",
                ),
                language.text(
                    "稍后再来看看新插件，或添加更多插件市场。",
                    "Check for new plugins later or add more marketplaces.",
                ),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PluginLoadErrorType {
    PathNotFound,
    GitAuthFailed,
    GitTimeout,
    NetworkError,
    ManifestParseError,
    ManifestValidationError,
    PluginNotFound,
    MarketplaceNotFound,
    MarketplaceLoadFailed,
    McpConfigInvalid,
    McpServerSuppressedDuplicate,
    LspConfigInvalid,
    HookLoadFailed,
    ComponentLoadFailed,
    McpbDownloadFailed,
    McpbExtractFailed,
    McpbInvalidManifest,
    LspServerStartFailed,
    LspServerCrashed,
    LspRequestTimeout,
    LspRequestFailed,
    MarketplaceBlockedByPolicy,
    DependencyUnsatisfied,
    PluginCacheMiss,
    GenericError,
}

impl PluginLoadErrorType {
    fn label(self, language: UiLanguage) -> &'static str {
        match self {
            Self::PathNotFound => language.text("路径不存在", "Path not found"),
            Self::GitAuthFailed => language.text("Git 认证失败", "Git authentication failed"),
            Self::GitTimeout => language.text("Git 操作超时", "Git operation timed out"),
            Self::NetworkError => language.text("网络错误", "Network error"),
            Self::ManifestParseError => language.text("清单解析失败", "Manifest parse failed"),
            Self::ManifestValidationError => {
                language.text("清单校验失败", "Manifest validation failed")
            }
            Self::PluginNotFound => language.text("未找到插件", "Plugin not found"),
            Self::MarketplaceNotFound => language.text("未找到市场", "Marketplace not found"),
            Self::MarketplaceLoadFailed => language.text("市场加载失败", "Marketplace load failed"),
            Self::McpConfigInvalid => language.text("MCP 配置无效", "Invalid MCP config"),
            Self::McpServerSuppressedDuplicate => {
                language.text("MCP 服务重复", "Duplicate MCP server suppressed")
            }
            Self::LspConfigInvalid => language.text("LSP 配置无效", "Invalid LSP config"),
            Self::HookLoadFailed => language.text("钩子加载失败", "Hook load failed"),
            Self::ComponentLoadFailed => language.text("组件加载失败", "Component load failed"),
            Self::McpbDownloadFailed => language.text("MCPB 下载失败", "MCPB download failed"),
            Self::McpbExtractFailed => language.text("MCPB 解压失败", "MCPB extraction failed"),
            Self::McpbInvalidManifest => language.text("MCPB 清单无效", "Invalid MCPB manifest"),
            Self::LspServerStartFailed => {
                language.text("LSP 服务启动失败", "LSP server failed to start")
            }
            Self::LspServerCrashed => language.text("LSP 服务崩溃", "LSP server crashed"),
            Self::LspRequestTimeout => language.text("LSP 请求超时", "LSP request timed out"),
            Self::LspRequestFailed => language.text("LSP 请求失败", "LSP request failed"),
            Self::MarketplaceBlockedByPolicy => {
                language.text("市场被策略阻止", "Marketplace blocked by policy")
            }
            Self::DependencyUnsatisfied => {
                language.text("插件依赖未满足", "Plugin dependency unsatisfied")
            }
            Self::PluginCacheMiss => language.text("插件缓存缺失", "Plugin cache missing"),
            Self::GenericError => language.text("插件加载错误", "Plugin load error"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ValidationFileType {
    Plugin,
    Marketplace,
    Skill,
    Agent,
    Command,
    Hooks,
}

impl ValidationFileType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::Marketplace => "marketplace",
            Self::Skill => "skill",
            Self::Agent => "agent",
            Self::Command => "command",
            Self::Hooks => "hooks",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UsagePluginActionKind {
    UsageRead,
    UsageSetFiveHourContinue,
    PluginInventoryRead,
    PluginMarketplaceInventoryRead,
    PluginMarketplaceCatalogRead,
    PluginInstall,
    PluginUninstall,
    PluginSetEnabled,
    PluginUpdate,
    PluginMarketplaceAdd,
    PluginMarketplaceRemove,
    PluginMarketplaceUpdate,
    PluginMarketplaceSetAutoUpdate,
    PluginValidate,
}

impl UsagePluginActionKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::UsageRead => "usage_read",
            Self::UsageSetFiveHourContinue => "usage_set_five_hour_continue",
            Self::PluginInventoryRead => "plugin_inventory_read",
            Self::PluginMarketplaceInventoryRead => "plugin_marketplace_inventory_read",
            Self::PluginMarketplaceCatalogRead => "plugin_marketplace_catalog_read",
            Self::PluginInstall => "plugin_install",
            Self::PluginUninstall => "plugin_uninstall",
            Self::PluginSetEnabled => "plugin_set_enabled",
            Self::PluginUpdate => "plugin_update",
            Self::PluginMarketplaceAdd => "plugin_marketplace_add",
            Self::PluginMarketplaceRemove => "plugin_marketplace_remove",
            Self::PluginMarketplaceUpdate => "plugin_marketplace_update",
            Self::PluginMarketplaceSetAutoUpdate => "plugin_marketplace_set_auto_update",
            Self::PluginValidate => "plugin_validate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UsagePluginErrorCode {
    UsageUnavailable,
    UsageWriteRejected,
    PluginInventoryUnavailable,
    MarketplaceInventoryUnavailable,
    MarketplaceCatalogUnavailable,
    MarketplaceBlockedByPolicy,
    InvalidMarketplaceSource,
    PluginOperationRejected,
    MarketplaceOperationRejected,
    ValidationUnavailable,
    AuthorityFailure,
}

#[derive(Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum UsagePluginRuntimeAction {
    UsageRead,
    UsageSetFiveHourContinue {
        enabled: bool,
    },
    PluginInventoryRead,
    PluginMarketplaceInventoryRead,
    PluginMarketplaceCatalogRead {
        marketplace_name: String,
    },
    PluginInstall {
        plugin_id: String,
        scope: PluginInstallScope,
    },
    PluginUninstall {
        plugin_id: String,
        scope: PluginInstallScope,
        delete_data: bool,
    },
    PluginSetEnabled {
        plugin_id: String,
        enabled: bool,
        scope: Option<PluginInstallScope>,
    },
    PluginUpdate {
        plugin_id: String,
        scope: PluginScope,
    },
    PluginMarketplaceAdd {
        source_input: String,
    },
    PluginMarketplaceRemove {
        marketplace_name: String,
    },
    PluginMarketplaceUpdate {
        marketplace_name: String,
    },
    PluginMarketplaceSetAutoUpdate {
        marketplace_name: String,
        enabled: bool,
    },
    PluginValidate {
        path: String,
    },
}

impl fmt::Debug for UsagePluginRuntimeAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("UsagePluginRuntimeAction");
        debug.field("kind", &self.kind().as_str());
        match self {
            Self::PluginMarketplaceAdd { source_input } => {
                debug.field("source_input", &"[REDACTED]");
                debug.field("bytes", &source_input.len());
            }
            Self::PluginValidate { path } => {
                debug.field("path", &"[REDACTED]");
                debug.field("bytes", &path.len());
            }
            Self::PluginMarketplaceCatalogRead { marketplace_name }
            | Self::PluginMarketplaceRemove { marketplace_name }
            | Self::PluginMarketplaceUpdate { marketplace_name }
            | Self::PluginMarketplaceSetAutoUpdate {
                marketplace_name, ..
            } => {
                debug.field("marketplace_name", marketplace_name);
            }
            Self::PluginInstall {
                plugin_id, scope, ..
            }
            | Self::PluginUninstall {
                plugin_id, scope, ..
            } => {
                debug.field("plugin_id", plugin_id);
                debug.field("scope", scope);
            }
            Self::PluginSetEnabled { plugin_id, .. } | Self::PluginUpdate { plugin_id, .. } => {
                debug.field("plugin_id", plugin_id);
            }
            Self::UsageRead
            | Self::UsageSetFiveHourContinue { .. }
            | Self::PluginInventoryRead
            | Self::PluginMarketplaceInventoryRead => {}
        }
        debug.finish()
    }
}

impl UsagePluginRuntimeAction {
    pub(crate) const fn kind(&self) -> UsagePluginActionKind {
        match self {
            Self::UsageRead => UsagePluginActionKind::UsageRead,
            Self::UsageSetFiveHourContinue { .. } => {
                UsagePluginActionKind::UsageSetFiveHourContinue
            }
            Self::PluginInventoryRead => UsagePluginActionKind::PluginInventoryRead,
            Self::PluginMarketplaceInventoryRead => {
                UsagePluginActionKind::PluginMarketplaceInventoryRead
            }
            Self::PluginMarketplaceCatalogRead { .. } => {
                UsagePluginActionKind::PluginMarketplaceCatalogRead
            }
            Self::PluginInstall { .. } => UsagePluginActionKind::PluginInstall,
            Self::PluginUninstall { .. } => UsagePluginActionKind::PluginUninstall,
            Self::PluginSetEnabled { .. } => UsagePluginActionKind::PluginSetEnabled,
            Self::PluginUpdate { .. } => UsagePluginActionKind::PluginUpdate,
            Self::PluginMarketplaceAdd { .. } => UsagePluginActionKind::PluginMarketplaceAdd,
            Self::PluginMarketplaceRemove { .. } => UsagePluginActionKind::PluginMarketplaceRemove,
            Self::PluginMarketplaceUpdate { .. } => UsagePluginActionKind::PluginMarketplaceUpdate,
            Self::PluginMarketplaceSetAutoUpdate { .. } => {
                UsagePluginActionKind::PluginMarketplaceSetAutoUpdate
            }
            Self::PluginValidate { .. } => UsagePluginActionKind::PluginValidate,
        }
    }

    pub(crate) fn value(&self) -> Value {
        match self {
            Self::UsageRead => json!({ "kind": "usage_read" }),
            Self::UsageSetFiveHourContinue { enabled } => {
                json!({ "kind": "usage_set_five_hour_continue", "enabled": enabled })
            }
            Self::PluginInventoryRead => json!({ "kind": "plugin_inventory_read" }),
            Self::PluginMarketplaceInventoryRead => {
                json!({ "kind": "plugin_marketplace_inventory_read" })
            }
            Self::PluginMarketplaceCatalogRead { marketplace_name } => json!({
                "kind": "plugin_marketplace_catalog_read",
                "marketplace_name": marketplace_name,
            }),
            Self::PluginInstall { plugin_id, scope } => json!({
                "kind": "plugin_install",
                "plugin_id": plugin_id,
                "scope": scope.as_str(),
            }),
            Self::PluginUninstall {
                plugin_id,
                scope,
                delete_data,
            } => json!({
                "kind": "plugin_uninstall",
                "plugin_id": plugin_id,
                "scope": scope.as_str(),
                "delete_data": delete_data,
            }),
            Self::PluginSetEnabled {
                plugin_id,
                enabled,
                scope,
            } => json!({
                "kind": "plugin_set_enabled",
                "plugin_id": plugin_id,
                "enabled": enabled,
                "scope": scope.map(PluginInstallScope::as_str),
            }),
            Self::PluginUpdate { plugin_id, scope } => json!({
                "kind": "plugin_update",
                "plugin_id": plugin_id,
                "scope": scope.as_str(),
            }),
            Self::PluginMarketplaceAdd { source_input } => json!({
                "kind": "plugin_marketplace_add",
                "source_input": source_input,
            }),
            Self::PluginMarketplaceRemove { marketplace_name } => json!({
                "kind": "plugin_marketplace_remove",
                "marketplace_name": marketplace_name,
            }),
            Self::PluginMarketplaceUpdate { marketplace_name } => json!({
                "kind": "plugin_marketplace_update",
                "marketplace_name": marketplace_name,
            }),
            Self::PluginMarketplaceSetAutoUpdate {
                marketplace_name,
                enabled,
            } => json!({
                "kind": "plugin_marketplace_set_auto_update",
                "marketplace_name": marketplace_name,
                "enabled": enabled,
            }),
            Self::PluginValidate { path } => {
                json!({ "kind": "plugin_validate", "path": path })
            }
        }
    }
}

/// A required JSON member whose value may explicitly be `null`.
///
/// Plain `Option<T>` would also accept a missing member during serde struct
/// decoding.  The wrapper is intentionally not `Default`, so the closed wire
/// contract distinguishes missing from present-null.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequiredNullable<T>(pub(crate) Option<T>);

impl<'de, T> Deserialize<'de> for RequiredNullable<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RateLimitSnapshot {
    pub(crate) utilization: RequiredNullable<f64>,
    pub(crate) resets_at: RequiredNullable<String>,
    pub(crate) overridable: RequiredNullable<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtraUsageSnapshot {
    pub(crate) is_enabled: bool,
    pub(crate) monthly_limit: RequiredNullable<f64>,
    pub(crate) used_credits: RequiredNullable<f64>,
    pub(crate) utilization: RequiredNullable<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UtilizationSnapshot {
    pub(crate) five_hour: RequiredNullable<RateLimitSnapshot>,
    pub(crate) seven_day: RequiredNullable<RateLimitSnapshot>,
    pub(crate) extra_usage: RequiredNullable<ExtraUsageSnapshot>,
    pub(crate) five_hour_continue_enabled: RequiredNullable<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EntitlementBalanceSnapshot {
    pub(crate) total_token_quota: f64,
    pub(crate) total_token_used: f64,
    pub(crate) total_token_remaining: f64,
    pub(crate) total_call_quota: f64,
    pub(crate) total_call_used: f64,
    pub(crate) total_call_remaining: f64,
    pub(crate) active_entitlements: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginInstallationSnapshot {
    pub(crate) scope: PluginScope,
    pub(crate) version: RequiredNullable<String>,
    pub(crate) installed_at: RequiredNullable<String>,
    pub(crate) last_updated: RequiredNullable<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginInventoryEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) marketplace: String,
    pub(crate) description: RequiredNullable<String>,
    pub(crate) version: RequiredNullable<String>,
    pub(crate) is_builtin: bool,
    pub(crate) loaded: bool,
    pub(crate) enabled: bool,
    pub(crate) configured_scope: RequiredNullable<PluginConfiguredScope>,
    pub(crate) installations: Vec<PluginInstallationSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginLoadDiagnostic {
    #[serde(rename = "type")]
    pub(crate) error_type: PluginLoadErrorType,
    pub(crate) plugin_name: RequiredNullable<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarketplaceInventoryEntry {
    pub(crate) name: String,
    pub(crate) source_kind: MarketplaceSourceKind,
    pub(crate) last_updated: RequiredNullable<String>,
    pub(crate) plugin_count: RequiredNullable<u64>,
    pub(crate) installed_plugin_count: u64,
    pub(crate) auto_update: bool,
    pub(crate) load_failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarketplaceCatalogPlugin {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) display_name: String,
    pub(crate) description: RequiredNullable<String>,
    pub(crate) version: RequiredNullable<String>,
    pub(crate) category: RequiredNullable<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) globally_installed: bool,
    pub(crate) enabled: bool,
    pub(crate) install_count: RequiredNullable<u64>,
    pub(crate) installations: Vec<PluginInstallationSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginValidationDiagnostic {
    pub(crate) path: String,
    pub(crate) code: RequiredNullable<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum UsagePluginRuntimeResult {
    UsageSnapshot {
        utilization: RequiredNullable<UtilizationSnapshot>,
        entitlement_balance: RequiredNullable<EntitlementBalanceSnapshot>,
    },
    UsageFiveHourContinueUpdated {
        enabled: bool,
    },
    PluginInventorySnapshot {
        plugins: Vec<PluginInventoryEntry>,
        load_diagnostics: Vec<PluginLoadDiagnostic>,
        truncated: bool,
    },
    PluginMarketplaceInventorySnapshot {
        marketplaces: Vec<MarketplaceInventoryEntry>,
        empty_reason: PluginDiscoverEmptyReason,
        truncated: bool,
    },
    PluginMarketplaceCatalogSnapshot {
        marketplace_name: String,
        plugins: Vec<MarketplaceCatalogPlugin>,
        truncated: bool,
    },
    PluginInstalled {
        plugin_id: String,
        plugin_name: String,
        scope: PluginInstallScope,
    },
    PluginUninstalled {
        plugin_id: String,
        plugin_name: String,
        scope: PluginInstallScope,
        reverse_dependents: Vec<String>,
    },
    PluginEnabledStateUpdated {
        plugin_id: String,
        plugin_name: String,
        enabled: bool,
        scope: PluginInstallScope,
        reverse_dependents: Vec<String>,
    },
    PluginUpdated {
        plugin_id: String,
        scope: PluginScope,
        old_version: RequiredNullable<String>,
        new_version: RequiredNullable<String>,
        already_up_to_date: bool,
    },
    PluginMarketplaceAdded {
        marketplace_name: String,
        source_kind: MarketplaceSourceKind,
        already_materialized: bool,
    },
    PluginMarketplaceRemoved {
        marketplace_name: String,
    },
    PluginMarketplaceUpdated {
        marketplace_name: String,
        updated_plugin_ids: Vec<String>,
        plugin_update_failure_count: u64,
    },
    PluginMarketplaceAutoUpdateUpdated {
        marketplace_name: String,
        enabled: bool,
    },
    PluginValidationResult {
        success: bool,
        file_type: ValidationFileType,
        errors: Vec<PluginValidationDiagnostic>,
        warnings: Vec<PluginValidationDiagnostic>,
        related_result_count: u64,
        truncated: bool,
    },
    UsagePluginError {
        action_kind: UsagePluginActionKind,
        code: UsagePluginErrorCode,
        message: String,
    },
}

impl UsagePluginRuntimeResult {
    fn result_kind(&self) -> &'static str {
        match self {
            Self::UsageSnapshot { .. } => "usage_snapshot",
            Self::UsageFiveHourContinueUpdated { .. } => "usage_five_hour_continue_updated",
            Self::PluginInventorySnapshot { .. } => "plugin_inventory_snapshot",
            Self::PluginMarketplaceInventorySnapshot { .. } => {
                "plugin_marketplace_inventory_snapshot"
            }
            Self::PluginMarketplaceCatalogSnapshot { .. } => "plugin_marketplace_catalog_snapshot",
            Self::PluginInstalled { .. } => "plugin_installed",
            Self::PluginUninstalled { .. } => "plugin_uninstalled",
            Self::PluginEnabledStateUpdated { .. } => "plugin_enabled_state_updated",
            Self::PluginUpdated { .. } => "plugin_updated",
            Self::PluginMarketplaceAdded { .. } => "plugin_marketplace_added",
            Self::PluginMarketplaceRemoved { .. } => "plugin_marketplace_removed",
            Self::PluginMarketplaceUpdated { .. } => "plugin_marketplace_updated",
            Self::PluginMarketplaceAutoUpdateUpdated { .. } => {
                "plugin_marketplace_auto_update_updated"
            }
            Self::PluginValidationResult { .. } => "plugin_validation_result",
            Self::UsagePluginError { .. } => "usage_plugin_error",
        }
    }
}

pub(crate) fn parse_usage_plugin_runtime_result(
    value: Value,
) -> Result<UsagePluginRuntimeResult, String> {
    validate_result_object_shape(&value)?;
    let result = serde_json::from_value::<UsagePluginRuntimeResult>(value)
        .map_err(|error| format!("closed usage/plugin result decode failed: {error}"))?;
    validate_runtime_result(&result)?;
    Ok(result)
}

fn validate_result_object_shape(value: &Value) -> Result<(), String> {
    let object = exact_object(value, &["kind"], &[], "runtime result", true)?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "runtime result kind must be a string".to_string())?;
    match kind {
        "usage_snapshot" => {
            let object = exact_object(
                value,
                &["kind", "utilization", "entitlement_balance"],
                &[],
                kind,
                false,
            )?;
            if let Some(utilization) = object.get("utilization").filter(|value| !value.is_null()) {
                let utilization = exact_object(
                    utilization,
                    &[
                        "five_hour",
                        "seven_day",
                        "extra_usage",
                        "five_hour_continue_enabled",
                    ],
                    &[],
                    "utilization",
                    false,
                )?;
                for field in ["five_hour", "seven_day"] {
                    if let Some(limit) = utilization.get(field).filter(|value| !value.is_null()) {
                        exact_object(
                            limit,
                            &["utilization", "resets_at", "overridable"],
                            &[],
                            field,
                            false,
                        )?;
                    }
                }
                if let Some(extra) = utilization
                    .get("extra_usage")
                    .filter(|value| !value.is_null())
                {
                    exact_object(
                        extra,
                        &["is_enabled", "monthly_limit", "used_credits", "utilization"],
                        &[],
                        "extra_usage",
                        false,
                    )?;
                }
            }
            if let Some(balance) = object
                .get("entitlement_balance")
                .filter(|value| !value.is_null())
            {
                exact_object(
                    balance,
                    &[
                        "total_token_quota",
                        "total_token_used",
                        "total_token_remaining",
                        "total_call_quota",
                        "total_call_used",
                        "total_call_remaining",
                        "active_entitlements",
                    ],
                    &[],
                    "entitlement_balance",
                    false,
                )?;
            }
        }
        "usage_five_hour_continue_updated" => {
            exact_object(value, &["kind", "enabled"], &[], kind, false)?;
        }
        "plugin_inventory_snapshot" => {
            let object = exact_object(
                value,
                &["kind", "plugins", "load_diagnostics", "truncated"],
                &[],
                kind,
                false,
            )?;
            validate_array_objects(
                object.get("plugins"),
                &[
                    "id",
                    "name",
                    "marketplace",
                    "description",
                    "version",
                    "is_builtin",
                    "loaded",
                    "enabled",
                    "configured_scope",
                    "installations",
                ],
                "plugins",
                |plugin| {
                    validate_array_objects(
                        plugin.get("installations"),
                        &["scope", "version", "installed_at", "last_updated"],
                        "installations",
                        |_| Ok(()),
                    )
                },
            )?;
            validate_array_objects(
                object.get("load_diagnostics"),
                &["type", "plugin_name"],
                "load_diagnostics",
                |_| Ok(()),
            )?;
        }
        "plugin_marketplace_inventory_snapshot" => {
            let object = exact_object(
                value,
                &["kind", "marketplaces", "empty_reason", "truncated"],
                &[],
                kind,
                false,
            )?;
            validate_array_objects(
                object.get("marketplaces"),
                &[
                    "name",
                    "source_kind",
                    "last_updated",
                    "plugin_count",
                    "installed_plugin_count",
                    "auto_update",
                    "load_failed",
                ],
                "marketplaces",
                |_| Ok(()),
            )?;
        }
        "plugin_marketplace_catalog_snapshot" => {
            let object = exact_object(
                value,
                &["kind", "marketplace_name", "plugins", "truncated"],
                &[],
                kind,
                false,
            )?;
            validate_array_objects(
                object.get("plugins"),
                &[
                    "id",
                    "name",
                    "display_name",
                    "description",
                    "version",
                    "category",
                    "tags",
                    "globally_installed",
                    "enabled",
                    "install_count",
                    "installations",
                ],
                "catalog plugins",
                |plugin| {
                    validate_array_objects(
                        plugin.get("installations"),
                        &["scope", "version", "installed_at", "last_updated"],
                        "catalog installations",
                        |_| Ok(()),
                    )
                },
            )?;
        }
        "plugin_installed" => {
            exact_object(
                value,
                &["kind", "plugin_id", "plugin_name", "scope"],
                &[],
                kind,
                false,
            )?;
        }
        "plugin_uninstalled" => {
            exact_object(
                value,
                &[
                    "kind",
                    "plugin_id",
                    "plugin_name",
                    "scope",
                    "reverse_dependents",
                ],
                &[],
                kind,
                false,
            )?;
        }
        "plugin_enabled_state_updated" => {
            exact_object(
                value,
                &[
                    "kind",
                    "plugin_id",
                    "plugin_name",
                    "enabled",
                    "scope",
                    "reverse_dependents",
                ],
                &[],
                kind,
                false,
            )?;
        }
        "plugin_updated" => {
            exact_object(
                value,
                &[
                    "kind",
                    "plugin_id",
                    "scope",
                    "old_version",
                    "new_version",
                    "already_up_to_date",
                ],
                &[],
                kind,
                false,
            )?;
        }
        "plugin_marketplace_added" => {
            exact_object(
                value,
                &[
                    "kind",
                    "marketplace_name",
                    "source_kind",
                    "already_materialized",
                ],
                &[],
                kind,
                false,
            )?;
        }
        "plugin_marketplace_removed" => {
            exact_object(value, &["kind", "marketplace_name"], &[], kind, false)?;
        }
        "plugin_marketplace_updated" => {
            exact_object(
                value,
                &[
                    "kind",
                    "marketplace_name",
                    "updated_plugin_ids",
                    "plugin_update_failure_count",
                ],
                &[],
                kind,
                false,
            )?;
        }
        "plugin_marketplace_auto_update_updated" => {
            exact_object(
                value,
                &["kind", "marketplace_name", "enabled"],
                &[],
                kind,
                false,
            )?;
        }
        "plugin_validation_result" => {
            let object = exact_object(
                value,
                &[
                    "kind",
                    "success",
                    "file_type",
                    "errors",
                    "warnings",
                    "related_result_count",
                    "truncated",
                ],
                &[],
                kind,
                false,
            )?;
            for field in ["errors", "warnings"] {
                validate_array_objects(object.get(field), &["path", "code"], field, |_| Ok(()))?;
            }
        }
        "usage_plugin_error" => {
            exact_object(
                value,
                &["kind", "action_kind", "code", "message"],
                &[],
                kind,
                false,
            )?;
        }
        _ => return Err(format!("unknown closed usage/plugin result kind `{kind}`")),
    }
    Ok(())
}

fn exact_object<'a>(
    value: &'a Value,
    required: &[&str],
    optional: &[&str],
    label: &str,
    allow_other_fields_for_kind_probe: bool,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    if required.iter().any(|field| !object.contains_key(*field)) {
        return Err(format!("{label} is missing a required field"));
    }
    if !allow_other_fields_for_kind_probe
        && object
            .keys()
            .any(|field| !required.contains(&field.as_str()) && !optional.contains(&field.as_str()))
    {
        return Err(format!("{label} contains an unknown field"));
    }
    Ok(object)
}

fn validate_array_objects<F>(
    value: Option<&Value>,
    fields: &[&str],
    label: &str,
    mut nested: F,
) -> Result<(), String>
where
    F: FnMut(&serde_json::Map<String, Value>) -> Result<(), String>,
{
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{label} must be an array"))?;
    for value in values {
        let object = exact_object(value, fields, &[], label, false)?;
        nested(object)?;
    }
    Ok(())
}

fn validate_runtime_result(result: &UsagePluginRuntimeResult) -> Result<(), String> {
    match result {
        UsagePluginRuntimeResult::UsageSnapshot {
            utilization,
            entitlement_balance,
        } => {
            if let Some(utilization) = utilization.0.as_ref() {
                for limit in [
                    utilization.five_hour.0.as_ref(),
                    utilization.seven_day.0.as_ref(),
                ]
                .into_iter()
                .flatten()
                {
                    validate_percentage(limit.utilization.0, "rate-limit utilization")?;
                    validate_nullable_wire_text(&limit.resets_at.0, "rate-limit resets_at")?;
                }
                if let Some(extra) = utilization.extra_usage.0.as_ref() {
                    validate_nonnegative(extra.monthly_limit.0, "extra-usage monthly_limit")?;
                    validate_nonnegative(extra.used_credits.0, "extra-usage used_credits")?;
                    validate_percentage(extra.utilization.0, "extra-usage utilization")?;
                }
            }
            if let Some(balance) = entitlement_balance.0.as_ref() {
                for (label, value) in [
                    ("total_token_quota", balance.total_token_quota),
                    ("total_token_used", balance.total_token_used),
                    ("total_token_remaining", balance.total_token_remaining),
                    ("total_call_used", balance.total_call_used),
                ] {
                    validate_nonnegative(Some(value), label)?;
                }
                for (label, value) in [
                    ("total_call_quota", balance.total_call_quota),
                    ("total_call_remaining", balance.total_call_remaining),
                ] {
                    validate_finite(value, label)?;
                }
            }
        }
        UsagePluginRuntimeResult::PluginInventorySnapshot {
            plugins,
            load_diagnostics,
            ..
        } => {
            bounded_len(plugins.len(), MAX_PLUGIN_ROWS, "plugins")?;
            bounded_len(
                load_diagnostics.len(),
                MAX_PLUGIN_DIAGNOSTICS,
                "load_diagnostics",
            )?;
            for plugin in plugins {
                validate_selector(&plugin.id, "plugin id")?;
                validate_wire_text(&plugin.name, "plugin name")?;
                validate_wire_text(&plugin.marketplace, "plugin marketplace")?;
                validate_nullable_wire_text(&plugin.description.0, "plugin description")?;
                validate_nullable_wire_text(&plugin.version.0, "plugin version")?;
                validate_installations(&plugin.installations)?;
            }
            for diagnostic in load_diagnostics {
                validate_nullable_wire_text(&diagnostic.plugin_name.0, "diagnostic plugin name")?;
            }
        }
        UsagePluginRuntimeResult::PluginMarketplaceInventorySnapshot { marketplaces, .. } => {
            bounded_len(marketplaces.len(), MAX_MARKETPLACE_ROWS, "marketplaces")?;
            for marketplace in marketplaces {
                validate_wire_text(&marketplace.name, "marketplace name")?;
                validate_nullable_wire_text(&marketplace.last_updated.0, "last_updated")?;
            }
        }
        UsagePluginRuntimeResult::PluginMarketplaceCatalogSnapshot {
            marketplace_name,
            plugins,
            ..
        } => {
            validate_wire_text(marketplace_name, "marketplace_name")?;
            bounded_len(plugins.len(), MAX_PLUGIN_ROWS, "catalog plugins")?;
            for plugin in plugins {
                validate_selector(&plugin.id, "catalog plugin id")?;
                validate_wire_text(&plugin.name, "catalog plugin name")?;
                validate_wire_text(&plugin.display_name, "catalog display name")?;
                validate_nullable_wire_text(&plugin.description.0, "catalog description")?;
                validate_nullable_wire_text(&plugin.version.0, "catalog version")?;
                validate_nullable_wire_text(&plugin.category.0, "catalog category")?;
                bounded_len(plugin.tags.len(), MAX_TAGS_PER_PLUGIN, "catalog tags")?;
                for tag in &plugin.tags {
                    validate_wire_text(tag, "catalog tag")?;
                }
                validate_installations(&plugin.installations)?;
            }
        }
        UsagePluginRuntimeResult::PluginInstalled {
            plugin_id,
            plugin_name,
            ..
        } => {
            validate_selector(plugin_id, "plugin_id")?;
            validate_wire_text(plugin_name, "plugin_name")?;
        }
        UsagePluginRuntimeResult::PluginUninstalled {
            plugin_id,
            plugin_name,
            reverse_dependents,
            ..
        }
        | UsagePluginRuntimeResult::PluginEnabledStateUpdated {
            plugin_id,
            plugin_name,
            reverse_dependents,
            ..
        } => {
            validate_selector(plugin_id, "plugin_id")?;
            validate_wire_text(plugin_name, "plugin_name")?;
            bounded_len(
                reverse_dependents.len(),
                MAX_REVERSE_DEPENDENTS,
                "reverse_dependents",
            )?;
            for dependent_id in reverse_dependents {
                validate_selector(dependent_id, "reverse dependent")?;
            }
        }
        UsagePluginRuntimeResult::PluginUpdated {
            plugin_id,
            old_version,
            new_version,
            ..
        } => {
            validate_selector(plugin_id, "plugin_id")?;
            validate_nullable_wire_text(&old_version.0, "old_version")?;
            validate_nullable_wire_text(&new_version.0, "new_version")?;
        }
        UsagePluginRuntimeResult::PluginMarketplaceAdded {
            marketplace_name, ..
        }
        | UsagePluginRuntimeResult::PluginMarketplaceRemoved { marketplace_name }
        | UsagePluginRuntimeResult::PluginMarketplaceAutoUpdateUpdated {
            marketplace_name, ..
        } => validate_wire_text(marketplace_name, "marketplace_name")?,
        UsagePluginRuntimeResult::PluginMarketplaceUpdated {
            marketplace_name,
            updated_plugin_ids,
            ..
        } => {
            validate_wire_text(marketplace_name, "marketplace_name")?;
            bounded_len(
                updated_plugin_ids.len(),
                MAX_PLUGIN_ROWS,
                "updated_plugin_ids",
            )?;
            for plugin_id in updated_plugin_ids {
                validate_selector(plugin_id, "updated plugin id")?;
            }
        }
        UsagePluginRuntimeResult::PluginValidationResult {
            errors, warnings, ..
        } => {
            bounded_len(errors.len(), MAX_PLUGIN_DIAGNOSTICS, "validation errors")?;
            bounded_len(
                warnings.len(),
                MAX_PLUGIN_DIAGNOSTICS,
                "validation warnings",
            )?;
            for diagnostic in errors.iter().chain(warnings) {
                validate_wire_text(&diagnostic.path, "validation path")?;
                validate_nullable_wire_text(&diagnostic.code.0, "validation code")?;
            }
        }
        UsagePluginRuntimeResult::UsagePluginError { message, .. } => {
            validate_wire_text(message, "usage/plugin error message")?;
        }
        UsagePluginRuntimeResult::UsageFiveHourContinueUpdated { .. } => {}
    }
    Ok(())
}

fn validate_installations(installations: &[PluginInstallationSnapshot]) -> Result<(), String> {
    bounded_len(
        installations.len(),
        MAX_INSTALLATIONS_PER_PLUGIN,
        "plugin installations",
    )?;
    for installation in installations {
        validate_nullable_wire_text(&installation.version.0, "installation version")?;
        validate_nullable_wire_text(&installation.installed_at.0, "installed_at")?;
        validate_nullable_wire_text(&installation.last_updated.0, "last_updated")?;
    }
    Ok(())
}

fn bounded_len(actual: usize, maximum: usize, label: &str) -> Result<(), String> {
    if actual <= maximum {
        Ok(())
    } else {
        Err(format!("{label} exceeds closed contract maximum {maximum}"))
    }
}

fn validate_percentage(value: Option<f64>, label: &str) -> Result<(), String> {
    if value.is_none_or(|value| value.is_finite() && (0.0..=100.0).contains(&value)) {
        Ok(())
    } else {
        Err(format!("{label} is outside 0..=100"))
    }
}

fn validate_nonnegative(value: Option<f64>, label: &str) -> Result<(), String> {
    if value.is_none_or(|value| value.is_finite() && value >= 0.0) {
        Ok(())
    } else {
        Err(format!("{label} must be finite and nonnegative"))
    }
}

fn validate_finite(value: f64, label: &str) -> Result<(), String> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(format!("{label} must be finite"))
    }
}

fn validate_nullable_wire_text(value: &Option<String>, label: &str) -> Result<(), String> {
    value
        .as_deref()
        .map_or(Ok(()), |value| validate_wire_text(value, label))
}

fn validate_wire_text(value: &str, label: &str) -> Result<(), String> {
    if value.encode_utf16().count() <= MAX_WIRE_TEXT_CODE_UNITS
        && !contains_forbidden_control(value)
    {
        Ok(())
    } else {
        Err(format!("{label} is not safe wire text"))
    }
}

fn validate_selector(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_SELECTOR_BYTES
        || value.trim() != value
        || contains_forbidden_control(value)
    {
        return Err(format!("{label} is invalid"));
    }
    let mut parts = value.split('@');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    if parts.next().is_some()
        || !valid_selector_segment(first)
        || second.is_some_and(|segment| !valid_selector_segment(segment))
    {
        return Err(format!("{label} is invalid"));
    }
    Ok(())
}

fn validate_marketplace_name(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_MARKETPLACE_NAME_BYTES
        || !valid_selector_segment(value)
    {
        Err("invalid marketplace name".to_string())
    } else {
        Ok(value.to_string())
    }
}

fn valid_selector_segment(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_')
        })
}

fn contains_forbidden_control(value: &str) -> bool {
    value
        .chars()
        .any(|character| character <= '\u{1f}' || character == '\u{7f}')
}

fn safe_input(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.encode_utf16().count() > MAX_INPUT_CODE_UNITS
        || contains_forbidden_control(value)
    {
        Err("invalid input".to_string())
    } else {
        Ok(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UsagePluginRequestPurpose {
    UsageRead,
    UsagePreferenceWrite,
    PluginInventoryRead,
    MarketplaceInventoryRead,
    MarketplaceCatalogRead { marketplace_name: String },
    PluginInstall { plugin_id: String },
    PluginUninstall { plugin_id: String },
    PluginEnabledWrite { plugin_id: String },
    PluginUpdate { plugin_id: String },
    MarketplaceAdd,
    MarketplaceRemove { marketplace_name: String },
    MarketplaceUpdate { marketplace_name: String },
    MarketplaceAutoUpdateWrite { marketplace_name: String },
    PluginValidate,
}

impl UsagePluginRequestPurpose {
    const fn action_kind(&self) -> UsagePluginActionKind {
        match self {
            Self::UsageRead => UsagePluginActionKind::UsageRead,
            Self::UsagePreferenceWrite => UsagePluginActionKind::UsageSetFiveHourContinue,
            Self::PluginInventoryRead => UsagePluginActionKind::PluginInventoryRead,
            Self::MarketplaceInventoryRead => UsagePluginActionKind::PluginMarketplaceInventoryRead,
            Self::MarketplaceCatalogRead { .. } => {
                UsagePluginActionKind::PluginMarketplaceCatalogRead
            }
            Self::PluginInstall { .. } => UsagePluginActionKind::PluginInstall,
            Self::PluginUninstall { .. } => UsagePluginActionKind::PluginUninstall,
            Self::PluginEnabledWrite { .. } => UsagePluginActionKind::PluginSetEnabled,
            Self::PluginUpdate { .. } => UsagePluginActionKind::PluginUpdate,
            Self::MarketplaceAdd => UsagePluginActionKind::PluginMarketplaceAdd,
            Self::MarketplaceRemove { .. } => UsagePluginActionKind::PluginMarketplaceRemove,
            Self::MarketplaceUpdate { .. } => UsagePluginActionKind::PluginMarketplaceUpdate,
            Self::MarketplaceAutoUpdateWrite { .. } => {
                UsagePluginActionKind::PluginMarketplaceSetAutoUpdate
            }
            Self::PluginValidate => UsagePluginActionKind::PluginValidate,
        }
    }

    const fn expected_result_kind(&self) -> &'static str {
        match self {
            Self::UsageRead => "usage_snapshot",
            Self::UsagePreferenceWrite => "usage_five_hour_continue_updated",
            Self::PluginInventoryRead => "plugin_inventory_snapshot",
            Self::MarketplaceInventoryRead => "plugin_marketplace_inventory_snapshot",
            Self::MarketplaceCatalogRead { .. } => "plugin_marketplace_catalog_snapshot",
            Self::PluginInstall { .. } => "plugin_installed",
            Self::PluginUninstall { .. } => "plugin_uninstalled",
            Self::PluginEnabledWrite { .. } => "plugin_enabled_state_updated",
            Self::PluginUpdate { .. } => "plugin_updated",
            Self::MarketplaceAdd => "plugin_marketplace_added",
            Self::MarketplaceRemove { .. } => "plugin_marketplace_removed",
            Self::MarketplaceUpdate { .. } => "plugin_marketplace_updated",
            Self::MarketplaceAutoUpdateWrite { .. } => "plugin_marketplace_auto_update_updated",
            Self::PluginValidate => "plugin_validation_result",
        }
    }
}

#[derive(Clone, PartialEq)]
pub(crate) enum UsagePluginManagementEffect {
    Private {
        token: u64,
        purpose: UsagePluginRequestPurpose,
        action: UsagePluginRuntimeAction,
    },
    Close,
}

impl fmt::Debug for UsagePluginManagementEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Private {
                token,
                purpose,
                action,
            } => formatter
                .debug_struct("Private")
                .field("token", token)
                .field("purpose", purpose)
                .field("action", action)
                .finish(),
            Self::Close => formatter.write_str("Close"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingRequest {
    token: u64,
    purpose: UsagePluginRequestPurpose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginManagementTab {
    Discover,
    Installed,
    Marketplaces,
    Errors,
}

impl PluginManagementTab {
    const ALL: [Self; 4] = [
        Self::Discover,
        Self::Installed,
        Self::Marketplaces,
        Self::Errors,
    ];

    fn label(self, language: UiLanguage) -> &'static str {
        match self {
            Self::Discover => language.text("发现", "Discover"),
            Self::Installed => language.text("已安装", "Installed"),
            Self::Marketplaces => language.text("插件市场", "Marketplaces"),
            Self::Errors => language.text("错误", "Errors"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum View {
    Usage,
    Help,
    Discover,
    Catalog(String),
    CatalogPlugin(String),
    Installed,
    InstalledPlugin(String),
    UninstallScope(String),
    UninstallConfirm {
        plugin_id: String,
        scope: PluginInstallScope,
    },
    Marketplaces,
    Marketplace(String),
    MarketplaceRemoveConfirm(String),
    MarketplaceAdd,
    Errors,
    Validate,
    ValidationResult,
}

impl View {
    fn tab(&self) -> Option<PluginManagementTab> {
        match self {
            Self::Discover | Self::Catalog(_) | Self::CatalogPlugin(_) => {
                Some(PluginManagementTab::Discover)
            }
            Self::Installed
            | Self::InstalledPlugin(_)
            | Self::UninstallScope(_)
            | Self::UninstallConfirm { .. } => Some(PluginManagementTab::Installed),
            Self::Marketplaces
            | Self::Marketplace(_)
            | Self::MarketplaceRemoveConfirm(_)
            | Self::MarketplaceAdd => Some(PluginManagementTab::Marketplaces),
            Self::Errors => Some(PluginManagementTab::Errors),
            Self::Usage | Self::Help | Self::Validate | Self::ValidationResult => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstalledDirectAction {
    Manage,
    Enable,
    Disable,
    Uninstall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarketplaceDirectAction {
    Browse,
    Update,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeferredRoute {
    Discover {
        target: String,
        install: bool,
    },
    Installed {
        target: String,
        action: InstalledDirectAction,
    },
    Marketplace {
        target: String,
        action: MarketplaceDirectAction,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedPluginRoute {
    Help,
    Discover {
        target: Option<String>,
        install: bool,
    },
    Installed {
        target: Option<String>,
        action: InstalledDirectAction,
    },
    Marketplaces,
    MarketplaceTarget {
        target: String,
        action: MarketplaceDirectAction,
    },
    MarketplaceAdd(Option<String>),
    Validate(Option<String>),
}

fn parse_plugin_route(arguments: &str) -> ParsedPluginRoute {
    let words = arguments.split_whitespace().collect::<Vec<_>>();
    let Some(command) = words.first().copied() else {
        return ParsedPluginRoute::Discover {
            target: None,
            install: false,
        };
    };
    match command {
        "help" | "-h" | "--help" => ParsedPluginRoute::Help,
        "install" | "i" => {
            let target = words
                .get(1..)
                .map(|parts| parts.join(" "))
                .filter(|s| !s.is_empty());
            if target.as_deref().is_some_and(looks_like_marketplace_source) {
                ParsedPluginRoute::MarketplaceAdd(target)
            } else {
                ParsedPluginRoute::Discover {
                    target,
                    install: true,
                }
            }
        }
        "manage" => ParsedPluginRoute::Installed {
            target: words.get(1).map(|value| (*value).to_string()),
            action: InstalledDirectAction::Manage,
        },
        "uninstall" => ParsedPluginRoute::Installed {
            target: words.get(1).map(|value| (*value).to_string()),
            action: InstalledDirectAction::Uninstall,
        },
        "enable" => ParsedPluginRoute::Installed {
            target: words.get(1).map(|value| (*value).to_string()),
            action: InstalledDirectAction::Enable,
        },
        "disable" => ParsedPluginRoute::Installed {
            target: words.get(1).map(|value| (*value).to_string()),
            action: InstalledDirectAction::Disable,
        },
        "validate" => ParsedPluginRoute::Validate(
            words
                .get(1..)
                .map(|parts| parts.join(" "))
                .filter(|value| !value.is_empty()),
        ),
        "marketplace" | "market" => parse_marketplace_route(&words[1..]),
        _ => ParsedPluginRoute::Discover {
            target: Some(arguments.trim().to_string()),
            install: false,
        },
    }
}

fn parse_marketplace_route(words: &[&str]) -> ParsedPluginRoute {
    let Some(command) = words.first().copied() else {
        return ParsedPluginRoute::Marketplaces;
    };
    match command {
        "list" => ParsedPluginRoute::Marketplaces,
        "add" => ParsedPluginRoute::MarketplaceAdd(
            words
                .get(1..)
                .map(|parts| parts.join(" "))
                .filter(|value| !value.is_empty()),
        ),
        "remove" | "rm" => words
            .get(1)
            .map_or(ParsedPluginRoute::Marketplaces, |target| {
                ParsedPluginRoute::MarketplaceTarget {
                    target: (*target).to_string(),
                    action: MarketplaceDirectAction::Remove,
                }
            }),
        "update" => words
            .get(1)
            .map_or(ParsedPluginRoute::Marketplaces, |target| {
                ParsedPluginRoute::MarketplaceTarget {
                    target: (*target).to_string(),
                    action: MarketplaceDirectAction::Update,
                }
            }),
        target => ParsedPluginRoute::MarketplaceTarget {
            target: target.to_string(),
            action: MarketplaceDirectAction::Browse,
        },
    }
}

fn looks_like_marketplace_source(value: &str) -> bool {
    value.contains("://")
        || value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
        || value.contains('\\')
        || value.ends_with(".git")
        || (value.contains('/') && !value.contains('@'))
}

#[derive(Debug, Clone, PartialEq)]
struct UsageData {
    utilization: Option<UtilizationSnapshot>,
    entitlement_balance: Option<EntitlementBalanceSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
struct ValidationData {
    success: bool,
    file_type: ValidationFileType,
    errors: Vec<PluginValidationDiagnostic>,
    warnings: Vec<PluginValidationDiagnostic>,
    related_result_count: u64,
    truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueuedMutation {
    Install {
        plugin_id: String,
        scope: PluginInstallScope,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsagePluginManagementRow {
    pub(crate) label: String,
    pub(crate) detail: Option<String>,
    pub(crate) disabled: bool,
    pub(crate) marked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsagePluginManagementTabView {
    pub(crate) label: String,
    pub(crate) active: bool,
    pub(crate) badge: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsagePluginDetailTone {
    Section,
    Metric,
    Supporting,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsagePluginDetailLine {
    pub(crate) text: String,
    pub(crate) tone: UsagePluginDetailTone,
}

#[derive(Debug, Clone)]
pub(crate) struct UsagePluginManagementState {
    view: View,
    selected: usize,
    notice: Option<String>,
    pending: Option<PendingRequest>,
    next_token: u64,
    usage: Option<UsageData>,
    plugins: Vec<PluginInventoryEntry>,
    load_diagnostics: Vec<PluginLoadDiagnostic>,
    inventory_truncated: bool,
    marketplaces: Vec<MarketplaceInventoryEntry>,
    marketplace_inventory_truncated: bool,
    discover_empty_reason: Option<PluginDiscoverEmptyReason>,
    catalogs: BTreeMap<String, Vec<MarketplaceCatalogPlugin>>,
    catalog_truncated: BTreeSet<String>,
    catalog_failures: BTreeSet<String>,
    catalog_queue: VecDeque<String>,
    deferred_route: Option<DeferredRoute>,
    queued_mutations: VecDeque<QueuedMutation>,
    marked_plugins: BTreeSet<String>,
    query: String,
    query_cursor: usize,
    editing_query: bool,
    input: String,
    input_cursor: usize,
    validation: Option<ValidationData>,
    last_validation_path: Option<String>,
}

impl UsagePluginManagementState {
    fn empty(view: View) -> Self {
        Self {
            view,
            selected: 0,
            notice: None,
            pending: None,
            next_token: 1,
            usage: None,
            plugins: Vec::new(),
            load_diagnostics: Vec::new(),
            inventory_truncated: false,
            marketplaces: Vec::new(),
            marketplace_inventory_truncated: false,
            discover_empty_reason: None,
            catalogs: BTreeMap::new(),
            catalog_truncated: BTreeSet::new(),
            catalog_failures: BTreeSet::new(),
            catalog_queue: VecDeque::new(),
            deferred_route: None,
            queued_mutations: VecDeque::new(),
            marked_plugins: BTreeSet::new(),
            query: String::new(),
            query_cursor: 0,
            editing_query: false,
            input: String::new(),
            input_cursor: 0,
            validation: None,
            last_validation_path: None,
        }
    }

    pub(crate) fn open_usage() -> (Self, UsagePluginManagementEffect) {
        let mut state = Self::empty(View::Usage);
        let effect = state.begin(
            UsagePluginRequestPurpose::UsageRead,
            UsagePluginRuntimeAction::UsageRead,
        );
        (state, effect)
    }

    pub(crate) fn open_plugin(arguments: &str) -> (Self, Vec<UsagePluginManagementEffect>) {
        let route = parse_plugin_route(arguments);
        let mut state = Self::empty(View::Discover);
        let mut effects = Vec::new();
        match route {
            ParsedPluginRoute::Help => state.view = View::Help,
            ParsedPluginRoute::Discover { target, install } => {
                let qualified_marketplace = target
                    .as_deref()
                    .and_then(|target| target.rsplit_once('@'))
                    .map(|(_, marketplace)| marketplace)
                    .filter(|marketplace| !marketplace.is_empty())
                    .and_then(|marketplace| validate_marketplace_name(marketplace).ok());
                if let (Some(target), Some(marketplace_name)) =
                    (target.clone(), qualified_marketplace)
                {
                    state.view = View::Catalog(marketplace_name.clone());
                    state.deferred_route = Some(DeferredRoute::Discover { target, install });
                    effects.push(state.begin(
                        UsagePluginRequestPurpose::MarketplaceCatalogRead {
                            marketplace_name: marketplace_name.clone(),
                        },
                        UsagePluginRuntimeAction::PluginMarketplaceCatalogRead { marketplace_name },
                    ));
                } else {
                    state.view = View::Discover;
                    if let Some(target) = target {
                        state.deferred_route = Some(DeferredRoute::Discover { target, install });
                    }
                    effects.push(state.begin(
                        UsagePluginRequestPurpose::MarketplaceInventoryRead,
                        UsagePluginRuntimeAction::PluginMarketplaceInventoryRead,
                    ));
                }
            }
            ParsedPluginRoute::Installed { target, action } => {
                state.view = View::Installed;
                if let Some(target) = target {
                    state.deferred_route = Some(DeferredRoute::Installed { target, action });
                }
                effects.push(state.begin(
                    UsagePluginRequestPurpose::PluginInventoryRead,
                    UsagePluginRuntimeAction::PluginInventoryRead,
                ));
            }
            ParsedPluginRoute::Marketplaces => {
                state.view = View::Marketplaces;
                effects.push(state.begin(
                    UsagePluginRequestPurpose::MarketplaceInventoryRead,
                    UsagePluginRuntimeAction::PluginMarketplaceInventoryRead,
                ));
            }
            ParsedPluginRoute::MarketplaceTarget { target, action } => {
                state.view = View::Marketplaces;
                state.deferred_route = Some(DeferredRoute::Marketplace { target, action });
                effects.push(state.begin(
                    UsagePluginRequestPurpose::MarketplaceInventoryRead,
                    UsagePluginRuntimeAction::PluginMarketplaceInventoryRead,
                ));
            }
            ParsedPluginRoute::MarketplaceAdd(source) => {
                state.view = View::MarketplaceAdd;
                if let Some(source) = source {
                    state.set_input(source);
                    if let Ok(source_input) = safe_input(&state.input) {
                        effects.push(state.begin(
                            UsagePluginRequestPurpose::MarketplaceAdd,
                            UsagePluginRuntimeAction::PluginMarketplaceAdd { source_input },
                        ));
                    }
                }
            }
            ParsedPluginRoute::Validate(path) => {
                state.view = View::Validate;
                if let Some(path) = path {
                    state.set_input(path);
                    if let Ok(path) = safe_input(&state.input) {
                        state.last_validation_path = Some(path.clone());
                        effects.push(state.begin(
                            UsagePluginRequestPurpose::PluginValidate,
                            UsagePluginRuntimeAction::PluginValidate { path },
                        ));
                    }
                }
            }
        }
        (state, effects)
    }

    fn set_input(&mut self, input: String) {
        self.input = input;
        self.input_cursor = self.input.len();
    }

    fn begin(
        &mut self,
        purpose: UsagePluginRequestPurpose,
        action: UsagePluginRuntimeAction,
    ) -> UsagePluginManagementEffect {
        debug_assert_eq!(purpose.action_kind(), action.kind());
        let token = self.next_token.max(1);
        self.next_token = token.wrapping_add(1).max(1);
        self.pending = Some(PendingRequest {
            token,
            purpose: purpose.clone(),
        });
        UsagePluginManagementEffect::Private {
            token,
            purpose,
            action,
        }
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub(crate) fn apply_send_failure(
        &mut self,
        token: u64,
        language: UiLanguage,
        error_code: &str,
    ) {
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.token != token)
        {
            return;
        }
        self.pending = None;
        self.notice = Some(match language {
            UiLanguage::ZhCn => {
                format!("直连请求发送失败（{error_code}）；可重试，面板仍保持打开。")
            }
            UiLanguage::EnUs => format!(
                "Direct request send failed ({error_code}); retry is available and the panel remains open."
            ),
        });
    }

    pub(crate) fn apply_protocol_failure(
        &mut self,
        token: u64,
        language: UiLanguage,
        error_code: &str,
    ) {
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.token != token)
        {
            return;
        }
        self.pending = None;
        self.notice = Some(match language {
            UiLanguage::ZhCn => {
                format!("直连运行结果不符合闭合协议（{error_code}）；已忽略，可重试。")
            }
            UiLanguage::EnUs => format!(
                "Direct runtime result violated the closed contract ({error_code}); it was ignored and can be retried."
            ),
        });
    }

    pub(crate) fn apply_result(
        &mut self,
        token: u64,
        value: Value,
        language: UiLanguage,
    ) -> Vec<UsagePluginManagementEffect> {
        let Some(pending) = self.pending.as_ref() else {
            return Vec::new();
        };
        if pending.token != token {
            return Vec::new();
        }
        let pending = match self.pending.take() {
            Some(pending) => pending,
            None => return Vec::new(),
        };
        let result = match parse_usage_plugin_runtime_result(value) {
            Ok(result) => result,
            Err(error) => {
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!("运行结果不符合闭合协议：{error}；可重试。"),
                    UiLanguage::EnUs => {
                        format!(
                            "Runtime result violated the closed contract: {error}; retry is available."
                        )
                    }
                });
                return Vec::new();
            }
        };
        if let UsagePluginRuntimeResult::UsagePluginError {
            action_kind,
            code,
            message,
        } = result
        {
            if action_kind != pending.purpose.action_kind() {
                self.notice = Some(language.text(
                    "运行结果与当前请求不匹配；已忽略，可重试。",
                    "Runtime result did not match the active request; it was ignored and can be retried.",
                ).to_string());
                return Vec::new();
            }
            self.notice = Some(localized_runtime_error(language, code, &message));
            return self.continue_after_business_error(&pending.purpose, language);
        }
        if result.result_kind() != pending.purpose.expected_result_kind() {
            self.notice = Some(language.text(
                "运行结果类型与当前请求不匹配；已忽略，可重试。",
                "Runtime result kind did not match the active request; it was ignored and can be retried.",
            ).to_string());
            return Vec::new();
        }
        self.apply_success(result, pending.purpose, language)
    }

    fn apply_success(
        &mut self,
        result: UsagePluginRuntimeResult,
        purpose: UsagePluginRequestPurpose,
        language: UiLanguage,
    ) -> Vec<UsagePluginManagementEffect> {
        match result {
            UsagePluginRuntimeResult::UsageSnapshot {
                utilization,
                entitlement_balance,
            } => {
                self.usage = Some(UsageData {
                    utilization: utilization.0,
                    entitlement_balance: entitlement_balance.0,
                });
                self.notice = None;
                Vec::new()
            }
            UsagePluginRuntimeResult::UsageFiveHourContinueUpdated { enabled } => {
                if let Some(utilization) = self
                    .usage
                    .as_mut()
                    .and_then(|usage| usage.utilization.as_mut())
                {
                    utilization.five_hour_continue_enabled = RequiredNullable(Some(enabled));
                }
                self.notice = Some(
                    if enabled {
                        language.text(
                            "已启用五小时额度耗尽后继续使用。",
                            "Continue-after-five-hour-limit is enabled.",
                        )
                    } else {
                        language.text(
                            "已停用五小时额度耗尽后继续使用。",
                            "Continue-after-five-hour-limit is disabled.",
                        )
                    }
                    .to_string(),
                );
                Vec::new()
            }
            UsagePluginRuntimeResult::PluginInventorySnapshot {
                plugins,
                load_diagnostics,
                truncated,
            } => {
                self.plugins = plugins;
                self.load_diagnostics = load_diagnostics;
                self.inventory_truncated = truncated;
                self.normalize_selection();
                self.resolve_installed_route(language)
            }
            UsagePluginRuntimeResult::PluginMarketplaceInventorySnapshot {
                marketplaces,
                empty_reason,
                truncated,
            } => {
                self.marketplaces = marketplaces;
                self.marketplace_inventory_truncated = truncated;
                self.discover_empty_reason = Some(empty_reason);
                self.normalize_selection();
                self.after_marketplace_inventory(language)
            }
            UsagePluginRuntimeResult::PluginMarketplaceCatalogSnapshot {
                marketplace_name,
                plugins,
                truncated,
            } => {
                if let UsagePluginRequestPurpose::MarketplaceCatalogRead {
                    marketplace_name: requested,
                } = &purpose
                    && requested != &marketplace_name
                {
                    self.notice = Some(language.text(
                            "市场目录结果与请求的市场不一致；已忽略。",
                            "Marketplace catalog result did not match the requested marketplace; it was ignored.",
                        ).to_string());
                    return Vec::new();
                }
                if truncated {
                    self.catalog_truncated.insert(marketplace_name.clone());
                } else {
                    self.catalog_truncated.remove(&marketplace_name);
                }
                self.catalog_failures.remove(&marketplace_name);
                self.catalogs.insert(marketplace_name, plugins);
                self.continue_catalog_loading(language)
            }
            UsagePluginRuntimeResult::PluginInstalled {
                plugin_id,
                plugin_name,
                scope,
            } => {
                self.marked_plugins.remove(&plugin_id);
                self.notice = Some(match language {
                    UiLanguage::ZhCn => {
                        format!("已在{}范围安装插件 {plugin_name}。", scope.label(language))
                    }
                    UiLanguage::EnUs => {
                        format!(
                            "Installed {plugin_name} in {} scope.",
                            scope.label(language)
                        )
                    }
                });
                if let Some(effect) = self.begin_next_queued_mutation() {
                    vec![effect]
                } else {
                    self.view = View::Installed;
                    vec![self.begin(
                        UsagePluginRequestPurpose::PluginInventoryRead,
                        UsagePluginRuntimeAction::PluginInventoryRead,
                    )]
                }
            }
            UsagePluginRuntimeResult::PluginUninstalled {
                plugin_name,
                scope,
                reverse_dependents,
                ..
            } => {
                self.notice = Some(mutation_notice_with_dependents(
                    language,
                    format!("已从{}范围卸载插件 {plugin_name}。", scope.label(language)),
                    format!(
                        "Uninstalled {plugin_name} from {} scope.",
                        scope.label(language)
                    ),
                    &reverse_dependents,
                ));
                self.view = View::Installed;
                vec![self.begin(
                    UsagePluginRequestPurpose::PluginInventoryRead,
                    UsagePluginRuntimeAction::PluginInventoryRead,
                )]
            }
            UsagePluginRuntimeResult::PluginEnabledStateUpdated {
                plugin_name,
                enabled,
                scope,
                reverse_dependents,
                ..
            } => {
                let zh = format!(
                    "已在{}设置中{}插件 {plugin_name}。",
                    scope.label(language),
                    if enabled { "启用" } else { "停用" }
                );
                let en = format!(
                    "{} {plugin_name} in {} settings.",
                    if enabled { "Enabled" } else { "Disabled" },
                    scope.label(language)
                );
                self.notice = Some(mutation_notice_with_dependents(
                    language,
                    zh,
                    en,
                    &reverse_dependents,
                ));
                self.view = View::Installed;
                vec![self.begin(
                    UsagePluginRequestPurpose::PluginInventoryRead,
                    UsagePluginRuntimeAction::PluginInventoryRead,
                )]
            }
            UsagePluginRuntimeResult::PluginUpdated {
                plugin_id,
                scope,
                old_version,
                new_version,
                already_up_to_date,
            } => {
                self.notice = Some(match language {
                    UiLanguage::ZhCn if already_up_to_date => {
                        format!(
                            "插件 {plugin_id}（{}）已是最新版本。",
                            scope.label(language)
                        )
                    }
                    UiLanguage::EnUs if already_up_to_date => {
                        format!(
                            "Plugin {plugin_id} ({}) is already up to date.",
                            scope.label(language)
                        )
                    }
                    UiLanguage::ZhCn => format!(
                        "已更新插件 {plugin_id}（{}）：{} → {}。",
                        scope.label(language),
                        old_version.0.as_deref().unwrap_or("—"),
                        new_version.0.as_deref().unwrap_or("—")
                    ),
                    UiLanguage::EnUs => format!(
                        "Updated {plugin_id} ({}): {} → {}.",
                        scope.label(language),
                        old_version.0.as_deref().unwrap_or("—"),
                        new_version.0.as_deref().unwrap_or("—")
                    ),
                });
                self.view = View::Installed;
                vec![self.begin(
                    UsagePluginRequestPurpose::PluginInventoryRead,
                    UsagePluginRuntimeAction::PluginInventoryRead,
                )]
            }
            UsagePluginRuntimeResult::PluginMarketplaceAdded {
                marketplace_name,
                source_kind,
                already_materialized,
            } => {
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!(
                        "已添加插件市场 {marketplace_name}（{}{}）。",
                        source_kind.as_str(),
                        if already_materialized {
                            "，已存在本地内容"
                        } else {
                            ""
                        }
                    ),
                    UiLanguage::EnUs => format!(
                        "Added marketplace {marketplace_name} ({}{}).",
                        source_kind.as_str(),
                        if already_materialized {
                            ", local content already existed"
                        } else {
                            ""
                        }
                    ),
                });
                self.view = View::Catalog(marketplace_name.clone());
                self.input.clear();
                self.input_cursor = 0;
                match validate_marketplace_name(&marketplace_name) {
                    Ok(marketplace_name) => vec![self.begin(
                        UsagePluginRequestPurpose::MarketplaceCatalogRead {
                            marketplace_name: marketplace_name.clone(),
                        },
                        UsagePluginRuntimeAction::PluginMarketplaceCatalogRead { marketplace_name },
                    )],
                    Err(_) => {
                        self.notice = Some(language.text(
                            "运行环境返回了不能用于目录读取的市场名称；未继续请求。",
                            "The runtime returned a marketplace name that cannot be used for a catalog read; no follow-up was sent.",
                        ).to_string());
                        Vec::new()
                    }
                }
            }
            UsagePluginRuntimeResult::PluginMarketplaceRemoved { marketplace_name } => {
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!("已移除插件市场 {marketplace_name}。"),
                    UiLanguage::EnUs => format!("Removed marketplace {marketplace_name}."),
                });
                self.catalogs.remove(&marketplace_name);
                self.view = View::Marketplaces;
                vec![self.begin(
                    UsagePluginRequestPurpose::MarketplaceInventoryRead,
                    UsagePluginRuntimeAction::PluginMarketplaceInventoryRead,
                )]
            }
            UsagePluginRuntimeResult::PluginMarketplaceUpdated {
                marketplace_name,
                updated_plugin_ids,
                plugin_update_failure_count,
            } => {
                self.catalogs.remove(&marketplace_name);
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!(
                        "已更新市场 {marketplace_name}；更新插件 {} 个，失败 {} 个。",
                        updated_plugin_ids.len(),
                        plugin_update_failure_count
                    ),
                    UiLanguage::EnUs => format!(
                        "Updated marketplace {marketplace_name}; {} plugin updates, {} failures.",
                        updated_plugin_ids.len(),
                        plugin_update_failure_count
                    ),
                });
                self.view = View::Marketplaces;
                vec![self.begin(
                    UsagePluginRequestPurpose::MarketplaceInventoryRead,
                    UsagePluginRuntimeAction::PluginMarketplaceInventoryRead,
                )]
            }
            UsagePluginRuntimeResult::PluginMarketplaceAutoUpdateUpdated {
                marketplace_name,
                enabled,
            } => {
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!(
                        "市场 {marketplace_name} 自动更新已{}。",
                        if enabled { "启用" } else { "停用" }
                    ),
                    UiLanguage::EnUs => format!(
                        "Automatic updates for {marketplace_name} are {}.",
                        if enabled { "enabled" } else { "disabled" }
                    ),
                });
                self.view = View::Marketplace(marketplace_name);
                vec![self.begin(
                    UsagePluginRequestPurpose::MarketplaceInventoryRead,
                    UsagePluginRuntimeAction::PluginMarketplaceInventoryRead,
                )]
            }
            UsagePluginRuntimeResult::PluginValidationResult {
                success,
                file_type,
                errors,
                warnings,
                related_result_count,
                truncated,
            } => {
                self.validation = Some(ValidationData {
                    success,
                    file_type,
                    errors,
                    warnings,
                    related_result_count,
                    truncated,
                });
                self.view = View::ValidationResult;
                self.selected = 0;
                self.notice = None;
                Vec::new()
            }
            UsagePluginRuntimeResult::UsagePluginError { .. } => Vec::new(),
        }
    }

    fn continue_after_business_error(
        &mut self,
        purpose: &UsagePluginRequestPurpose,
        language: UiLanguage,
    ) -> Vec<UsagePluginManagementEffect> {
        match purpose {
            UsagePluginRequestPurpose::MarketplaceCatalogRead { marketplace_name } => {
                self.catalog_failures.insert(marketplace_name.clone());
                if self.catalog_queue.is_empty()
                    && self.deferred_route.as_ref().is_some_and(|route| {
                        matches!(route, DeferredRoute::Discover { target, .. }
                            if target.rsplit_once('@').is_some_and(|(_, requested)| requested == marketplace_name))
                    })
                {
                    self.deferred_route = None;
                    return Vec::new();
                }
                self.continue_catalog_loading(language)
            }
            UsagePluginRequestPurpose::PluginInstall { .. } => self
                .begin_next_queued_mutation()
                .into_iter()
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        }
    }

    fn after_marketplace_inventory(
        &mut self,
        language: UiLanguage,
    ) -> Vec<UsagePluginManagementEffect> {
        if matches!(self.view, View::Discover) {
            self.catalog_queue = self
                .marketplaces
                .iter()
                .filter(|entry| !entry.load_failed)
                .map(|entry| entry.name.clone())
                .filter(|name| !self.catalogs.contains_key(name))
                .collect();
            return self.continue_catalog_loading(language);
        }
        self.resolve_marketplace_route(language)
    }

    fn continue_catalog_loading(
        &mut self,
        language: UiLanguage,
    ) -> Vec<UsagePluginManagementEffect> {
        while let Some(name) = self.catalog_queue.pop_front() {
            match validate_marketplace_name(&name) {
                Ok(marketplace_name) => {
                    return vec![self.begin(
                        UsagePluginRequestPurpose::MarketplaceCatalogRead {
                            marketplace_name: marketplace_name.clone(),
                        },
                        UsagePluginRuntimeAction::PluginMarketplaceCatalogRead { marketplace_name },
                    )];
                }
                Err(_) => {
                    self.catalog_failures.insert(name);
                }
            }
        }
        self.resolve_discover_route(language)
    }

    fn resolve_discover_route(&mut self, language: UiLanguage) -> Vec<UsagePluginManagementEffect> {
        let Some(DeferredRoute::Discover { target, install }) = self.deferred_route.clone() else {
            return Vec::new();
        };
        let matches = self.find_catalog_matches(&target);
        match matches.as_slice() {
            [] => {
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!("未找到插件 {target}。"),
                    UiLanguage::EnUs => format!("Plugin {target} was not found."),
                });
                self.deferred_route = None;
                Vec::new()
            }
            [plugin_id] => {
                let plugin_id = plugin_id.clone();
                self.deferred_route = None;
                self.view = View::CatalogPlugin(plugin_id.clone());
                self.selected = 0;
                if install {
                    self.notice = Some(
                        language
                            .text(
                                "请选择明确的安装范围。",
                                "Select an explicit installation scope.",
                            )
                            .to_string(),
                    );
                }
                Vec::new()
            }
            _ => {
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!(
                        "插件名 {target} 对应多个市场；请使用 plugin@marketplace 精确指定。"
                    ),
                    UiLanguage::EnUs => format!(
                        "Plugin name {target} exists in multiple marketplaces; use plugin@marketplace."
                    ),
                });
                self.deferred_route = None;
                Vec::new()
            }
        }
    }

    fn resolve_installed_route(
        &mut self,
        language: UiLanguage,
    ) -> Vec<UsagePluginManagementEffect> {
        let Some(DeferredRoute::Installed { target, action }) = self.deferred_route.clone() else {
            return Vec::new();
        };
        let matches = self.find_installed_matches(&target);
        let plugin_id = match matches.as_slice() {
            [] => {
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!("未找到已安装插件 {target}。"),
                    UiLanguage::EnUs => format!("Installed plugin {target} was not found."),
                });
                self.deferred_route = None;
                return Vec::new();
            }
            [plugin_id] => plugin_id.clone(),
            _ => {
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!(
                        "已安装插件名 {target} 不唯一；请使用 plugin@marketplace 精确指定。"
                    ),
                    UiLanguage::EnUs => format!(
                        "Installed plugin name {target} is ambiguous; use plugin@marketplace."
                    ),
                });
                self.deferred_route = None;
                return Vec::new();
            }
        };
        self.deferred_route = None;
        match action {
            InstalledDirectAction::Manage => {
                self.view = View::InstalledPlugin(plugin_id);
                self.selected = 0;
                Vec::new()
            }
            InstalledDirectAction::Enable | InstalledDirectAction::Disable => {
                let enabled = matches!(action, InstalledDirectAction::Enable);
                vec![self.begin(
                    UsagePluginRequestPurpose::PluginEnabledWrite {
                        plugin_id: plugin_id.clone(),
                    },
                    // Fixed `ManagePlugins.tsx` intentionally omitted the scope
                    // for enable/disable because configuration scope can differ
                    // from installation scope.  Preserve that authority lookup.
                    UsagePluginRuntimeAction::PluginSetEnabled {
                        plugin_id,
                        enabled,
                        scope: None,
                    },
                )]
            }
            InstalledDirectAction::Uninstall => {
                self.open_uninstall_for(plugin_id, language);
                Vec::new()
            }
        }
    }

    fn resolve_marketplace_route(
        &mut self,
        language: UiLanguage,
    ) -> Vec<UsagePluginManagementEffect> {
        let Some(DeferredRoute::Marketplace { target, action }) = self.deferred_route.clone()
        else {
            return Vec::new();
        };
        let matches = self
            .marketplaces
            .iter()
            .filter(|marketplace| marketplace.name == target)
            .map(|marketplace| marketplace.name.clone())
            .collect::<Vec<_>>();
        let Some(name) = matches.first().cloned() else {
            self.notice = Some(match language {
                UiLanguage::ZhCn => format!("未找到插件市场 {target}。"),
                UiLanguage::EnUs => format!("Marketplace {target} was not found."),
            });
            self.deferred_route = None;
            return Vec::new();
        };
        self.deferred_route = None;
        match action {
            MarketplaceDirectAction::Browse => {
                self.view = View::Catalog(name.clone());
                match validate_marketplace_name(&name) {
                    Ok(marketplace_name) => vec![self.begin(
                        UsagePluginRequestPurpose::MarketplaceCatalogRead {
                            marketplace_name: marketplace_name.clone(),
                        },
                        UsagePluginRuntimeAction::PluginMarketplaceCatalogRead { marketplace_name },
                    )],
                    Err(_) => Vec::new(),
                }
            }
            MarketplaceDirectAction::Update => vec![self.begin(
                UsagePluginRequestPurpose::MarketplaceUpdate {
                    marketplace_name: name.clone(),
                },
                UsagePluginRuntimeAction::PluginMarketplaceUpdate {
                    marketplace_name: name,
                },
            )],
            MarketplaceDirectAction::Remove => {
                let installed_count = self
                    .marketplaces
                    .iter()
                    .find(|marketplace| marketplace.name == name)
                    .map_or(0, |marketplace| marketplace.installed_plugin_count);
                if installed_count > 0 {
                    self.view = View::Marketplace(name);
                    self.notice = Some(language.text(
                        "该市场仍有已安装插件；当前直连 authority 会拒绝移除，请先卸载对应范围。",
                        "This marketplace still has installed plugins; the direct authority rejects removal until those scopes are uninstalled.",
                    ).to_string());
                } else {
                    self.view = View::MarketplaceRemoveConfirm(name);
                    self.selected = 0;
                }
                Vec::new()
            }
        }
    }

    fn find_catalog_matches(&self, target: &str) -> Vec<String> {
        let mut matches = self
            .catalogs
            .values()
            .flatten()
            .filter(|plugin| {
                plugin.id == target || plugin.name == target || plugin.display_name == target
            })
            .map(|plugin| plugin.id.clone())
            .collect::<Vec<_>>();
        matches.sort();
        matches.dedup();
        matches
    }

    fn find_installed_matches(&self, target: &str) -> Vec<String> {
        let mut matches = self
            .plugins
            .iter()
            .filter(|plugin| plugin.id == target || plugin.name == target)
            .map(|plugin| plugin.id.clone())
            .collect::<Vec<_>>();
        matches.sort();
        matches.dedup();
        matches
    }

    fn open_uninstall_for(&mut self, plugin_id: String, language: UiLanguage) {
        let scopes = self
            .plugins
            .iter()
            .find(|plugin| plugin.id == plugin_id)
            .map(|plugin| {
                plugin
                    .installations
                    .iter()
                    .filter_map(|installation| installation.scope.installable())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        match scopes.len() {
            0 => {
                self.view = View::InstalledPlugin(plugin_id);
                self.notice = Some(language.text(
                    "该插件没有可卸载的用户/项目/本地安装范围。",
                    "This plugin has no uninstallable user/project/local installation scope.",
                ).to_string());
            }
            1 => {
                if let Some(scope) = scopes.first().copied() {
                    self.view = View::UninstallConfirm { plugin_id, scope };
                    self.selected = 0;
                }
            }
            _ => {
                self.view = View::UninstallScope(plugin_id);
                self.selected = 0;
            }
        }
    }

    fn begin_next_queued_mutation(&mut self) -> Option<UsagePluginManagementEffect> {
        self.queued_mutations
            .pop_front()
            .map(|QueuedMutation::Install { plugin_id, scope }| {
                self.begin(
                    UsagePluginRequestPurpose::PluginInstall {
                        plugin_id: plugin_id.clone(),
                    },
                    UsagePluginRuntimeAction::PluginInstall { plugin_id, scope },
                )
            })
    }

    fn normalize_selection(&mut self) {
        let maximum = self.row_count_hint().saturating_sub(1);
        self.selected = self.selected.min(maximum);
    }

    fn row_count_hint(&self) -> usize {
        match &self.view {
            View::Usage => 2,
            View::Help => 1,
            View::Discover => self.filtered_discover_plugins().len() + 1,
            View::Catalog(name) => self.filtered_catalog_plugins(name).len() + 1,
            View::CatalogPlugin(_) => 4,
            View::Installed => self.filtered_installed_plugins().len() + 1,
            View::InstalledPlugin(plugin_id) => self.installed_action_rows(plugin_id).len(),
            View::UninstallScope(plugin_id) => self.uninstall_scopes(plugin_id).len() + 1,
            View::UninstallConfirm { .. } => 3,
            View::Marketplaces => self.marketplaces.len() + 2,
            View::Marketplace(_) => 5,
            View::MarketplaceRemoveConfirm(_) => 2,
            View::MarketplaceAdd | View::Validate => 0,
            View::Errors => self.load_diagnostics.len() + 1,
            View::ValidationResult => 2,
        }
    }
}

fn localized_runtime_error(
    language: UiLanguage,
    code: UsagePluginErrorCode,
    authority_message: &str,
) -> String {
    let label = match code {
        UsagePluginErrorCode::UsageUnavailable => {
            language.text("用量数据不可用", "Usage data is unavailable")
        }
        UsagePluginErrorCode::UsageWriteRejected => {
            language.text("用量偏好更新被拒绝", "Usage preference update was rejected")
        }
        UsagePluginErrorCode::PluginInventoryUnavailable => {
            language.text("插件清单不可用", "Plugin inventory is unavailable")
        }
        UsagePluginErrorCode::MarketplaceInventoryUnavailable => {
            language.text("市场清单不可用", "Marketplace inventory is unavailable")
        }
        UsagePluginErrorCode::MarketplaceCatalogUnavailable => {
            language.text("市场目录不可用", "Marketplace catalog is unavailable")
        }
        UsagePluginErrorCode::MarketplaceBlockedByPolicy => {
            language.text("市场被策略阻止", "Marketplace is blocked by policy")
        }
        UsagePluginErrorCode::InvalidMarketplaceSource => {
            language.text("市场来源无效", "Marketplace source is invalid")
        }
        UsagePluginErrorCode::PluginOperationRejected => {
            language.text("插件操作被拒绝", "Plugin operation was rejected")
        }
        UsagePluginErrorCode::MarketplaceOperationRejected => {
            language.text("市场操作被拒绝", "Marketplace operation was rejected")
        }
        UsagePluginErrorCode::ValidationUnavailable => {
            language.text("插件校验不可用", "Plugin validation is unavailable")
        }
        UsagePluginErrorCode::AuthorityFailure => {
            language.text("直连 authority 执行失败", "Direct authority failed")
        }
    };
    if authority_message.is_empty() {
        format!("{label}。")
    } else {
        format!("{label}：{authority_message}")
    }
}

fn mutation_notice_with_dependents(
    language: UiLanguage,
    zh: String,
    en: String,
    dependents: &[String],
) -> String {
    let base = match language {
        UiLanguage::ZhCn => zh,
        UiLanguage::EnUs => en,
    };
    if dependents.is_empty() {
        base
    } else {
        match language {
            UiLanguage::ZhCn => format!("{base} 受影响的反向依赖：{}。", dependents.join("、")),
            UiLanguage::EnUs => {
                format!(
                    "{base} Affected reverse dependents: {}.",
                    dependents.join(", ")
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstalledRowAction {
    Toggle(bool),
    Update(PluginScope),
    Uninstall(PluginInstallScope),
    Back,
}

impl UsagePluginManagementState {
    pub(crate) fn title(&self, language: UiLanguage) -> String {
        match &self.view {
            View::Usage => language.text("用量与额度", "Usage and limits"),
            View::Help => language.text("插件命令帮助", "Plugin command help"),
            View::Discover => language.text("发现插件", "Discover plugins"),
            View::Catalog(name) => {
                return match language {
                    UiLanguage::ZhCn => format!("插件市场：{name}"),
                    UiLanguage::EnUs => format!("Marketplace: {name}"),
                };
            }
            View::CatalogPlugin(_) => language.text("插件详情", "Plugin details"),
            View::Installed => language.text("已安装插件", "Installed plugins"),
            View::InstalledPlugin(_) => language.text("管理插件", "Manage plugin"),
            View::UninstallScope(_) => language.text("选择卸载范围", "Select uninstall scope"),
            View::UninstallConfirm { .. } => {
                language.text("确认卸载插件", "Confirm plugin uninstall")
            }
            View::Marketplaces => language.text("插件市场", "Plugin marketplaces"),
            View::Marketplace(_) => language.text("管理插件市场", "Manage marketplace"),
            View::MarketplaceRemoveConfirm(_) => {
                language.text("确认移除插件市场", "Confirm marketplace removal")
            }
            View::MarketplaceAdd => language.text("添加插件市场", "Add marketplace"),
            View::Errors => language.text("插件加载错误", "Plugin load errors"),
            View::Validate => language.text("校验插件", "Validate plugin"),
            View::ValidationResult => language.text("插件校验结果", "Plugin validation result"),
        }
        .to_string()
    }

    pub(crate) fn footer(&self, language: UiLanguage) -> &'static str {
        if self.pending.is_some() {
            if matches!(self.view, View::Usage) {
                return language.text(
                    "Esc 只关闭面板 · 正在等候直连运行环境 · Ctrl-Q 退出程序",
                    "Esc only closes this panel · waiting for direct runtime · Ctrl-Q quits",
                );
            }
            return language.text(
                "正在等待直连运行环境；Esc 只关闭面板，Ctrl-Q 退出程序",
                "Waiting for the direct runtime; Esc only closes this panel, Ctrl-Q quits",
            );
        }
        if matches!(self.view, View::Usage) {
            return language.text(
                "R 刷新 · Enter 执行 · Esc 关闭 · ↑/↓ 选择 · Ctrl-Q 退出",
                "R refresh · Enter run · Esc close · Up/Down select · Ctrl-Q quit",
            );
        }
        if matches!(self.view, View::MarketplaceAdd | View::Validate) {
            return language.text(
                "Enter 提交 · Esc 关闭面板 · Ctrl-Q 退出程序",
                "Enter submit · Esc closes panel · Ctrl-Q quits",
            );
        }
        if self.editing_query {
            return language.text(
                "输入筛选 · Enter 完成筛选 · Esc 关闭面板",
                "Type to filter · Enter finishes filter · Esc closes panel",
            );
        }
        language.text(
            "↑/↓ 选择 · Enter 确认 · / 搜索 · Tab 切换页签 · Esc 关闭面板",
            "Up/Down select · Enter confirm · / search · Tab switches tabs · Esc closes panel",
        )
    }

    pub(crate) fn tabs(&self, language: UiLanguage) -> Vec<UsagePluginManagementTabView> {
        let active = self.view.tab();
        if active.is_none() {
            return Vec::new();
        }
        PluginManagementTab::ALL
            .into_iter()
            .map(|tab| UsagePluginManagementTabView {
                label: tab.label(language).to_string(),
                active: active == Some(tab),
                badge: if tab == PluginManagementTab::Errors && !self.load_diagnostics.is_empty() {
                    Some(self.load_diagnostics.len())
                } else {
                    None
                },
            })
            .collect()
    }

    pub(crate) fn input(&self, language: UiLanguage) -> Option<(String, String, bool)> {
        match self.view {
            View::MarketplaceAdd => Some((
                language
                    .text(
                        "市场来源（URL、仓库或本地路径）",
                        "Marketplace source (URL, repository, or local path)",
                    )
                    .to_string(),
                self.input.clone(),
                false,
            )),
            View::Validate => Some((
                language
                    .text("插件或清单路径", "Plugin or manifest path")
                    .to_string(),
                self.input.clone(),
                false,
            )),
            _ if self.editing_query => Some((
                language.text("搜索", "Search").to_string(),
                self.query.clone(),
                false,
            )),
            _ => None,
        }
    }

    pub(crate) fn details(&self, language: UiLanguage) -> Vec<String> {
        match &self.view {
            View::Usage => self
                .usage_detail_lines(language, 90)
                .into_iter()
                .map(|line| line.text)
                .collect(),
            View::Help => plugin_help_lines(language),
            View::Discover => {
                let mut lines = vec![language
                    .text(
                        "目录由已配置且未被策略阻止的市场聚合；已全局安装的插件不在此列表重复显示。",
                        "Catalogs are aggregated from configured, policy-allowed marketplaces; globally installed plugins are omitted here.",
                    )
                    .to_string()];
                if !self.catalog_failures.is_empty() {
                    lines.push(match language {
                        UiLanguage::ZhCn => format!(
                            "未能读取的市场：{}",
                            self.catalog_failures.iter().cloned().collect::<Vec<_>>().join("、")
                        ),
                        UiLanguage::EnUs => format!(
                            "Catalogs unavailable: {}",
                            self.catalog_failures.iter().cloned().collect::<Vec<_>>().join(", ")
                        ),
                    });
                }
                if !self.catalog_truncated.is_empty() {
                    lines.push(match language {
                        UiLanguage::ZhCn => format!(
                            "目录已截断的市场：{}",
                            self.catalog_truncated
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join("、")
                        ),
                        UiLanguage::EnUs => format!(
                            "Truncated catalogs: {}",
                            self.catalog_truncated
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    });
                }
                if self.pending.is_none()
                    && self.catalog_queue.is_empty()
                    && self.filtered_discover_plugins().is_empty()
                {
                    let has_available_plugin = self
                        .catalogs
                        .values()
                        .flatten()
                        .any(|plugin| !plugin.globally_installed);
                    if !self.query.is_empty() && has_available_plugin {
                        lines.push(match language {
                            UiLanguage::ZhCn => {
                                format!("没有与「{}」匹配的插件。", self.query)
                            }
                            UiLanguage::EnUs => {
                                format!("No plugins match \"{}\".", self.query)
                            }
                        });
                    } else if let Some(reason) = self.discover_empty_reason {
                        lines.extend(
                            reason
                                .lines(language)
                                .into_iter()
                                .map(str::to_string),
                        );
                    }
                }
                lines
            }
            View::Catalog(name) => {
                let mut lines = self
                    .marketplaces
                    .iter()
                    .find(|marketplace| &marketplace.name == name)
                    .map(|marketplace| marketplace_details(marketplace, language))
                    .unwrap_or_default();
                if self.catalog_truncated.contains(name) {
                    lines.push(
                        language
                            .text(
                                "该市场目录已按闭合协议上限截断。",
                                "This marketplace catalog was truncated at the closed-contract limit.",
                            )
                            .to_string(),
                    );
                }
                lines
            }
            View::CatalogPlugin(plugin_id) => self
                .find_catalog_plugin(plugin_id)
                .map(|plugin| catalog_plugin_details(plugin, language))
                .unwrap_or_default(),
            View::Installed => {
                let mut lines = Vec::new();
                if self.inventory_truncated {
                    lines.push(language.text(
                        "插件清单已按闭合协议上限截断。",
                        "Plugin inventory was truncated at the closed-contract limit.",
                    ).to_string());
                }
                lines
            }
            View::InstalledPlugin(plugin_id)
            | View::UninstallScope(plugin_id)
            | View::UninstallConfirm { plugin_id, .. } => self
                .plugins
                .iter()
                .find(|plugin| &plugin.id == plugin_id)
                .map(|plugin| installed_plugin_details(plugin, language))
                .unwrap_or_default(),
            View::Marketplaces => {
                let mut lines = Vec::new();
                if self.marketplace_inventory_truncated {
                    lines.push(language.text(
                        "市场清单已按闭合协议上限截断。",
                        "Marketplace inventory was truncated at the closed-contract limit.",
                    ).to_string());
                }
                lines
            }
            View::Marketplace(name) | View::MarketplaceRemoveConfirm(name) => self
                .marketplaces
                .iter()
                .find(|marketplace| &marketplace.name == name)
                .map(|marketplace| marketplace_details(marketplace, language))
                .unwrap_or_default(),
            View::MarketplaceAdd => vec![language
                .text(
                    "示例：https://host/org/repo.git · github:org/repo · ./local-marketplace",
                    "Examples: https://host/org/repo.git · github:org/repo · ./local-marketplace",
                )
                .to_string()],
            View::Errors => self
                .load_diagnostics
                .iter()
                .map(|diagnostic| match (&diagnostic.plugin_name.0, language) {
                    (Some(name), UiLanguage::ZhCn) => {
                        format!("{}：{name}", diagnostic.error_type.label(language))
                    }
                    (Some(name), UiLanguage::EnUs) => {
                        format!("{}: {name}", diagnostic.error_type.label(language))
                    }
                    (None, _) => diagnostic.error_type.label(language).to_string(),
                })
                .collect(),
            View::Validate => vec![language
                .text(
                    "支持插件、市场、技能、代理、命令与 hooks 清单。校验失败只显示在本面板，不改变进程退出码。",
                    "Plugin, marketplace, skill, agent, command, and hooks manifests are supported. Validation failure stays in this panel and never changes the process exit code.",
                )
                .to_string()],
            View::ValidationResult => self.validation_details(language),
        }
    }

    pub(crate) fn detail_lines(
        &self,
        language: UiLanguage,
        available_width: u16,
    ) -> Vec<UsagePluginDetailLine> {
        if matches!(self.view, View::Usage) {
            self.usage_detail_lines(language, usize::from(available_width))
        } else {
            self.details(language)
                .into_iter()
                .map(|text| UsagePluginDetailLine {
                    text,
                    tone: UsagePluginDetailTone::Supporting,
                })
                .collect()
        }
    }

    pub(crate) const fn detail_line_limit(&self) -> usize {
        if matches!(self.view, View::Usage) {
            12
        } else {
            6
        }
    }

    pub(crate) fn rows(&self, language: UiLanguage) -> Vec<UsagePluginManagementRow> {
        let row = |label: String, detail: Option<String>, disabled: bool, marked: bool| {
            UsagePluginManagementRow {
                label,
                detail,
                disabled,
                marked,
            }
        };
        match &self.view {
            View::Usage => {
                let mut rows = vec![row(
                    language.text("刷新用量", "Refresh usage").to_string(),
                    None,
                    false,
                    false,
                )];
                if let Some(enabled) = self.five_hour_continue_enabled() {
                    rows.push(row(
                        if enabled {
                            language
                                .text(
                                    "停用五小时额度耗尽后继续使用",
                                    "Disable continue after five-hour limit",
                                )
                                .to_string()
                        } else {
                            language
                                .text(
                                    "启用五小时额度耗尽后继续使用",
                                    "Enable continue after five-hour limit",
                                )
                                .to_string()
                        },
                        Some(language.text(
                            "仅修改后端已有偏好，不改变额度计算。",
                            "Only changes the existing backend preference; limit accounting is unchanged.",
                        ).to_string()),
                        false,
                        false,
                    ));
                }
                rows
            }
            View::Help => vec![row(
                language.text("返回对话", "Back to chat").to_string(),
                None,
                false,
                false,
            )],
            View::Discover => {
                let mut rows = self
                    .filtered_discover_plugins()
                    .into_iter()
                    .map(|plugin| {
                        row(
                            plugin.display_name.clone(),
                            Some(catalog_plugin_row_detail(plugin, language)),
                            false,
                            self.marked_plugins.contains(&plugin.id),
                        )
                    })
                    .collect::<Vec<_>>();
                rows.push(row(
                    language.text("刷新目录", "Refresh catalogs").to_string(),
                    None,
                    false,
                    false,
                ));
                rows
            }
            View::Catalog(name) => {
                let mut rows = self
                    .filtered_catalog_plugins(name)
                    .into_iter()
                    .map(|plugin| {
                        row(
                            plugin.display_name.clone(),
                            Some(catalog_plugin_row_detail(plugin, language)),
                            false,
                            self.marked_plugins.contains(&plugin.id),
                        )
                    })
                    .collect::<Vec<_>>();
                rows.push(row(
                    language.text("返回发现", "Back to discover").to_string(),
                    None,
                    false,
                    false,
                ));
                rows
            }
            View::CatalogPlugin(plugin_id) => {
                let globally_installed = self
                    .find_catalog_plugin(plugin_id)
                    .is_some_and(|plugin| plugin.globally_installed);
                let mut rows = PluginInstallScope::ALL
                    .into_iter()
                    .map(|scope| {
                        row(
                            match language {
                                UiLanguage::ZhCn => format!("安装到{}范围", scope.label(language)),
                                UiLanguage::EnUs => {
                                    format!("Install in {} scope", scope.label(language))
                                }
                            },
                            None,
                            scope == PluginInstallScope::User && globally_installed,
                            false,
                        )
                    })
                    .collect::<Vec<_>>();
                rows.push(row(
                    language.text("返回", "Back").to_string(),
                    None,
                    false,
                    false,
                ));
                rows
            }
            View::Installed => {
                let mut rows = self
                    .filtered_installed_plugins()
                    .into_iter()
                    .map(|plugin| {
                        row(
                            plugin.name.clone(),
                            Some(installed_plugin_row_detail(plugin, language)),
                            false,
                            false,
                        )
                    })
                    .collect::<Vec<_>>();
                rows.push(row(
                    language
                        .text("刷新插件清单", "Refresh plugin inventory")
                        .to_string(),
                    None,
                    false,
                    false,
                ));
                rows
            }
            View::InstalledPlugin(plugin_id) => self
                .installed_actions(plugin_id)
                .into_iter()
                .map(|action| match action {
                    InstalledRowAction::Toggle(enabled) => row(
                        if enabled {
                            language.text("启用", "Enable").to_string()
                        } else {
                            language.text("停用", "Disable").to_string()
                        },
                        Some(
                            language
                                .text(
                                    "设置范围由现有 authority 精确解析。",
                                    "The existing authority resolves the exact settings scope.",
                                )
                                .to_string(),
                        ),
                        false,
                        false,
                    ),
                    InstalledRowAction::Update(scope) => row(
                        match language {
                            UiLanguage::ZhCn => format!("更新{}范围安装", scope.label(language)),
                            UiLanguage::EnUs => {
                                format!("Update {} installation", scope.label(language))
                            }
                        },
                        None,
                        false,
                        false,
                    ),
                    InstalledRowAction::Uninstall(scope) => row(
                        match language {
                            UiLanguage::ZhCn => format!("卸载{}范围安装", scope.label(language)),
                            UiLanguage::EnUs => {
                                format!("Uninstall {} installation", scope.label(language))
                            }
                        },
                        None,
                        false,
                        false,
                    ),
                    InstalledRowAction::Back => row(
                        language.text("返回", "Back").to_string(),
                        None,
                        false,
                        false,
                    ),
                })
                .collect(),
            View::UninstallScope(plugin_id) => {
                let mut rows = self
                    .uninstall_scopes(plugin_id)
                    .into_iter()
                    .map(|scope| {
                        row(
                            match language {
                                UiLanguage::ZhCn => format!("{}范围", scope.label(language)),
                                UiLanguage::EnUs => format!("{} scope", scope.label(language)),
                            },
                            None,
                            false,
                            false,
                        )
                    })
                    .collect::<Vec<_>>();
                rows.push(row(
                    language.text("返回", "Back").to_string(),
                    None,
                    false,
                    false,
                ));
                rows
            }
            View::UninstallConfirm { .. } => vec![
                row(
                    language
                        .text("卸载并保留插件数据", "Uninstall and preserve plugin data")
                        .to_string(),
                    None,
                    false,
                    false,
                ),
                row(
                    language
                        .text("卸载并删除插件数据", "Uninstall and delete plugin data")
                        .to_string(),
                    Some(
                        language
                            .text(
                                "删除数据不可由本面板恢复。",
                                "Deleted data cannot be restored by this panel.",
                            )
                            .to_string(),
                    ),
                    false,
                    false,
                ),
                row(
                    language.text("取消", "Cancel").to_string(),
                    None,
                    false,
                    false,
                ),
            ],
            View::Marketplaces => {
                let mut rows = self
                    .marketplaces
                    .iter()
                    .map(|marketplace| {
                        row(
                            marketplace.name.clone(),
                            Some(marketplace_row_detail(marketplace, language)),
                            false,
                            false,
                        )
                    })
                    .collect::<Vec<_>>();
                rows.push(row(
                    language
                        .text("＋ 添加插件市场", "+ Add marketplace")
                        .to_string(),
                    None,
                    false,
                    false,
                ));
                rows.push(row(
                    language
                        .text("刷新市场清单", "Refresh marketplaces")
                        .to_string(),
                    None,
                    false,
                    false,
                ));
                rows
            }
            View::Marketplace(name) => {
                let marketplace = self.marketplaces.iter().find(|entry| &entry.name == name);
                let installed = marketplace.map_or(0, |entry| entry.installed_plugin_count);
                let auto_update = marketplace.is_some_and(|entry| entry.auto_update);
                vec![
                    row(
                        language.text("浏览插件", "Browse plugins").to_string(),
                        None,
                        false,
                        false,
                    ),
                    row(
                        language
                            .text(
                                "更新市场与已安装插件",
                                "Update marketplace and installed plugins",
                            )
                            .to_string(),
                        None,
                        false,
                        false,
                    ),
                    row(
                        if auto_update {
                            language
                                .text("停用自动更新", "Disable automatic updates")
                                .to_string()
                        } else {
                            language
                                .text("启用自动更新", "Enable automatic updates")
                                .to_string()
                        },
                        None,
                        marketplace.is_none(),
                        false,
                    ),
                    row(
                        language.text("移除市场", "Remove marketplace").to_string(),
                        if installed > 0 {
                            Some(match language {
                                UiLanguage::ZhCn => {
                                    format!("仍有 {installed} 个已安装插件；请先卸载。")
                                }
                                UiLanguage::EnUs => format!(
                                    "{installed} installed plugins remain; uninstall them first."
                                ),
                            })
                        } else {
                            None
                        },
                        installed > 0,
                        false,
                    ),
                    row(
                        language.text("返回", "Back").to_string(),
                        None,
                        false,
                        false,
                    ),
                ]
            }
            View::MarketplaceRemoveConfirm(_) => vec![
                row(
                    language.text("取消", "Cancel").to_string(),
                    None,
                    false,
                    false,
                ),
                row(
                    language
                        .text("确认移除市场", "Confirm marketplace removal")
                        .to_string(),
                    None,
                    false,
                    false,
                ),
            ],
            View::MarketplaceAdd | View::Validate => Vec::new(),
            View::Errors => {
                let mut rows = self
                    .load_diagnostics
                    .iter()
                    .map(|diagnostic| {
                        row(
                            diagnostic.error_type.label(language).to_string(),
                            diagnostic.plugin_name.0.clone(),
                            false,
                            false,
                        )
                    })
                    .collect::<Vec<_>>();
                rows.push(row(
                    language.text("刷新错误", "Refresh errors").to_string(),
                    None,
                    false,
                    false,
                ));
                rows
            }
            View::ValidationResult => vec![
                row(
                    language.text("再次校验", "Validate again").to_string(),
                    None,
                    self.last_validation_path.is_none(),
                    false,
                ),
                row(
                    language.text("返回输入", "Back to input").to_string(),
                    None,
                    false,
                    false,
                ),
            ],
        }
    }

    pub(crate) fn paste(&mut self, text: &str) {
        if self.pending.is_some() {
            return;
        }
        if matches!(self.view, View::MarketplaceAdd | View::Validate) {
            insert_bounded(
                &mut self.input,
                &mut self.input_cursor,
                text,
                MAX_INPUT_STORAGE_BYTES,
            );
        } else if self.editing_query {
            insert_bounded(
                &mut self.query,
                &mut self.query_cursor,
                text,
                MAX_INPUT_STORAGE_BYTES,
            );
            self.selected = 0;
        }
    }

    pub(crate) fn key(
        &mut self,
        key: KeyEvent,
        language: UiLanguage,
    ) -> Vec<UsagePluginManagementEffect> {
        // Product override: Escape closes this modal and never terminates the
        // TUI.  It remains available while a direct request is pending.
        if matches!(key.code, KeyCode::Esc) {
            return vec![UsagePluginManagementEffect::Close];
        }
        if self.pending.is_some() {
            return Vec::new();
        }
        if matches!(self.view, View::MarketplaceAdd | View::Validate) {
            if matches!(key.code, KeyCode::Enter) {
                return self.submit_input(language);
            }
            edit_text(
                &mut self.input,
                &mut self.input_cursor,
                key,
                MAX_INPUT_STORAGE_BYTES,
            );
            return Vec::new();
        }
        if self.editing_query {
            if matches!(key.code, KeyCode::Enter) {
                self.editing_query = false;
            } else if edit_text(
                &mut self.query,
                &mut self.query_cursor,
                key,
                MAX_INPUT_STORAGE_BYTES,
            ) {
                self.selected = 0;
            }
            return Vec::new();
        }
        match key {
            KeyEvent {
                code: KeyCode::Char('/'),
                modifiers: KeyModifiers::NONE,
                ..
            } if self.view_supports_query() => {
                self.editing_query = true;
                self.query_cursor = self.query.len();
                Vec::new()
            }
            KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.selected = self.selected.saturating_sub(1);
                Vec::new()
            }
            KeyEvent {
                code: KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let maximum = self.rows(language).len().saturating_sub(1);
                self.selected = (self.selected + 1).min(maximum);
                Vec::new()
            }
            KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.switch_tab(false),
            KeyEvent {
                code: KeyCode::BackTab,
                modifiers: KeyModifiers::SHIFT,
                ..
            } => self.switch_tab(true),
            KeyEvent {
                code: KeyCode::Left,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.back();
                Vec::new()
            }
            KeyEvent {
                code: KeyCode::Char('r' | 'R'),
                modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
                ..
            } => self.refresh(),
            KeyEvent {
                code: KeyCode::Char(' '),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                self.toggle_mark();
                Vec::new()
            }
            KeyEvent {
                code: KeyCode::Char('i' | 'I'),
                modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
                ..
            } => self.install_marked_or_selected(),
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.accept(language),
            _ => Vec::new(),
        }
    }

    fn submit_input(&mut self, language: UiLanguage) -> Vec<UsagePluginManagementEffect> {
        let input = match safe_input(&self.input) {
            Ok(input) => input,
            Err(_) => {
                self.notice = Some(language.text(
                    "请输入非空、无控制字符且不超过 4096 个字符的值。",
                    "Enter a non-empty value without control characters, up to 4096 characters.",
                ).to_string());
                return Vec::new();
            }
        };
        match self.view {
            View::MarketplaceAdd => vec![self.begin(
                UsagePluginRequestPurpose::MarketplaceAdd,
                UsagePluginRuntimeAction::PluginMarketplaceAdd {
                    source_input: input,
                },
            )],
            View::Validate => {
                self.last_validation_path = Some(input.clone());
                vec![self.begin(
                    UsagePluginRequestPurpose::PluginValidate,
                    UsagePluginRuntimeAction::PluginValidate { path: input },
                )]
            }
            _ => Vec::new(),
        }
    }

    fn switch_tab(&mut self, backwards: bool) -> Vec<UsagePluginManagementEffect> {
        let Some(tab) = self.view.tab() else {
            return Vec::new();
        };
        let current = PluginManagementTab::ALL
            .iter()
            .position(|candidate| *candidate == tab)
            .unwrap_or_default();
        let next = if backwards {
            current
                .checked_sub(1)
                .unwrap_or(PluginManagementTab::ALL.len() - 1)
        } else {
            (current + 1) % PluginManagementTab::ALL.len()
        };
        self.selected = 0;
        self.query.clear();
        self.query_cursor = 0;
        self.editing_query = false;
        match PluginManagementTab::ALL[next] {
            PluginManagementTab::Discover => {
                self.view = View::Discover;
                self.refresh_discover()
            }
            PluginManagementTab::Installed => {
                self.view = View::Installed;
                vec![self.begin(
                    UsagePluginRequestPurpose::PluginInventoryRead,
                    UsagePluginRuntimeAction::PluginInventoryRead,
                )]
            }
            PluginManagementTab::Marketplaces => {
                self.view = View::Marketplaces;
                vec![self.begin(
                    UsagePluginRequestPurpose::MarketplaceInventoryRead,
                    UsagePluginRuntimeAction::PluginMarketplaceInventoryRead,
                )]
            }
            PluginManagementTab::Errors => {
                self.view = View::Errors;
                vec![self.begin(
                    UsagePluginRequestPurpose::PluginInventoryRead,
                    UsagePluginRuntimeAction::PluginInventoryRead,
                )]
            }
        }
    }

    fn refresh(&mut self) -> Vec<UsagePluginManagementEffect> {
        match self.view {
            View::Usage => vec![self.begin(
                UsagePluginRequestPurpose::UsageRead,
                UsagePluginRuntimeAction::UsageRead,
            )],
            View::Discover | View::Catalog(_) | View::CatalogPlugin(_) => self.refresh_discover(),
            View::Installed
            | View::InstalledPlugin(_)
            | View::UninstallScope(_)
            | View::UninstallConfirm { .. }
            | View::Errors => vec![self.begin(
                UsagePluginRequestPurpose::PluginInventoryRead,
                UsagePluginRuntimeAction::PluginInventoryRead,
            )],
            View::Marketplaces | View::Marketplace(_) | View::MarketplaceRemoveConfirm(_) => {
                vec![self.begin(
                    UsagePluginRequestPurpose::MarketplaceInventoryRead,
                    UsagePluginRuntimeAction::PluginMarketplaceInventoryRead,
                )]
            }
            View::ValidationResult => self.revalidate(),
            View::Help | View::MarketplaceAdd | View::Validate => Vec::new(),
        }
    }

    fn refresh_discover(&mut self) -> Vec<UsagePluginManagementEffect> {
        self.catalogs.clear();
        self.catalog_failures.clear();
        self.catalog_queue.clear();
        self.discover_empty_reason = None;
        vec![self.begin(
            UsagePluginRequestPurpose::MarketplaceInventoryRead,
            UsagePluginRuntimeAction::PluginMarketplaceInventoryRead,
        )]
    }

    fn back(&mut self) {
        self.selected = 0;
        self.notice = None;
        self.view = match &self.view {
            View::Catalog(_) | View::CatalogPlugin(_) => View::Discover,
            View::InstalledPlugin(_) => View::Installed,
            View::UninstallScope(plugin_id) | View::UninstallConfirm { plugin_id, .. } => {
                View::InstalledPlugin(plugin_id.clone())
            }
            View::Marketplace(_) | View::MarketplaceRemoveConfirm(_) | View::MarketplaceAdd => {
                View::Marketplaces
            }
            View::ValidationResult => View::Validate,
            other => other.clone(),
        };
    }

    fn accept(&mut self, language: UiLanguage) -> Vec<UsagePluginManagementEffect> {
        match &self.view {
            View::Usage => {
                if self.selected == 0 {
                    self.refresh()
                } else if self.selected == 1 {
                    self.five_hour_continue_enabled()
                        .map_or_else(Vec::new, |enabled| {
                            vec![self.begin(
                                UsagePluginRequestPurpose::UsagePreferenceWrite,
                                UsagePluginRuntimeAction::UsageSetFiveHourContinue {
                                    enabled: !enabled,
                                },
                            )]
                        })
                } else {
                    Vec::new()
                }
            }
            View::Help => vec![UsagePluginManagementEffect::Close],
            View::Discover => {
                let plugins = self.filtered_discover_plugins();
                if let Some(plugin) = plugins.get(self.selected) {
                    self.view = View::CatalogPlugin(plugin.id.clone());
                    self.selected = 0;
                    Vec::new()
                } else {
                    self.refresh_discover()
                }
            }
            View::Catalog(name) => {
                let plugins = self.filtered_catalog_plugins(name);
                if let Some(plugin) = plugins.get(self.selected) {
                    self.view = View::CatalogPlugin(plugin.id.clone());
                    self.selected = 0;
                } else {
                    self.view = View::Discover;
                    self.selected = 0;
                }
                Vec::new()
            }
            View::CatalogPlugin(plugin_id) => {
                if self.selected < PluginInstallScope::ALL.len() {
                    let scope = PluginInstallScope::ALL[self.selected];
                    let plugin_id = plugin_id.clone();
                    if scope == PluginInstallScope::User
                        && self
                            .find_catalog_plugin(&plugin_id)
                            .is_some_and(|plugin| plugin.globally_installed)
                    {
                        self.notice = Some(
                            language
                                .text(
                                    "该插件已全局安装；请选择项目或本地项目范围。",
                                    "This plugin is installed globally; choose project or local-project scope.",
                                )
                                .to_string(),
                        );
                        return Vec::new();
                    }
                    vec![self.begin(
                        UsagePluginRequestPurpose::PluginInstall {
                            plugin_id: plugin_id.clone(),
                        },
                        UsagePluginRuntimeAction::PluginInstall { plugin_id, scope },
                    )]
                } else {
                    self.back();
                    Vec::new()
                }
            }
            View::Installed => {
                let plugins = self.filtered_installed_plugins();
                if let Some(plugin) = plugins.get(self.selected) {
                    self.view = View::InstalledPlugin(plugin.id.clone());
                    self.selected = 0;
                    Vec::new()
                } else {
                    self.refresh()
                }
            }
            View::InstalledPlugin(plugin_id) => {
                let Some(action) = self
                    .installed_actions(plugin_id)
                    .get(self.selected)
                    .copied()
                else {
                    return Vec::new();
                };
                let plugin_id = plugin_id.clone();
                match action {
                    InstalledRowAction::Toggle(enabled) => vec![self.begin(
                        UsagePluginRequestPurpose::PluginEnabledWrite {
                            plugin_id: plugin_id.clone(),
                        },
                        UsagePluginRuntimeAction::PluginSetEnabled {
                            plugin_id,
                            enabled,
                            scope: None,
                        },
                    )],
                    InstalledRowAction::Update(scope) => vec![self.begin(
                        UsagePluginRequestPurpose::PluginUpdate {
                            plugin_id: plugin_id.clone(),
                        },
                        UsagePluginRuntimeAction::PluginUpdate { plugin_id, scope },
                    )],
                    InstalledRowAction::Uninstall(scope) => {
                        self.view = View::UninstallConfirm { plugin_id, scope };
                        self.selected = 0;
                        Vec::new()
                    }
                    InstalledRowAction::Back => {
                        self.back();
                        Vec::new()
                    }
                }
            }
            View::UninstallScope(plugin_id) => {
                let scopes = self.uninstall_scopes(plugin_id);
                if let Some(scope) = scopes.get(self.selected).copied() {
                    self.view = View::UninstallConfirm {
                        plugin_id: plugin_id.clone(),
                        scope,
                    };
                    self.selected = 0;
                } else {
                    self.back();
                }
                Vec::new()
            }
            View::UninstallConfirm { plugin_id, scope } => match self.selected {
                0 | 1 => vec![self.begin(
                    UsagePluginRequestPurpose::PluginUninstall {
                        plugin_id: plugin_id.clone(),
                    },
                    UsagePluginRuntimeAction::PluginUninstall {
                        plugin_id: plugin_id.clone(),
                        scope: *scope,
                        delete_data: self.selected == 1,
                    },
                )],
                _ => {
                    self.back();
                    Vec::new()
                }
            },
            View::Marketplaces => {
                if let Some(marketplace) = self.marketplaces.get(self.selected) {
                    self.view = View::Marketplace(marketplace.name.clone());
                    self.selected = 0;
                    Vec::new()
                } else if self.selected == self.marketplaces.len() {
                    self.view = View::MarketplaceAdd;
                    self.input.clear();
                    self.input_cursor = 0;
                    Vec::new()
                } else {
                    self.refresh()
                }
            }
            View::Marketplace(name) => {
                let name = name.clone();
                match self.selected {
                    0 => match validate_marketplace_name(&name) {
                        Ok(marketplace_name) => {
                            self.view = View::Catalog(marketplace_name.clone());
                            vec![self.begin(
                                UsagePluginRequestPurpose::MarketplaceCatalogRead {
                                    marketplace_name: marketplace_name.clone(),
                                },
                                UsagePluginRuntimeAction::PluginMarketplaceCatalogRead {
                                    marketplace_name,
                                },
                            )]
                        }
                        Err(_) => Vec::new(),
                    },
                    1 => vec![self.begin(
                        UsagePluginRequestPurpose::MarketplaceUpdate {
                            marketplace_name: name.clone(),
                        },
                        UsagePluginRuntimeAction::PluginMarketplaceUpdate {
                            marketplace_name: name,
                        },
                    )],
                    2 => {
                        let enabled = self
                            .marketplaces
                            .iter()
                            .find(|marketplace| marketplace.name == name)
                            .is_some_and(|marketplace| !marketplace.auto_update);
                        vec![self.begin(
                            UsagePluginRequestPurpose::MarketplaceAutoUpdateWrite {
                                marketplace_name: name.clone(),
                            },
                            UsagePluginRuntimeAction::PluginMarketplaceSetAutoUpdate {
                                marketplace_name: name,
                                enabled,
                            },
                        )]
                    }
                    3 => {
                        let installed = self
                            .marketplaces
                            .iter()
                            .find(|marketplace| marketplace.name == name)
                            .map_or(0, |marketplace| marketplace.installed_plugin_count);
                        if installed == 0 {
                            self.view = View::MarketplaceRemoveConfirm(name);
                            self.selected = 0;
                        } else {
                            self.notice = Some(language.text(
                                "该市场仍有已安装插件，不能移除。",
                                "This marketplace still has installed plugins and cannot be removed.",
                            ).to_string());
                        }
                        Vec::new()
                    }
                    _ => {
                        self.back();
                        Vec::new()
                    }
                }
            }
            View::MarketplaceRemoveConfirm(name) => {
                if self.selected == 1 {
                    vec![self.begin(
                        UsagePluginRequestPurpose::MarketplaceRemove {
                            marketplace_name: name.clone(),
                        },
                        UsagePluginRuntimeAction::PluginMarketplaceRemove {
                            marketplace_name: name.clone(),
                        },
                    )]
                } else {
                    self.back();
                    Vec::new()
                }
            }
            View::Errors => {
                if self.selected >= self.load_diagnostics.len() {
                    self.refresh()
                } else {
                    Vec::new()
                }
            }
            View::ValidationResult => {
                if self.selected == 0 {
                    self.revalidate()
                } else {
                    self.view = View::Validate;
                    self.selected = 0;
                    Vec::new()
                }
            }
            View::MarketplaceAdd | View::Validate => Vec::new(),
        }
    }

    fn revalidate(&mut self) -> Vec<UsagePluginManagementEffect> {
        let Some(path) = self.last_validation_path.clone() else {
            return Vec::new();
        };
        vec![self.begin(
            UsagePluginRequestPurpose::PluginValidate,
            UsagePluginRuntimeAction::PluginValidate { path },
        )]
    }

    fn toggle_mark(&mut self) {
        let plugin_id = match &self.view {
            View::Discover => self
                .filtered_discover_plugins()
                .get(self.selected)
                .map(|plugin| plugin.id.clone()),
            View::Catalog(name) => self
                .filtered_catalog_plugins(name)
                .get(self.selected)
                .map(|plugin| plugin.id.clone()),
            _ => None,
        };
        if let Some(plugin_id) = plugin_id {
            if self
                .find_catalog_plugin(&plugin_id)
                .is_some_and(|plugin| plugin.globally_installed)
            {
                return;
            }
            if !self.marked_plugins.remove(&plugin_id) {
                self.marked_plugins.insert(plugin_id);
            }
        }
    }

    fn install_marked_or_selected(&mut self) -> Vec<UsagePluginManagementEffect> {
        let mut plugin_ids = if self.marked_plugins.is_empty() {
            match &self.view {
                View::Discover => self
                    .filtered_discover_plugins()
                    .get(self.selected)
                    .map(|plugin| vec![plugin.id.clone()])
                    .unwrap_or_default(),
                View::Catalog(name) => self
                    .filtered_catalog_plugins(name)
                    .get(self.selected)
                    .map(|plugin| vec![plugin.id.clone()])
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        } else {
            self.marked_plugins.iter().cloned().collect::<Vec<_>>()
        };
        plugin_ids.retain(|plugin_id| {
            self.find_catalog_plugin(plugin_id)
                .is_none_or(|plugin| !plugin.globally_installed)
        });
        plugin_ids.sort();
        self.queued_mutations
            .extend(plugin_ids.into_iter().map(|plugin_id| {
                // Fixed BrowseMarketplace/DiscoverPlugins used user scope for the
                // `i`/batch shortcut; explicit detail rows expose all three scopes.
                QueuedMutation::Install {
                    plugin_id,
                    scope: PluginInstallScope::User,
                }
            }));
        self.begin_next_queued_mutation().into_iter().collect()
    }

    fn view_supports_query(&self) -> bool {
        matches!(
            self.view,
            View::Discover | View::Catalog(_) | View::Installed
        )
    }

    fn five_hour_continue_enabled(&self) -> Option<bool> {
        self.usage
            .as_ref()
            .and_then(|usage| usage.utilization.as_ref())
            .and_then(|utilization| utilization.five_hour_continue_enabled.0)
    }

    fn find_catalog_plugin(&self, plugin_id: &str) -> Option<&MarketplaceCatalogPlugin> {
        self.catalogs
            .values()
            .flatten()
            .find(|plugin| plugin.id == plugin_id)
    }

    fn filtered_discover_plugins(&self) -> Vec<&MarketplaceCatalogPlugin> {
        let query = self.query.to_lowercase();
        let mut seen = BTreeSet::new();
        let mut plugins = self
            .catalogs
            .values()
            .flatten()
            .filter(|plugin| !plugin.globally_installed)
            .filter(|plugin| catalog_matches_query(plugin, &query))
            .filter(|plugin| seen.insert(plugin.id.clone()))
            .collect::<Vec<_>>();
        plugins.sort_by(|left, right| {
            right
                .install_count
                .0
                .cmp(&left.install_count.0)
                .then_with(|| left.id.cmp(&right.id))
        });
        plugins
    }

    fn filtered_catalog_plugins(&self, marketplace_name: &str) -> Vec<&MarketplaceCatalogPlugin> {
        let query = self.query.to_lowercase();
        self.catalogs
            .get(marketplace_name)
            .into_iter()
            .flatten()
            .filter(|plugin| catalog_matches_query(plugin, &query))
            .collect()
    }

    fn filtered_installed_plugins(&self) -> Vec<&PluginInventoryEntry> {
        let query = self.query.to_lowercase();
        self.plugins
            .iter()
            .filter(|plugin| {
                query.is_empty()
                    || plugin.id.to_lowercase().contains(&query)
                    || plugin.name.to_lowercase().contains(&query)
                    || plugin.marketplace.to_lowercase().contains(&query)
                    || plugin
                        .description
                        .0
                        .as_deref()
                        .is_some_and(|description| description.to_lowercase().contains(&query))
            })
            .collect()
    }

    fn uninstall_scopes(&self, plugin_id: &str) -> Vec<PluginInstallScope> {
        self.plugins
            .iter()
            .find(|plugin| plugin.id == plugin_id)
            .map(|plugin| {
                plugin
                    .installations
                    .iter()
                    .filter_map(|installation| installation.scope.installable())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn installed_actions(&self, plugin_id: &str) -> Vec<InstalledRowAction> {
        let Some(plugin) = self.plugins.iter().find(|plugin| plugin.id == plugin_id) else {
            return vec![InstalledRowAction::Back];
        };
        let mut actions = Vec::new();
        let settings_editable = !matches!(
            plugin.configured_scope.0,
            Some(PluginConfiguredScope::Managed | PluginConfiguredScope::Flag)
        );
        if settings_editable {
            actions.push(InstalledRowAction::Toggle(!plugin.enabled));
        }
        if !plugin.is_builtin {
            let managed_configuration =
                plugin.configured_scope.0 == Some(PluginConfiguredScope::Managed);
            for scope in plugin
                .installations
                .iter()
                .map(|installation| installation.scope)
                .collect::<BTreeSet<_>>()
            {
                actions.push(InstalledRowAction::Update(scope));
                if !managed_configuration && let Some(scope) = scope.installable() {
                    actions.push(InstalledRowAction::Uninstall(scope));
                }
            }
        }
        actions.push(InstalledRowAction::Back);
        actions
    }

    fn installed_action_rows(&self, plugin_id: &str) -> Vec<InstalledRowAction> {
        self.installed_actions(plugin_id)
    }

    fn usage_detail_lines(
        &self,
        language: UiLanguage,
        available_width: usize,
    ) -> Vec<UsagePluginDetailLine> {
        let Some(usage) = self.usage.as_ref() else {
            return vec![detail_line(
                language.text("正在读取用量与额度…", "Loading usage and limits…"),
                UsagePluginDetailTone::Metric,
            )];
        };
        let narrow = available_width < 76;
        let mut core = Vec::new();
        let mut backend_values = Vec::new();

        if let Some(balance) = usage.entitlement_balance.as_ref() {
            core.push(detail_line(
                match language {
                    UiLanguage::ZhCn => format!(
                        "额度余额 · 有效额度 {} 项",
                        format_u64(balance.active_entitlements)
                    ),
                    UiLanguage::EnUs => format!(
                        "Entitlement balance · {} active entitlement{}",
                        format_u64(balance.active_entitlements),
                        if balance.active_entitlements == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                },
                UsagePluginDetailTone::Section,
            ));

            let token = balance_metric_detail(
                "Token",
                balance.total_token_used,
                balance.total_token_quota,
                balance.total_token_remaining,
                language,
                narrow,
            );
            core.push(token.summary);
            if let Some(raw) = token.backend_value {
                backend_values.push(raw);
            }

            if balance.total_call_quota < 0.0 || balance.total_call_remaining < 0.0 {
                core.push(detail_line(
                    language.text(
                        "调用 · 后端未提供独立调用额度",
                        "Calls · separate call quota not reported",
                    ),
                    UsagePluginDetailTone::Supporting,
                ));
            } else {
                let calls = balance_metric_detail(
                    language.text("调用", "Calls"),
                    balance.total_call_used,
                    balance.total_call_quota,
                    balance.total_call_remaining,
                    language,
                    narrow,
                );
                core.push(calls.summary);
                if let Some(raw) = calls.backend_value {
                    backend_values.push(raw);
                }
            }
        } else {
            core.push(detail_line(
                language.text("额度余额", "Entitlement balance"),
                UsagePluginDetailTone::Section,
            ));
            core.push(detail_line(
                language.text(
                    "运行环境未返回 Token、调用或有效额度数据。",
                    "The runtime returned no token, call, or entitlement balance.",
                ),
                UsagePluginDetailTone::Supporting,
            ));
        }

        core.push(detail_line(
            language.text("时间窗口", "Usage windows"),
            UsagePluginDetailTone::Section,
        ));
        if let Some(utilization) = usage.utilization.as_ref() {
            if let Some(limit) = utilization.five_hour.0.as_ref() {
                core.extend(rate_limit_lines(
                    language.text("当前会话", "Current session"),
                    language.text("5 小时窗口", "5-hour window"),
                    limit,
                    language,
                    narrow,
                ));
            } else {
                core.push(detail_line(
                    language.text(
                        "当前会话 · 5 小时窗口未提供",
                        "Current session · 5-hour window not reported",
                    ),
                    UsagePluginDetailTone::Supporting,
                ));
            }
            if let Some(limit) = utilization.seven_day.0.as_ref() {
                core.extend(rate_limit_lines(
                    language.text("七天用量", "Seven-day usage"),
                    language.text("滚动窗口", "rolling window"),
                    limit,
                    language,
                    narrow,
                ));
            } else {
                core.push(detail_line(
                    language.text(
                        "七天用量 · 滚动窗口未提供",
                        "Seven-day usage · rolling window not reported",
                    ),
                    UsagePluginDetailTone::Supporting,
                ));
            }
            if let Some(extra) = utilization.extra_usage.0.as_ref() {
                let extra_lines = extra_usage_detail_lines(extra, language, narrow);
                core.extend(extra_lines.core);
                backend_values.extend(extra_lines.backend_values);
            }
        } else {
            core.push(detail_line(
                language.text(
                    "当前会话与七天用量均未由运行环境返回。",
                    "Current-session and seven-day usage were not returned by the runtime.",
                ),
                UsagePluginDetailTone::Supporting,
            ));
        }

        const MAX_USAGE_DETAIL_LINES: usize = 12;
        let remaining = MAX_USAGE_DETAIL_LINES.saturating_sub(core.len());
        if backend_values.len() <= remaining {
            core.extend(backend_values);
        } else if remaining > 0 {
            core.extend(backend_values.into_iter().take(remaining.saturating_sub(1)));
            core.push(detail_line(
                language.text(
                    "部分后端原值因面板高度省略；紧凑值以 ≈ 标记，快照未改写。",
                    "Some backend values are omitted for height; ≈ marks compact values and the snapshot is unchanged.",
                ),
                UsagePluginDetailTone::Supporting,
            ));
        }
        core
    }

    fn validation_details(&self, language: UiLanguage) -> Vec<String> {
        let Some(validation) = self.validation.as_ref() else {
            return Vec::new();
        };
        let mut lines = vec![match language {
            UiLanguage::ZhCn => format!(
                "{} · 类型 {} · 关联结果 {}",
                if validation.success {
                    "校验通过"
                } else {
                    "校验失败"
                },
                validation.file_type.as_str(),
                validation.related_result_count
            ),
            UiLanguage::EnUs => format!(
                "{} · type {} · {} related results",
                if validation.success {
                    "Valid"
                } else {
                    "Invalid"
                },
                validation.file_type.as_str(),
                validation.related_result_count
            ),
        }];
        for diagnostic in &validation.errors {
            lines.push(validation_diagnostic_line(diagnostic, true, language));
        }
        for diagnostic in &validation.warnings {
            lines.push(validation_diagnostic_line(diagnostic, false, language));
        }
        if validation.truncated {
            lines.push(
                language
                    .text(
                        "诊断已按闭合协议上限截断。",
                        "Diagnostics were truncated at the closed-contract limit.",
                    )
                    .to_string(),
            );
        }
        lines
    }
}

fn insert_bounded(value: &mut String, cursor: &mut usize, text: &str, maximum: usize) {
    let safe = text
        .chars()
        .filter(|character| !character.is_control() && *character != '\u{7f}')
        .collect::<String>();
    let remaining = maximum.saturating_sub(value.len());
    let mut end = safe.len().min(remaining);
    while end > 0 && !safe.is_char_boundary(end) {
        end -= 1;
    }
    value.insert_str(*cursor, &safe[..end]);
    *cursor += end;
}

fn edit_text(value: &mut String, cursor: &mut usize, key: KeyEvent, maximum: usize) -> bool {
    match key {
        KeyEvent {
            code: KeyCode::Char(character),
            modifiers,
            ..
        } if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && !character.is_control() =>
        {
            let mut buffer = [0_u8; 4];
            insert_bounded(value, cursor, character.encode_utf8(&mut buffer), maximum);
            true
        }
        KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::NONE,
            ..
        } => {
            if *cursor > 0 {
                let previous = value[..*cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(index, _)| index);
                value.replace_range(previous..*cursor, "");
                *cursor = previous;
            }
            true
        }
        KeyEvent {
            code: KeyCode::Delete,
            modifiers: KeyModifiers::NONE,
            ..
        } => {
            if *cursor < value.len() {
                let next = value[*cursor..]
                    .char_indices()
                    .nth(1)
                    .map_or(value.len(), |(offset, _)| *cursor + offset);
                value.replace_range(*cursor..next, "");
            }
            true
        }
        KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::NONE,
            ..
        } => {
            *cursor = value[..*cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(index, _)| index);
            true
        }
        KeyEvent {
            code: KeyCode::Right,
            modifiers: KeyModifiers::NONE,
            ..
        } => {
            *cursor = value[*cursor..]
                .char_indices()
                .nth(1)
                .map_or(value.len(), |(offset, _)| *cursor + offset);
            true
        }
        KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::NONE,
            ..
        } => {
            *cursor = 0;
            true
        }
        KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
            ..
        } => {
            *cursor = value.len();
            true
        }
        _ => false,
    }
}

fn catalog_matches_query(plugin: &MarketplaceCatalogPlugin, query: &str) -> bool {
    query.is_empty()
        || plugin.id.to_lowercase().contains(query)
        || plugin.name.to_lowercase().contains(query)
        || plugin.display_name.to_lowercase().contains(query)
        || plugin
            .description
            .0
            .as_deref()
            .is_some_and(|description| description.to_lowercase().contains(query))
}

fn catalog_plugin_row_detail(plugin: &MarketplaceCatalogPlugin, language: UiLanguage) -> String {
    let mut parts = Vec::new();
    if let Some(version) = plugin.version.0.as_ref() {
        parts.push(format!("v{version}"));
    }
    if let Some(category) = plugin.category.0.as_ref() {
        parts.push(category.clone());
    }
    if let Some(count) = plugin.install_count.0 {
        parts.push(match language {
            UiLanguage::ZhCn => format!("安装 {count} 次"),
            UiLanguage::EnUs => format!("{count} installs"),
        });
    }
    if plugin.globally_installed {
        parts.push(
            language
                .text("已全局安装", "installed globally")
                .to_string(),
        );
    } else if plugin.enabled {
        parts.push(language.text("已启用", "enabled").to_string());
    }
    parts.join(" · ")
}

fn catalog_plugin_details(plugin: &MarketplaceCatalogPlugin, language: UiLanguage) -> Vec<String> {
    let mut lines = vec![plugin.id.clone()];
    if let Some(description) = plugin.description.0.as_ref() {
        lines.push(description.clone());
    }
    if !plugin.tags.is_empty() {
        lines.push(match language {
            UiLanguage::ZhCn => format!("标签：{}", plugin.tags.join("、")),
            UiLanguage::EnUs => format!("Tags: {}", plugin.tags.join(", ")),
        });
    }
    if let Some(version) = plugin.version.0.as_ref() {
        lines.push(match language {
            UiLanguage::ZhCn => format!("版本：{version}"),
            UiLanguage::EnUs => format!("Version: {version}"),
        });
    }
    if let Some(category) = plugin.category.0.as_ref() {
        lines.push(match language {
            UiLanguage::ZhCn => format!("分类：{category}"),
            UiLanguage::EnUs => format!("Category: {category}"),
        });
    }
    lines.push(match language {
        UiLanguage::ZhCn => format!(
            "状态：{}{}",
            if plugin.enabled {
                "已启用"
            } else {
                "未启用"
            },
            if plugin.globally_installed {
                " · 已全局安装"
            } else {
                ""
            }
        ),
        UiLanguage::EnUs => format!(
            "Status: {}{}",
            if plugin.enabled {
                "enabled"
            } else {
                "not enabled"
            },
            if plugin.globally_installed {
                " · installed globally"
            } else {
                ""
            }
        ),
    });
    if let Some(count) = plugin.install_count.0 {
        lines.push(match language {
            UiLanguage::ZhCn => format!("安装次数：{count}"),
            UiLanguage::EnUs => format!("Install count: {count}"),
        });
    }
    for installation in &plugin.installations {
        let mut detail = match language {
            UiLanguage::ZhCn => format!("已安装范围：{}", installation.scope.label(language)),
            UiLanguage::EnUs => {
                format!("Installed scope: {}", installation.scope.label(language))
            }
        };
        if let Some(version) = installation.version.0.as_ref() {
            detail.push_str(&format!(" · v{version}"));
        }
        if let Some(installed_at) = installation.installed_at.0.as_ref() {
            detail.push_str(&format!(" · {installed_at}"));
        }
        if let Some(updated_at) = installation.last_updated.0.as_ref() {
            detail.push_str(&format!(" · {updated_at}"));
        }
        lines.push(detail);
    }
    lines.push(language.text(
        "安全提示：第三方插件可能包含工具、钩子或可执行组件；仅安装你信任的来源。",
        "Security: third-party plugins may contain tools, hooks, or executable components; install only trusted sources.",
    ).to_string());
    lines
}

fn installed_plugin_row_detail(plugin: &PluginInventoryEntry, language: UiLanguage) -> String {
    let mut parts = vec![plugin.marketplace.clone()];
    parts.push(if plugin.loaded {
        if plugin.enabled {
            language.text("已启用", "enabled").to_string()
        } else {
            language.text("已停用", "disabled").to_string()
        }
    } else {
        language.text("未加载", "not loaded").to_string()
    });
    if let Some(scope) = plugin.configured_scope.0 {
        parts.push(scope.label(language).to_string());
    }
    parts.join(" · ")
}

fn installed_plugin_details(plugin: &PluginInventoryEntry, language: UiLanguage) -> Vec<String> {
    let mut lines = vec![plugin.id.clone()];
    if plugin.is_builtin {
        lines.push(language.text("内置插件", "Built-in plugin").to_string());
    }
    if let Some(description) = plugin.description.0.as_ref() {
        lines.push(description.clone());
    }
    if let Some(version) = plugin.version.0.as_ref() {
        lines.push(match language {
            UiLanguage::ZhCn => format!("当前版本：{version}"),
            UiLanguage::EnUs => format!("Current version: {version}"),
        });
    }
    for installation in &plugin.installations {
        let mut detail = installation.scope.label(language).to_string();
        if let Some(version) = installation.version.0.as_ref() {
            detail.push_str(&format!(" · v{version}"));
        }
        if let Some(installed_at) = installation.installed_at.0.as_ref() {
            detail.push_str(&format!(" · {installed_at}"));
        }
        if let Some(updated) = installation.last_updated.0.as_ref() {
            detail.push_str(&format!(" · {updated}"));
        }
        lines.push(detail);
    }
    lines
}

fn marketplace_row_detail(marketplace: &MarketplaceInventoryEntry, language: UiLanguage) -> String {
    let plugin_count = marketplace
        .plugin_count
        .0
        .map_or_else(|| "—".to_string(), |count| count.to_string());
    match language {
        UiLanguage::ZhCn => format!(
            "{} · 插件 {} · 已安装 {} · 自动更新 {}{}",
            marketplace.source_kind.as_str(),
            plugin_count,
            marketplace.installed_plugin_count,
            if marketplace.auto_update {
                "开"
            } else {
                "关"
            },
            if marketplace.load_failed {
                " · 加载失败"
            } else {
                ""
            }
        ),
        UiLanguage::EnUs => format!(
            "{} · {} plugins · {} installed · auto-update {}{}",
            marketplace.source_kind.as_str(),
            plugin_count,
            marketplace.installed_plugin_count,
            if marketplace.auto_update { "on" } else { "off" },
            if marketplace.load_failed {
                " · load failed"
            } else {
                ""
            }
        ),
    }
}

fn marketplace_details(
    marketplace: &MarketplaceInventoryEntry,
    language: UiLanguage,
) -> Vec<String> {
    let mut lines = vec![marketplace_row_detail(marketplace, language)];
    if let Some(last_updated) = marketplace.last_updated.0.as_ref() {
        lines.push(match language {
            UiLanguage::ZhCn => format!("最后更新：{last_updated}"),
            UiLanguage::EnUs => format!("Last updated: {last_updated}"),
        });
    }
    lines
}

fn plugin_help_lines(language: UiLanguage) -> Vec<String> {
    let descriptions = match language {
        UiLanguage::ZhCn => [
            "/plugin · 发现插件",
            "/plugin install [plugin|source] · 安装插件或添加市场",
            "/plugin manage [plugin] · 管理已安装插件",
            "/plugin enable|disable <plugin> · 启停插件",
            "/plugin uninstall <plugin> · 选择范围并确认卸载",
            "/plugin marketplace [list|add|remove|update] · 管理市场",
            "/plugin validate [path] · 校验插件或清单",
            "/plugins 与 /marketplace 使用同一原生面板",
        ],
        UiLanguage::EnUs => [
            "/plugin · discover plugins",
            "/plugin install [plugin|source] · install a plugin or add a marketplace",
            "/plugin manage [plugin] · manage installed plugins",
            "/plugin enable|disable <plugin> · enable or disable a plugin",
            "/plugin uninstall <plugin> · select scope and confirm uninstall",
            "/plugin marketplace [list|add|remove|update] · manage marketplaces",
            "/plugin validate [path] · validate a plugin or manifest",
            "/plugins and /marketplace use the same native panel",
        ],
    };
    descriptions.into_iter().map(str::to_string).collect()
}

fn detail_line(text: impl Into<String>, tone: UsagePluginDetailTone) -> UsagePluginDetailLine {
    UsagePluginDetailLine {
        text: text.into(),
        tone,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AmountDisplay {
    compact: String,
    backend_value: Option<String>,
    unusually_large: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BalanceMetricDetail {
    summary: UsagePluginDetailLine,
    backend_value: Option<UsagePluginDetailLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtraUsageDetailLines {
    core: Vec<UsagePluginDetailLine>,
    backend_values: Vec<UsagePluginDetailLine>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BalanceMetricStatus {
    UsedPercentage(f64),
    Empty,
    Inconsistent,
    UnusuallyLarge,
}

fn balance_metric_detail(
    label: &str,
    used: f64,
    quota: f64,
    remaining: f64,
    language: UiLanguage,
    narrow: bool,
) -> BalanceMetricDetail {
    let used_display = format_amount(used);
    let quota_display = format_amount(quota);
    let remaining_display = format_amount(remaining);
    let status = balance_metric_status(used, quota, remaining);
    let status_text = match (status, language) {
        (BalanceMetricStatus::UsedPercentage(value), UiLanguage::ZhCn) => {
            format!("{} 已用", format_percentage(value))
        }
        (BalanceMetricStatus::UsedPercentage(value), UiLanguage::EnUs) => {
            format!("{} used", format_percentage(value))
        }
        (BalanceMetricStatus::Empty, UiLanguage::ZhCn) => "未分配额度".to_string(),
        (BalanceMetricStatus::Empty, UiLanguage::EnUs) => "no quota allocated".to_string(),
        (BalanceMetricStatus::Inconsistent, UiLanguage::ZhCn) => {
            "⚠ 后端数值不一致，未计算比例".to_string()
        }
        (BalanceMetricStatus::Inconsistent, UiLanguage::EnUs) => {
            "⚠ inconsistent backend values; percentage not calculated".to_string()
        }
        (BalanceMetricStatus::UnusuallyLarge, UiLanguage::ZhCn) => {
            "⚠ 后端值异常大，未计算比例".to_string()
        }
        (BalanceMetricStatus::UnusuallyLarge, UiLanguage::EnUs) => {
            "⚠ unusually large backend value; percentage not calculated".to_string()
        }
    };
    let summary = match (language, narrow) {
        (UiLanguage::ZhCn, true) => format!(
            "{label} · {status_text} · {}/{} · 可用 {}",
            used_display.compact, quota_display.compact, remaining_display.compact
        ),
        (UiLanguage::ZhCn, false) => format!(
            "{label} · 已用 {} / 总额 {} · 可用 {} · {status_text}",
            used_display.compact, quota_display.compact, remaining_display.compact
        ),
        (UiLanguage::EnUs, true) => format!(
            "{label} · {status_text} · {}/{} · {} left",
            used_display.compact, quota_display.compact, remaining_display.compact
        ),
        (UiLanguage::EnUs, false) => format!(
            "{label} · {} used / {} total · {} left · {status_text}",
            used_display.compact, quota_display.compact, remaining_display.compact
        ),
    };
    let tone = if matches!(
        status,
        BalanceMetricStatus::Inconsistent | BalanceMetricStatus::UnusuallyLarge
    ) {
        UsagePluginDetailTone::Warning
    } else {
        UsagePluginDetailTone::Metric
    };
    let backend_value = if used_display.backend_value.is_some()
        || quota_display.backend_value.is_some()
        || remaining_display.backend_value.is_some()
    {
        let used = used_display
            .backend_value
            .clone()
            .unwrap_or_else(|| used_display.compact.clone());
        let quota = quota_display
            .backend_value
            .clone()
            .unwrap_or_else(|| quota_display.compact.clone());
        let remaining = remaining_display
            .backend_value
            .clone()
            .unwrap_or_else(|| remaining_display.compact.clone());
        Some(detail_line(
            match language {
                UiLanguage::ZhCn => {
                    format!("{label} 后端原值 · 已用 {used} / 总额 {quota} / 可用 {remaining}")
                }
                UiLanguage::EnUs => format!(
                    "{label} backend values · {used} used / {quota} total / {remaining} left"
                ),
            },
            UsagePluginDetailTone::Supporting,
        ))
    } else {
        None
    };

    BalanceMetricDetail {
        summary: detail_line(summary, tone),
        backend_value,
    }
}

fn balance_metric_status(used: f64, quota: f64, remaining: f64) -> BalanceMetricStatus {
    if [used, quota, remaining]
        .into_iter()
        .any(|value| value.abs() >= 1.0e15)
    {
        return BalanceMetricStatus::UnusuallyLarge;
    }
    if quota == 0.0 && used == 0.0 && remaining == 0.0 {
        return BalanceMetricStatus::Empty;
    }
    if quota <= 0.0 {
        return BalanceMetricStatus::Inconsistent;
    }
    let tolerance = (quota.abs() * 0.005).max(1.0);
    if used > quota + tolerance
        || remaining > quota + tolerance
        || (used + remaining - quota).abs() > tolerance
    {
        BalanceMetricStatus::Inconsistent
    } else {
        BalanceMetricStatus::UsedPercentage(used / quota * 100.0)
    }
}

fn rate_limit_lines(
    label: &str,
    window: &str,
    limit: &RateLimitSnapshot,
    language: UiLanguage,
    narrow: bool,
) -> Vec<UsagePluginDetailLine> {
    let usage = limit.utilization.0.map_or_else(
        || {
            language
                .text("用量未提供", "usage not reported")
                .to_string()
        },
        |value| match language {
            UiLanguage::ZhCn => format!(
                "{} 已用 · {} 可用",
                format_percentage(value),
                format_percentage(100.0 - value)
            ),
            UiLanguage::EnUs => format!(
                "{} used · {} available",
                format_percentage(value),
                format_percentage(100.0 - value)
            ),
        },
    );
    let reset = limit.resets_at.0.as_deref().map_or_else(
        || {
            language
                .text("重置时间未提供", "reset time not reported")
                .to_string()
        },
        |value| match language {
            UiLanguage::ZhCn => format!("重置 {}", format_reset_time(value)),
            UiLanguage::EnUs => format!("resets {}", format_reset_time(value)),
        },
    );
    let policy = limit
        .overridable
        .0
        .map(|overridable| match (language, overridable) {
            (UiLanguage::ZhCn, true) => "耗尽后可继续",
            (UiLanguage::ZhCn, false) => "耗尽后需等待重置",
            (UiLanguage::EnUs, true) => "can continue at limit",
            (UiLanguage::EnUs, false) => "must wait at limit",
        });
    let metric = format!("{label} · {window} · {usage}");
    let reset_and_policy = policy.map_or(reset.clone(), |policy| format!("{reset} · {policy}"));
    if narrow {
        vec![
            detail_line(metric, UsagePluginDetailTone::Metric),
            detail_line(
                format!("  {reset_and_policy}"),
                UsagePluginDetailTone::Supporting,
            ),
        ]
    } else {
        vec![detail_line(
            format!("{metric} · {reset_and_policy}"),
            UsagePluginDetailTone::Metric,
        )]
    }
}

fn extra_usage_detail_lines(
    extra: &ExtraUsageSnapshot,
    language: UiLanguage,
    narrow: bool,
) -> ExtraUsageDetailLines {
    if !extra.is_enabled {
        return ExtraUsageDetailLines {
            core: vec![detail_line(
                language.text("额外用量 · 未启用", "Extra usage · disabled"),
                UsagePluginDetailTone::Supporting,
            )],
            backend_values: Vec::new(),
        };
    }

    let used = extra.used_credits.0.map(format_amount);
    let limit = extra.monthly_limit.0.map(format_amount);
    let used_compact = used
        .as_ref()
        .map_or_else(|| "—".to_string(), |value| value.compact.clone());
    let limit_compact = limit.as_ref().map_or_else(
        || language.text("不限额", "unlimited").to_string(),
        |value| value.compact.clone(),
    );
    let utilization = extra.utilization.0.map_or_else(
        || {
            language
                .text("比例未提供", "percentage not reported")
                .to_string()
        },
        |value| match language {
            UiLanguage::ZhCn => format!("{} 已用", format_percentage(value)),
            UiLanguage::EnUs => format!("{} used", format_percentage(value)),
        },
    );
    let unusually_large = used.as_ref().is_some_and(|value| value.unusually_large)
        || limit.as_ref().is_some_and(|value| value.unusually_large);
    let warning = if unusually_large {
        language.text(" · ⚠ 后端值异常大", " · ⚠ unusually large backend value")
    } else {
        ""
    };
    let tone = if unusually_large {
        UsagePluginDetailTone::Warning
    } else {
        UsagePluginDetailTone::Metric
    };
    let core = if narrow {
        vec![
            detail_line(
                match language {
                    UiLanguage::ZhCn => {
                        format!("额外用量 · 已用 {used_compact} · {utilization}{warning}")
                    }
                    UiLanguage::EnUs => {
                        format!("Extra usage · {used_compact} used · {utilization}{warning}")
                    }
                },
                tone,
            ),
            detail_line(
                match language {
                    UiLanguage::ZhCn => format!("  月限额 {limit_compact}"),
                    UiLanguage::EnUs => format!("  Monthly limit {limit_compact}"),
                },
                UsagePluginDetailTone::Supporting,
            ),
        ]
    } else {
        vec![detail_line(
            match language {
                UiLanguage::ZhCn => format!(
                    "额外用量 · 已用 {used_compact} · 月限额 {limit_compact} · {utilization}{warning}"
                ),
                UiLanguage::EnUs => format!(
                    "Extra usage · {used_compact} used · {limit_compact} monthly limit · {utilization}{warning}"
                ),
            },
            tone,
        )]
    };
    let mut backend_values = Vec::new();
    if used
        .as_ref()
        .is_some_and(|value| value.backend_value.is_some())
        || limit
            .as_ref()
            .is_some_and(|value| value.backend_value.is_some())
    {
        let used = used
            .as_ref()
            .and_then(|value| value.backend_value.as_ref())
            .cloned()
            .unwrap_or(used_compact);
        let limit = limit
            .as_ref()
            .and_then(|value| value.backend_value.as_ref())
            .cloned()
            .unwrap_or(limit_compact);
        backend_values.push(detail_line(
            match language {
                UiLanguage::ZhCn => {
                    format!("额外用量后端原值 · 已用 {used} / 月限额 {limit}")
                }
                UiLanguage::EnUs => {
                    format!("Extra-usage backend values · {used} used / {limit} monthly limit")
                }
            },
            UsagePluginDetailTone::Supporting,
        ));
    }

    ExtraUsageDetailLines {
        core,
        backend_values,
    }
}

fn format_amount(value: f64) -> AmountDisplay {
    let absolute = value.abs();
    let backend_value = format_backend_number(value);
    let (scaled, suffix) = if absolute >= 1_000_000_000_000.0 {
        (value / 1_000_000_000_000.0, "T")
    } else if absolute >= 1_000_000_000.0 {
        (value / 1_000_000_000.0, "B")
    } else if absolute >= 1_000_000.0 {
        (value / 1_000_000.0, "M")
    } else if absolute >= 100_000.0 {
        (value / 1_000.0, "K")
    } else {
        return AmountDisplay {
            compact: backend_value,
            backend_value: None,
            unusually_large: false,
        };
    };
    let compact = if absolute >= 1.0e15 {
        format!("≈{value:.2e}")
    } else {
        format!("≈{}{suffix}", format_scaled_number(scaled))
    };
    AmountDisplay {
        compact,
        backend_value: Some(backend_value),
        unusually_large: absolute >= 1.0e15,
    }
}

fn format_scaled_number(value: f64) -> String {
    let precision = if value.abs() >= 100.0 {
        0
    } else if value.abs() >= 10.0 {
        1
    } else {
        2
    };
    trim_decimal_zeros(format!("{value:.precision$}"))
}

fn format_backend_number(value: f64) -> String {
    let absolute = value.abs();
    if absolute >= 1.0e15 || (absolute > 0.0 && absolute < 0.000_001) {
        return format!("{value:.17e}");
    }
    let plain = trim_decimal_zeros(format!("{value:.6}"));
    let (integer, fraction) = plain
        .split_once('.')
        .map_or((plain.as_str(), None), |(integer, fraction)| {
            (integer, Some(fraction))
        });
    let grouped = group_integer_digits(integer);
    fraction.map_or(grouped.clone(), |fraction| format!("{grouped}.{fraction}"))
}

fn format_u64(value: u64) -> String {
    group_integer_digits(&value.to_string())
}

fn group_integer_digits(value: &str) -> String {
    let (sign, digits) = value
        .strip_prefix('-')
        .map_or(("", value), |digits| ("-", digits));
    let mut grouped = String::with_capacity(value.len() + value.len() / 3);
    grouped.push_str(sign);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped
}

fn trim_decimal_zeros(mut value: String) -> String {
    if value.contains('.') {
        while value.ends_with('0') {
            value.pop();
        }
        if value.ends_with('.') {
            value.pop();
        }
    }
    value
}

fn format_percentage(value: f64) -> String {
    format!("{value:.1}%")
}

fn format_reset_time(value: &str) -> String {
    value
        .strip_suffix('Z')
        .and_then(|value| value.split_once('T'))
        .filter(|(date, time)| date.len() == 10 && !time.is_empty())
        .map_or_else(
            || value.to_string(),
            |(date, time)| format!("{date} {time} UTC"),
        )
}

fn validation_diagnostic_line(
    diagnostic: &PluginValidationDiagnostic,
    error: bool,
    language: UiLanguage,
) -> String {
    let label = match (language, error) {
        (UiLanguage::ZhCn, true) => "错误",
        (UiLanguage::ZhCn, false) => "警告",
        (UiLanguage::EnUs, true) => "Error",
        (UiLanguage::EnUs, false) => "Warning",
    };
    match language {
        UiLanguage::ZhCn => diagnostic.code.0.as_ref().map_or_else(
            || format!("{label}：{}", diagnostic.path),
            |code| format!("{label} [{code}]：{}", diagnostic.path),
        ),
        UiLanguage::EnUs => diagnostic.code.0.as_ref().map_or_else(
            || format!("{label}: {}", diagnostic.path),
            |code| format!("{label} [{code}]: {}", diagnostic.path),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private(effect: &UsagePluginManagementEffect) -> (u64, Value) {
        match effect {
            UsagePluginManagementEffect::Private { token, action, .. } => (*token, action.value()),
            UsagePluginManagementEffect::Close => panic!("expected private effect"),
        }
    }

    fn usage_snapshot(continue_enabled: Value) -> Value {
        json!({
            "kind": "usage_snapshot",
            "utilization": {
                "five_hour": {
                    "utilization": 25.0,
                    "resets_at": "2030-01-01T00:00:00Z",
                    "overridable": true
                },
                "seven_day": {
                    "utilization": null,
                    "resets_at": null,
                    "overridable": null
                },
                "extra_usage": {
                    "is_enabled": true,
                    "monthly_limit": null,
                    "used_credits": 12.5,
                    "utilization": null
                },
                "five_hour_continue_enabled": continue_enabled
            },
            "entitlement_balance": {
                "total_token_quota": 1000.0,
                "total_token_used": 250.0,
                "total_token_remaining": 750.0,
                "total_call_quota": 10.0,
                "total_call_used": 2.0,
                "total_call_remaining": 8.0,
                "active_entitlements": 1
            }
        })
    }

    fn installation(scope: &str) -> Value {
        json!({
            "scope": scope,
            "version": "1.0.0",
            "installed_at": null,
            "last_updated": null
        })
    }

    fn inventory_plugin(id: &str, scopes: &[&str]) -> Value {
        let (name, marketplace) = id.split_once('@').unwrap_or((id, "local"));
        json!({
            "id": id,
            "name": name,
            "marketplace": marketplace,
            "description": null,
            "version": "1.0.0",
            "is_builtin": false,
            "loaded": true,
            "enabled": true,
            "configured_scope": "user",
            "installations": scopes.iter().map(|scope| installation(scope)).collect::<Vec<_>>()
        })
    }

    fn inventory_result(plugins: Vec<Value>) -> Value {
        json!({
            "kind": "plugin_inventory_snapshot",
            "plugins": plugins,
            "load_diagnostics": [],
            "truncated": false
        })
    }

    fn marketplace(name: &str, installed: u64) -> Value {
        json!({
            "name": name,
            "source_kind": "github",
            "last_updated": null,
            "plugin_count": 1,
            "installed_plugin_count": installed,
            "auto_update": false,
            "load_failed": false
        })
    }

    fn marketplace_inventory(names: &[(&str, u64)]) -> Value {
        let empty_reason = if names.is_empty() {
            "no-marketplaces-configured"
        } else {
            "all-plugins-installed"
        };
        marketplace_inventory_with_reason(names, empty_reason)
    }

    fn marketplace_inventory_with_reason(names: &[(&str, u64)], empty_reason: &str) -> Value {
        json!({
            "kind": "plugin_marketplace_inventory_snapshot",
            "marketplaces": names
                .iter()
                .map(|(name, installed)| marketplace(name, *installed))
                .collect::<Vec<_>>(),
            "empty_reason": empty_reason,
            "truncated": false
        })
    }

    fn catalog_plugin(id: &str) -> Value {
        let (name, _) = id.split_once('@').unwrap_or((id, "local"));
        json!({
            "id": id,
            "name": name,
            "display_name": name,
            "description": null,
            "version": null,
            "category": null,
            "tags": [],
            "globally_installed": false,
            "enabled": false,
            "install_count": null,
            "installations": []
        })
    }

    fn catalog_result(marketplace_name: &str, plugins: Vec<Value>) -> Value {
        json!({
            "kind": "plugin_marketplace_catalog_snapshot",
            "marketplace_name": marketplace_name,
            "plugins": plugins,
            "truncated": false
        })
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn all_fourteen_actions_have_exact_closed_values_and_sensitive_debug_is_redacted() {
        let actions = vec![
            UsagePluginRuntimeAction::UsageRead,
            UsagePluginRuntimeAction::UsageSetFiveHourContinue { enabled: true },
            UsagePluginRuntimeAction::PluginInventoryRead,
            UsagePluginRuntimeAction::PluginMarketplaceInventoryRead,
            UsagePluginRuntimeAction::PluginMarketplaceCatalogRead {
                marketplace_name: "official".to_string(),
            },
            UsagePluginRuntimeAction::PluginInstall {
                plugin_id: "demo@official".to_string(),
                scope: PluginInstallScope::User,
            },
            UsagePluginRuntimeAction::PluginUninstall {
                plugin_id: "demo@official".to_string(),
                scope: PluginInstallScope::Project,
                delete_data: false,
            },
            UsagePluginRuntimeAction::PluginSetEnabled {
                plugin_id: "demo@official".to_string(),
                enabled: false,
                scope: None,
            },
            UsagePluginRuntimeAction::PluginUpdate {
                plugin_id: "demo@official".to_string(),
                scope: PluginScope::Managed,
            },
            UsagePluginRuntimeAction::PluginMarketplaceAdd {
                source_input: "secret-source".to_string(),
            },
            UsagePluginRuntimeAction::PluginMarketplaceRemove {
                marketplace_name: "official".to_string(),
            },
            UsagePluginRuntimeAction::PluginMarketplaceUpdate {
                marketplace_name: "official".to_string(),
            },
            UsagePluginRuntimeAction::PluginMarketplaceSetAutoUpdate {
                marketplace_name: "official".to_string(),
                enabled: true,
            },
            UsagePluginRuntimeAction::PluginValidate {
                path: "/sensitive/path".to_string(),
            },
        ];
        let expected = vec![
            json!({"kind":"usage_read"}),
            json!({"kind":"usage_set_five_hour_continue","enabled":true}),
            json!({"kind":"plugin_inventory_read"}),
            json!({"kind":"plugin_marketplace_inventory_read"}),
            json!({"kind":"plugin_marketplace_catalog_read","marketplace_name":"official"}),
            json!({"kind":"plugin_install","plugin_id":"demo@official","scope":"user"}),
            json!({"kind":"plugin_uninstall","plugin_id":"demo@official","scope":"project","delete_data":false}),
            json!({"kind":"plugin_set_enabled","plugin_id":"demo@official","enabled":false,"scope":null}),
            json!({"kind":"plugin_update","plugin_id":"demo@official","scope":"managed"}),
            json!({"kind":"plugin_marketplace_add","source_input":"secret-source"}),
            json!({"kind":"plugin_marketplace_remove","marketplace_name":"official"}),
            json!({"kind":"plugin_marketplace_update","marketplace_name":"official"}),
            json!({"kind":"plugin_marketplace_set_auto_update","marketplace_name":"official","enabled":true}),
            json!({"kind":"plugin_validate","path":"/sensitive/path"}),
        ];
        assert_eq!(actions.len(), 14);
        assert_eq!(
            actions
                .iter()
                .map(UsagePluginRuntimeAction::value)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(!format!("{:?}", actions[9]).contains("secret-source"));
        assert!(!format!("{:?}", actions[13]).contains("/sensitive/path"));
    }

    #[test]
    fn all_fifteen_result_kinds_decode() {
        let values = vec![
            json!({"kind":"usage_snapshot","utilization":null,"entitlement_balance":null}),
            json!({"kind":"usage_five_hour_continue_updated","enabled":true}),
            inventory_result(Vec::new()),
            marketplace_inventory(&[]),
            catalog_result("official", Vec::new()),
            json!({"kind":"plugin_installed","plugin_id":"demo@official","plugin_name":"demo","scope":"user"}),
            json!({"kind":"plugin_uninstalled","plugin_id":"demo@official","plugin_name":"demo","scope":"local","reverse_dependents":[]}),
            json!({"kind":"plugin_enabled_state_updated","plugin_id":"demo@official","plugin_name":"demo","enabled":true,"scope":"project","reverse_dependents":[]}),
            json!({"kind":"plugin_updated","plugin_id":"demo@official","scope":"managed","old_version":null,"new_version":null,"already_up_to_date":true}),
            json!({"kind":"plugin_marketplace_added","marketplace_name":"official","source_kind":"github","already_materialized":false}),
            json!({"kind":"plugin_marketplace_removed","marketplace_name":"official"}),
            json!({"kind":"plugin_marketplace_updated","marketplace_name":"official","updated_plugin_ids":[],"plugin_update_failure_count":0}),
            json!({"kind":"plugin_marketplace_auto_update_updated","marketplace_name":"official","enabled":true}),
            json!({"kind":"plugin_validation_result","success":true,"file_type":"plugin","errors":[],"warnings":[],"related_result_count":0,"truncated":false}),
            json!({"kind":"usage_plugin_error","action_kind":"usage_read","code":"usage_unavailable","message":"unavailable"}),
        ];
        assert_eq!(values.len(), 15);
        for value in values {
            assert!(parse_usage_plugin_runtime_result(value).is_ok());
        }
    }

    #[test]
    fn result_parser_rejects_missing_nullable_unknown_fields_bad_ranges_and_caps() {
        let missing_required_nullable = json!({
            "kind":"usage_snapshot",
            "utilization":{
                "five_hour":{"utilization":1,"resets_at":null},
                "seven_day":null,
                "extra_usage":null,
                "five_hour_continue_enabled":null
            },
            "entitlement_balance":null
        });
        assert!(parse_usage_plugin_runtime_result(missing_required_nullable).is_err());

        let mut unknown = usage_snapshot(Value::Null);
        unknown
            .as_object_mut()
            .expect("object fixture")
            .insert("unexpected".to_string(), Value::Bool(true));
        assert!(parse_usage_plugin_runtime_result(unknown).is_err());

        let mut bad_range = usage_snapshot(Value::Null);
        bad_range["utilization"]["five_hour"]["utilization"] = json!(101.0);
        assert!(parse_usage_plugin_runtime_result(bad_range).is_err());

        let mut negative_call_used = usage_snapshot(Value::Null);
        negative_call_used["entitlement_balance"]["total_call_used"] = json!(-1.0);
        assert!(parse_usage_plugin_runtime_result(negative_call_used).is_err());

        let repeated = vec![inventory_plugin("demo@official", &[]); MAX_PLUGIN_ROWS + 1];
        assert!(parse_usage_plugin_runtime_result(inventory_result(repeated)).is_err());
        assert!(parse_usage_plugin_runtime_result(json!({"kind":"unknown"})).is_err());
    }

    #[test]
    fn usage_is_chinese_by_default_english_is_optional_and_toggle_is_fact_gated() {
        let (mut state, effect) = UsagePluginManagementState::open_usage();
        assert_eq!(state.title(UiLanguage::ZhCn), "用量与额度");
        assert_eq!(state.title(UiLanguage::EnUs), "Usage and limits");
        let (token, action) = private(&effect);
        assert_eq!(action, json!({"kind":"usage_read"}));
        assert!(
            state
                .apply_result(token, usage_snapshot(json!(false)), UiLanguage::ZhCn)
                .is_empty()
        );
        assert!(
            state
                .details(UiLanguage::ZhCn)
                .iter()
                .any(|line| line.contains("当前会话"))
        );
        assert!(
            state
                .details(UiLanguage::EnUs)
                .iter()
                .any(|line| line.contains("Current session"))
        );
        assert_eq!(state.rows(UiLanguage::ZhCn).len(), 2);

        let (mut unknown_state, unknown_effect) = UsagePluginManagementState::open_usage();
        let (unknown_token, _) = private(&unknown_effect);
        unknown_state.apply_result(unknown_token, usage_snapshot(Value::Null), UiLanguage::ZhCn);
        assert_eq!(unknown_state.rows(UiLanguage::ZhCn).len(), 1);
    }

    #[test]
    fn usage_accepts_backend_negative_call_sentinels_and_labels_calls_as_unreported() {
        let (mut state, effect) = UsagePluginManagementState::open_usage();
        let (token, _) = private(&effect);
        let mut snapshot = usage_snapshot(Value::Null);
        snapshot["entitlement_balance"]["total_call_quota"] = json!(-1.0);
        snapshot["entitlement_balance"]["total_call_remaining"] = json!(-1.0);

        assert!(
            state
                .apply_result(token, snapshot, UiLanguage::ZhCn)
                .is_empty()
        );
        let details = state.details(UiLanguage::ZhCn);
        assert!(details.iter().any(|line| line.starts_with("Token ·")));
        assert!(
            details
                .iter()
                .any(|line| line == "调用 · 后端未提供独立调用额度")
        );
        assert!(state.notice().is_none());
    }

    #[test]
    fn usage_information_architecture_formats_units_percentages_windows_and_backend_values() {
        let (mut state, effect) = UsagePluginManagementState::open_usage();
        let (token, _) = private(&effect);
        let mut snapshot = usage_snapshot(json!(true));
        snapshot["entitlement_balance"]["total_token_quota"] = json!(987_654_321_000.0);
        snapshot["entitlement_balance"]["total_token_used"] = json!(123_456_789_000.0);
        snapshot["entitlement_balance"]["total_token_remaining"] = json!(864_197_532_000.0);
        snapshot["entitlement_balance"]["active_entitlements"] = json!(12_345_u64);
        snapshot["utilization"]["seven_day"]["utilization"] = json!(62.5);
        snapshot["utilization"]["seven_day"]["resets_at"] = json!("2030-01-07T08:30:00Z");

        state.apply_result(token, snapshot, UiLanguage::ZhCn);
        let chinese = state.detail_lines(UiLanguage::ZhCn, 90);
        let chinese_text = chinese
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(chinese_text.contains("额度余额 · 有效额度 12,345 项"));
        assert!(chinese_text.contains("Token · 已用 ≈123B / 总额 ≈988B"));
        assert!(chinese_text.contains("12.5% 已用"));
        assert!(chinese_text.contains("Token 后端原值"));
        assert!(chinese_text.contains("123,456,789,000"));
        assert!(chinese_text.contains("当前会话 · 5 小时窗口"));
        assert!(chinese_text.contains("七天用量 · 滚动窗口"));
        assert!(chinese_text.contains("2030-01-07 08:30:00 UTC"));
        assert!(chinese_text.contains("额外用量"));
        assert!(chinese.len() <= 12);

        let english = state.detail_lines(UiLanguage::EnUs, 52);
        let english_text = english
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(english_text.contains("Entitlement balance"));
        assert!(english_text.contains("Calls"));
        assert!(english_text.contains("Current session"));
        assert!(english_text.contains("Seven-day usage"));
        assert!(english_text.contains("resets 2030-01-07 08:30:00 UTC"));
        assert!(english.len() <= 12);
    }

    #[test]
    fn usage_does_not_invent_a_percentage_for_inconsistent_backend_balances() {
        let (mut state, effect) = UsagePluginManagementState::open_usage();
        let (token, _) = private(&effect);
        let mut snapshot = usage_snapshot(Value::Null);
        snapshot["entitlement_balance"]["total_token_quota"] = json!(100.0);
        snapshot["entitlement_balance"]["total_token_used"] = json!(90.0);
        snapshot["entitlement_balance"]["total_token_remaining"] = json!(90.0);

        state.apply_result(token, snapshot, UiLanguage::ZhCn);
        let token_line = state
            .detail_lines(UiLanguage::ZhCn, 90)
            .into_iter()
            .find(|line| line.text.starts_with("Token ·"))
            .expect("token line");
        assert_eq!(token_line.tone, UsagePluginDetailTone::Warning);
        assert!(token_line.text.contains("后端数值不一致"));
        assert!(!token_line.text.contains('%'));
        assert!(token_line.text.contains("90"));
        assert!(token_line.text.contains("100"));
    }

    #[test]
    fn usage_number_formatting_is_bounded_and_transparent_for_extreme_finite_values() {
        let extreme = format_amount(f64::MAX);
        assert!(extreme.compact.starts_with("≈1.80e308"), "{extreme:?}");
        assert!(extreme.unusually_large);
        assert!(
            extreme
                .backend_value
                .as_deref()
                .is_some_and(|value| value.contains("e308"))
        );
        assert!(extreme.compact.len() < 32);
        assert!(format_backend_number(12_345.25).contains("12,345.25"));
        assert_eq!(
            format_reset_time("2030-01-01T00:00:00Z"),
            "2030-01-01 00:00:00 UTC"
        );
    }

    #[test]
    fn usage_flags_huge_backend_sentinels_instead_of_presenting_them_as_real_quota() {
        let (mut state, effect) = UsagePluginManagementState::open_usage();
        let (token, _) = private(&effect);
        let mut snapshot = usage_snapshot(Value::Null);
        snapshot["entitlement_balance"]["total_token_quota"] = json!(1.0e18);
        snapshot["entitlement_balance"]["total_token_used"] = json!(5.0e17);
        snapshot["entitlement_balance"]["total_token_remaining"] = json!(5.0e17);

        state.apply_result(token, snapshot, UiLanguage::ZhCn);
        let details = state.detail_lines(UiLanguage::ZhCn, 90);
        let token_line = details
            .iter()
            .find(|line| line.text.starts_with("Token ·"))
            .expect("token line");
        assert_eq!(token_line.tone, UsagePluginDetailTone::Warning);
        assert!(token_line.text.contains("后端值异常大"));
        assert!(!token_line.text.contains('%'));
        assert!(
            details
                .iter()
                .any(|line| line.text.contains("1.00000000000000000e18")),
            "decoded backend fact must remain inspectable: {details:?}"
        );
    }

    #[test]
    fn escape_only_closes_modal_even_while_request_is_pending() {
        let (mut state, _) = UsagePluginManagementState::open_usage();
        assert!(state.is_busy());
        assert_eq!(
            state.key(press(KeyCode::Esc), UiLanguage::ZhCn),
            vec![UsagePluginManagementEffect::Close]
        );
        assert!(state.is_busy());
    }

    #[test]
    fn stale_results_and_failures_are_ignored_and_matching_send_failure_is_retryable() {
        let (mut state, effect) = UsagePluginManagementState::open_usage();
        let (token, _) = private(&effect);
        assert!(
            state
                .apply_result(token + 10, usage_snapshot(json!(true)), UiLanguage::ZhCn)
                .is_empty()
        );
        state.apply_send_failure(token + 10, UiLanguage::ZhCn, "stale");
        assert!(state.is_busy());
        state.apply_send_failure(token, UiLanguage::ZhCn, "closed");
        assert!(!state.is_busy());
        assert!(
            state
                .notice()
                .is_some_and(|notice| notice.contains("可重试"))
        );

        let retry = state.key(press(KeyCode::Char('r')), UiLanguage::ZhCn);
        let (retry_token, _) = private(&retry[0]);
        assert_ne!(retry_token, token);
        state.apply_result(token, usage_snapshot(json!(true)), UiLanguage::ZhCn);
        assert!(state.is_busy());
        state.apply_result(retry_token, usage_snapshot(json!(true)), UiLanguage::ZhCn);
        assert!(!state.is_busy());
    }

    #[test]
    fn plugin_subcommands_route_to_the_existing_closed_actions() {
        let (_, default_effects) = UsagePluginManagementState::open_plugin("");
        assert_eq!(
            private(&default_effects[0]).1,
            json!({"kind":"plugin_marketplace_inventory_read"})
        );

        let (_, qualified) = UsagePluginManagementState::open_plugin("install demo@official");
        assert_eq!(
            private(&qualified[0]).1,
            json!({"kind":"plugin_marketplace_catalog_read","marketplace_name":"official"})
        );

        let (_, manage) = UsagePluginManagementState::open_plugin("manage demo");
        assert_eq!(
            private(&manage[0]).1,
            json!({"kind":"plugin_inventory_read"})
        );

        let (_, add) =
            UsagePluginManagementState::open_plugin("install https://example.invalid/plugins.git");
        assert_eq!(
            private(&add[0]).1,
            json!({"kind":"plugin_marketplace_add","source_input":"https://example.invalid/plugins.git"})
        );

        let (_, validate) = UsagePluginManagementState::open_plugin("validate ./plugin.json");
        assert_eq!(
            private(&validate[0]).1,
            json!({"kind":"plugin_validate","path":"./plugin.json"})
        );

        let (help, help_effects) = UsagePluginManagementState::open_plugin("help");
        assert!(help_effects.is_empty());
        assert_eq!(help.title(UiLanguage::ZhCn), "插件命令帮助");
    }

    #[test]
    fn discover_reads_catalogs_sequentially_and_never_guesses_an_ambiguous_plugin() {
        let (mut state, effects) = UsagePluginManagementState::open_plugin("demo");
        let (inventory_token, _) = private(&effects[0]);
        let next = state.apply_result(
            inventory_token,
            marketplace_inventory(&[("one", 0), ("two", 0)]),
            UiLanguage::ZhCn,
        );
        let (one_token, one_action) = private(&next[0]);
        assert_eq!(one_action["marketplace_name"], "one");
        let next = state.apply_result(
            one_token,
            catalog_result("one", vec![catalog_plugin("demo@one")]),
            UiLanguage::ZhCn,
        );
        let (two_token, two_action) = private(&next[0]);
        assert_eq!(two_action["marketplace_name"], "two");
        assert!(
            state
                .apply_result(
                    two_token,
                    catalog_result("two", vec![catalog_plugin("demo@two")]),
                    UiLanguage::ZhCn,
                )
                .is_empty()
        );
        assert!(
            state
                .notice()
                .is_some_and(|notice| notice.contains("多个市场"))
        );
        assert!(!state.is_busy());
    }

    #[test]
    fn one_failed_catalog_does_not_abort_remaining_discovery_catalogs() {
        let (mut state, effects) = UsagePluginManagementState::open_plugin("");
        let (inventory_token, _) = private(&effects[0]);
        let first = state.apply_result(
            inventory_token,
            marketplace_inventory(&[("one", 0), ("two", 0)]),
            UiLanguage::ZhCn,
        );
        let (one_token, _) = private(&first[0]);
        let second = state.apply_result(
            one_token,
            json!({
                "kind":"usage_plugin_error",
                "action_kind":"plugin_marketplace_catalog_read",
                "code":"marketplace_catalog_unavailable",
                "message":"catalog unavailable"
            }),
            UiLanguage::ZhCn,
        );
        assert_eq!(private(&second[0]).1["marketplace_name"], "two");
    }

    #[test]
    fn discover_renders_the_authority_empty_reason_after_loading_finishes() {
        let (mut no_marketplaces, effects) = UsagePluginManagementState::open_plugin("");
        let (token, _) = private(&effects[0]);
        assert!(
            no_marketplaces
                .apply_result(
                    token,
                    marketplace_inventory_with_reason(&[], "no-marketplaces-configured"),
                    UiLanguage::ZhCn,
                )
                .is_empty()
        );
        assert!(
            no_marketplaces
                .details(UiLanguage::ZhCn)
                .iter()
                .any(|line| line == "暂无可用插件。")
        );

        let (mut all_installed, effects) = UsagePluginManagementState::open_plugin("");
        let (inventory_token, _) = private(&effects[0]);
        let catalog_effects = all_installed.apply_result(
            inventory_token,
            marketplace_inventory_with_reason(&[("official", 1)], "all-plugins-installed"),
            UiLanguage::EnUs,
        );
        let (catalog_token, _) = private(&catalog_effects[0]);
        let mut installed_plugin = catalog_plugin("demo@official");
        installed_plugin["globally_installed"] = json!(true);
        assert!(
            all_installed
                .apply_result(
                    catalog_token,
                    catalog_result("official", vec![installed_plugin]),
                    UiLanguage::EnUs,
                )
                .is_empty()
        );
        assert!(
            all_installed
                .details(UiLanguage::EnUs)
                .iter()
                .any(|line| line == "All available plugins are already installed.")
        );
    }

    #[test]
    fn direct_enable_keeps_scope_null_for_authority_resolution() {
        let (mut state, effects) = UsagePluginManagementState::open_plugin("enable demo");
        let (token, _) = private(&effects[0]);
        let effects = state.apply_result(
            token,
            inventory_result(vec![inventory_plugin("demo@official", &["project"])]),
            UiLanguage::ZhCn,
        );
        let (_, action) = private(&effects[0]);
        assert_eq!(action["kind"], "plugin_set_enabled");
        assert_eq!(action["plugin_id"], "demo@official");
        assert!(action["scope"].is_null());
    }

    #[test]
    fn uninstall_with_multiple_scopes_requires_selection_and_defaults_to_preserve_data() {
        let (mut state, effects) = UsagePluginManagementState::open_plugin("uninstall demo");
        let (token, _) = private(&effects[0]);
        assert!(
            state
                .apply_result(
                    token,
                    inventory_result(vec![inventory_plugin(
                        "demo@official",
                        &["user", "project"]
                    )]),
                    UiLanguage::ZhCn,
                )
                .is_empty()
        );
        assert_eq!(state.title(UiLanguage::ZhCn), "选择卸载范围");
        assert_eq!(state.rows(UiLanguage::ZhCn).len(), 3);

        assert!(
            state
                .key(press(KeyCode::Enter), UiLanguage::ZhCn)
                .is_empty()
        );
        assert_eq!(state.title(UiLanguage::ZhCn), "确认卸载插件");
        let effects = state.key(press(KeyCode::Enter), UiLanguage::ZhCn);
        let (_, action) = private(&effects[0]);
        assert_eq!(action["kind"], "plugin_uninstall");
        assert_eq!(action["scope"], "user");
        assert_eq!(action["delete_data"], false);
    }

    #[test]
    fn marketplace_with_installed_plugins_cannot_enter_removal_confirmation() {
        let (mut state, effects) =
            UsagePluginManagementState::open_plugin("marketplace remove official");
        let (token, _) = private(&effects[0]);
        assert!(
            state
                .apply_result(
                    token,
                    marketplace_inventory(&[("official", 2)]),
                    UiLanguage::ZhCn,
                )
                .is_empty()
        );
        assert_eq!(state.title(UiLanguage::ZhCn), "管理插件市场");
        assert!(
            state
                .notice()
                .is_some_and(|notice| notice.contains("先卸载"))
        );
        assert!(state.rows(UiLanguage::ZhCn)[3].disabled);
    }

    #[test]
    fn validation_failure_stays_in_modal_and_never_emits_close() {
        let (mut state, effects) = UsagePluginManagementState::open_plugin("validate ./bad");
        let (token, _) = private(&effects[0]);
        let effects = state.apply_result(
            token,
            json!({
                "kind":"plugin_validation_result",
                "success":false,
                "file_type":"plugin",
                "errors":[{"path":"plugin.json","code":"invalid"}],
                "warnings":[],
                "related_result_count":1,
                "truncated":false
            }),
            UiLanguage::ZhCn,
        );
        assert!(effects.is_empty());
        assert_eq!(state.title(UiLanguage::ZhCn), "插件校验结果");
        assert!(state.details(UiLanguage::ZhCn)[0].contains("校验失败"));
        assert!(!state.is_busy());
    }

    #[test]
    fn business_error_is_nonfatal_and_retryable() {
        let (mut state, effect) = UsagePluginManagementState::open_usage();
        let (token, _) = private(&effect);
        let effects = state.apply_result(
            token,
            json!({
                "kind":"usage_plugin_error",
                "action_kind":"usage_read",
                "code":"usage_unavailable",
                "message":"temporarily unavailable"
            }),
            UiLanguage::ZhCn,
        );
        assert!(effects.is_empty());
        assert!(!state.is_busy());
        assert!(
            state
                .notice()
                .is_some_and(|notice| notice.contains("用量数据不可用"))
        );
        assert_eq!(
            private(&state.key(press(KeyCode::Char('r')), UiLanguage::ZhCn)[0]).1,
            json!({"kind":"usage_read"})
        );
    }

    #[test]
    fn source_has_no_process_exit_forbidden_host_crate_or_legacy_brand_identifier() {
        let source = include_str!("usage_plugin_management.rs").to_lowercase();
        let process_exit = ["process", "::", "exit"].concat();
        let forbidden_host_crate = ["acosmi", "_app", "_server"].concat();
        let legacy_brand = ["gro", "k_render"].concat();
        assert!(!source.contains(&process_exit));
        assert!(!source.contains(&forbidden_host_crate));
        assert!(!source.contains(&legacy_brand));
    }
}
