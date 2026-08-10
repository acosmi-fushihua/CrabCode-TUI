//! Renderer-owned model management state machine.
//!
//! Backend facts and mutations stay in the direct TypeScript runtime.  This
//! module owns only the native modal lifecycle and builds values accepted by
//! the closed `crabcode_tui_runtime_action` lane.

use std::fmt;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tui_app::{PrivateRuntimePurpose, UiLanguage};

// Exact fixed-product values.  The single source of truth is
// `src/utils/model/customModelDefaults.ts`:
// `CUSTOM_MODEL_DEFAULT_CONTEXT_WINDOW`,
// `CUSTOM_MODEL_DEFAULT_MAX_OUTPUT_TOKENS`,
// `CUSTOM_MODEL_DEFAULT_SUPPORTS_THINKING`, and
// `CUSTOM_MODEL_DEFAULT_SUPPORTS_VISION`.  The pinned historical
// `src/commands/model/CustomModelManagement.tsx` consumes those same symbols.
// Do not independently tune these renderer constants.
const CUSTOM_DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
const CUSTOM_DEFAULT_MAX_OUTPUT_TOKENS: u64 = 64_000;
const ACCOUNT_LOGIN_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_INPUT_BYTES: usize = 8 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SecretText {
    value: String,
    cursor: usize,
}

impl fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretText")
            .field("value", &"[REDACTED]")
            .field("bytes", &self.value.len())
            .finish()
    }
}

impl SecretText {
    fn empty() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
        }
    }

    fn expose_for_submission(&self) -> &str {
        &self.value
    }

    pub(crate) fn masked(&self) -> String {
        "•".repeat(self.value.chars().count())
    }

    fn insert(&mut self, text: &str) {
        insert_bounded(&mut self.value, &mut self.cursor, text);
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        edit_text(&mut self.value, &mut self.cursor, key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlainText {
    value: String,
    cursor: usize,
}

impl PlainText {
    fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.len();
        Self { value, cursor }
    }

    fn insert(&mut self, text: &str) {
        insert_bounded(&mut self.value, &mut self.cursor, text);
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        edit_text(&mut self.value, &mut self.cursor, key)
    }
}

fn insert_bounded(value: &mut String, cursor: &mut usize, text: &str) {
    let safe = text
        .chars()
        .filter(|character| !character.is_control() && *character != '\u{7f}')
        .collect::<String>();
    let remaining = MAX_INPUT_BYTES.saturating_sub(value.len());
    if remaining == 0 {
        return;
    }
    let mut end = safe.len().min(remaining);
    while end > 0 && !safe.is_char_boundary(end) {
        end -= 1;
    }
    value.insert_str(*cursor, &safe[..end]);
    *cursor += end;
}

fn edit_text(value: &mut String, cursor: &mut usize, key: KeyEvent) -> bool {
    match key {
        KeyEvent {
            code: KeyCode::Char(character),
            modifiers,
            ..
        } if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && !character.is_control() =>
        {
            let mut buffer = [0_u8; 4];
            insert_bounded(value, cursor, character.encode_utf8(&mut buffer));
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CustomModelView {
    pub(crate) id: String,
    pub(crate) brand: String,
    pub(crate) protocol: String,
    pub(crate) base_url: String,
    pub(crate) model_id: String,
    pub(crate) display_name: Option<String>,
    pub(crate) context_window: u64,
    pub(crate) max_output_tokens: u64,
    pub(crate) supports_thinking: Option<bool>,
    pub(crate) supports_tools: Option<bool>,
    pub(crate) supports_json_mode: Option<bool>,
    pub(crate) supports_vision: Option<bool>,
    pub(crate) enabled: bool,
    pub(crate) is_default: Option<bool>,
    pub(crate) has_stored_credential: bool,
    pub(crate) model_reference: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LocalCatalogEntry {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) description: Option<String>,
    pub(crate) runtime: String,
    pub(crate) protocol: String,
    pub(crate) format: String,
    pub(crate) source: String,
    pub(crate) license: Option<String>,
    pub(crate) size_bytes: Option<f64>,
    pub(crate) sha256: Option<String>,
    pub(crate) installed: bool,
    pub(crate) status: String,
    pub(crate) model_path: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) model_reference: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LocalRuntimeSupport {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) supported: bool,
    pub(crate) acceleration: Option<String>,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LocalProfile {
    pub(crate) platform: String,
    pub(crate) arch: String,
    pub(crate) memory_bytes: Option<f64>,
    pub(crate) recommended_runtime: Option<String>,
    pub(crate) supported_runtimes: Vec<LocalRuntimeSupport>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LocalServerStatus {
    pub(crate) state: String,
    pub(crate) reason: Option<String>,
    pub(crate) host: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) url: Option<String>,
    pub(crate) pid: Option<u64>,
    pub(crate) model_id: Option<String>,
    pub(crate) model_path: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) stderr_tail: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalCatalog {
    data: Vec<LocalCatalogEntry>,
    source: String,
    manifest_status: String,
    manifest_version: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalServerResult {
    status: LocalServerStatus,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalSnapshotResult {
    kind: String,
    catalog: LocalCatalog,
    profile: LocalProfile,
    server: LocalServerResult,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AccountEligibility {
    pub(crate) state: String,
    pub(crate) country_code: Option<String>,
    pub(crate) policy_version: Option<String>,
    pub(crate) checked_at: Option<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) reason_code: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AccountRuntime {
    pub(crate) state: String,
    pub(crate) component_version: Option<String>,
    pub(crate) protocol_version: Option<u64>,
    pub(crate) last_error_code: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AccountConnector {
    pub(crate) connector_id: String,
    pub(crate) display_name: String,
    pub(crate) auth_mode: String,
    pub(crate) enabled: bool,
    pub(crate) disabled_reason_code: Option<String>,
    pub(crate) terms_status: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AccountView {
    pub(crate) account_id: String,
    pub(crate) connector_id: String,
    pub(crate) display_label: String,
    pub(crate) status: String,
    pub(crate) connected_at: String,
    pub(crate) last_used_at: Option<String>,
    pub(crate) cooldown_until: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AccountRoute {
    pub(crate) route_id: String,
    pub(crate) account_id: String,
    pub(crate) connector_id: String,
    pub(crate) model_id: String,
    pub(crate) display_name: Option<String>,
    pub(crate) connector_label: String,
    pub(crate) account_label: String,
    pub(crate) chat_runtime_supported: Option<bool>,
    pub(crate) supports_tools: Option<bool>,
    pub(crate) supports_thinking: Option<bool>,
    pub(crate) supports_adaptive_thinking: Option<bool>,
    pub(crate) supports_effort: Option<bool>,
    pub(crate) supports_max_effort: Option<bool>,
    pub(crate) supports_vision: Option<bool>,
    pub(crate) supports_json_mode: Option<bool>,
    pub(crate) supported_thinking_modes: Vec<String>,
    pub(crate) default_thinking_mode: Option<String>,
    pub(crate) context_window: Option<u64>,
    pub(crate) max_output_tokens: Option<u64>,
    pub(crate) model_reference: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UsageWindow {
    pub(crate) label: String,
    pub(crate) limit: Option<f64>,
    pub(crate) used: Option<f64>,
    pub(crate) remaining_percent: Option<f64>,
    pub(crate) resets_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AccountUsage {
    pub(crate) route_id: String,
    pub(crate) account_id: String,
    pub(crate) state: String,
    pub(crate) remaining_percent: Option<f64>,
    pub(crate) limiting_window_label: Option<String>,
    pub(crate) resets_at: Option<String>,
    pub(crate) windows: Vec<UsageWindow>,
    pub(crate) observed_at: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountSnapshot {
    eligibility: AccountEligibility,
    runtime: Option<AccountRuntime>,
    connectors: Vec<AccountConnector>,
    accounts: Vec<AccountView>,
    routes: Vec<AccountRoute>,
    usage: Vec<AccountUsage>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountSnapshotResult {
    kind: String,
    snapshot: AccountSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CustomFormStep {
    Protocol,
    BaseUrl,
    ModelId,
    ApiKey,
    ContextWindow,
    MaxOutputTokens,
    SupportsThinking,
    SupportsVision,
    Enabled,
    Submit,
}

impl CustomFormStep {
    fn next(self) -> Option<Self> {
        use CustomFormStep::*;
        Some(match self {
            Protocol => BaseUrl,
            BaseUrl => ModelId,
            ModelId => ApiKey,
            ApiKey => ContextWindow,
            ContextWindow => MaxOutputTokens,
            MaxOutputTokens => SupportsThinking,
            SupportsThinking => SupportsVision,
            SupportsVision => Enabled,
            Enabled => Submit,
            Submit => return None,
        })
    }

    fn previous(self) -> Option<Self> {
        use CustomFormStep::*;
        Some(match self {
            Protocol => return None,
            BaseUrl => Protocol,
            ModelId => BaseUrl,
            ApiKey => ModelId,
            ContextWindow => ApiKey,
            MaxOutputTokens => ContextWindow,
            SupportsThinking => MaxOutputTokens,
            SupportsVision => SupportsThinking,
            Enabled => SupportsVision,
            Submit => Enabled,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CustomFormMode {
    Add,
    Edit { id: String },
}

#[derive(Clone, PartialEq, Eq)]
struct CustomDraft {
    brand: String,
    protocol: String,
    base_url: PlainText,
    model_id: PlainText,
    api_key: SecretText,
    context_window: PlainText,
    max_output_tokens: PlainText,
    supports_thinking: bool,
    supports_tools: Option<bool>,
    supports_json_mode: Option<bool>,
    supports_vision: bool,
    enabled: bool,
    display_name: Option<String>,
    is_default: Option<bool>,
}

impl fmt::Debug for CustomDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CustomDraft")
            .field("brand", &self.brand)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("api_key", &"[REDACTED]")
            .field("context_window", &self.context_window)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("supports_thinking", &self.supports_thinking)
            .field("supports_tools", &self.supports_tools)
            .field("supports_json_mode", &self.supports_json_mode)
            .field("supports_vision", &self.supports_vision)
            .field("enabled", &self.enabled)
            .field("display_name", &self.display_name)
            .field("is_default", &self.is_default)
            .finish()
    }
}

impl CustomDraft {
    fn empty() -> Self {
        Self {
            brand: "custom".to_string(),
            protocol: "anthropic-compatible".to_string(),
            base_url: PlainText::new(""),
            model_id: PlainText::new(""),
            api_key: SecretText::empty(),
            context_window: PlainText::new(CUSTOM_DEFAULT_CONTEXT_WINDOW.to_string()),
            max_output_tokens: PlainText::new(CUSTOM_DEFAULT_MAX_OUTPUT_TOKENS.to_string()),
            supports_thinking: true,
            // The fixed add form did not declare either capability.  Omit
            // both instead of inventing a renderer default.
            supports_tools: None,
            supports_json_mode: None,
            supports_vision: false,
            enabled: true,
            display_name: None,
            is_default: None,
        }
    }

    fn from_entry(entry: &CustomModelView) -> Self {
        Self {
            brand: entry.brand.clone(),
            protocol: entry.protocol.clone(),
            base_url: PlainText::new(entry.base_url.clone()),
            model_id: PlainText::new(entry.model_id.clone()),
            api_key: SecretText::empty(),
            context_window: PlainText::new(entry.context_window.to_string()),
            max_output_tokens: PlainText::new(entry.max_output_tokens.to_string()),
            supports_thinking: entry.supports_thinking.unwrap_or(true),
            supports_tools: entry.supports_tools,
            supports_json_mode: entry.supports_json_mode,
            supports_vision: entry.supports_vision.unwrap_or(false),
            enabled: entry.enabled,
            display_name: entry.display_name.clone(),
            is_default: entry.is_default,
        }
    }

    fn action_input(&self, include_empty_key: bool) -> Result<Value, &'static str> {
        let base_url = self.base_url.value.trim();
        let model_id = self.model_id.value.trim();
        if base_url.is_empty() {
            return Err("base-url-empty");
        }
        if model_id.is_empty() {
            return Err("model-id-empty");
        }
        let context_window = self
            .context_window
            .value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or("context-window-invalid")?;
        let max_output_tokens = self
            .max_output_tokens
            .value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or("max-output-tokens-invalid")?;
        let mut input = serde_json::Map::new();
        input.insert("brand".to_string(), json!(self.brand));
        input.insert("protocol".to_string(), json!(self.protocol));
        input.insert("baseUrl".to_string(), json!(base_url));
        input.insert("modelId".to_string(), json!(model_id));
        if include_empty_key || !self.api_key.expose_for_submission().is_empty() {
            input.insert(
                "apiKey".to_string(),
                json!(self.api_key.expose_for_submission()),
            );
        }
        if let Some(display_name) = self.display_name.as_ref() {
            input.insert("displayName".to_string(), json!(display_name));
        }
        input.insert("contextWindow".to_string(), json!(context_window));
        input.insert("maxOutputTokens".to_string(), json!(max_output_tokens));
        input.insert(
            "supportsThinking".to_string(),
            json!(self.supports_thinking),
        );
        if let Some(supports_tools) = self.supports_tools {
            input.insert("supportsTools".to_string(), json!(supports_tools));
        }
        if let Some(supports_json_mode) = self.supports_json_mode {
            input.insert("supportsJsonMode".to_string(), json!(supports_json_mode));
        }
        input.insert("supportsVision".to_string(), json!(self.supports_vision));
        if let Some(is_default) = self.is_default {
            input.insert("isDefault".to_string(), json!(is_default));
        }
        input.insert("enabled".to_string(), json!(self.enabled));
        Ok(Value::Object(input))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FormInput {
    Custom {
        mode: CustomFormMode,
        step: CustomFormStep,
        draft: CustomDraft,
    },
    ByoPath {
        path: PlainText,
    },
    ByoName {
        path: String,
        name: PlainText,
    },
    ServerPort {
        entry_id: String,
        path: Option<String>,
        runtime: String,
        port: PlainText,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum View {
    Home,
    CustomList,
    CustomEntry(String),
    CustomDelete(String),
    Form(FormInput),
    LocalList,
    LocalEntry(String),
    LocalRemove(String),
    AccountMain,
    AccountConnect,
    AccountRemove(String),
    AccountOauth {
        session_id: String,
        authorization_url: Option<String>,
        verification_url: Option<String>,
        user_code: Option<String>,
        expires_at: Option<String>,
        next_poll_at: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingOperation {
    CustomLoad,
    CustomMutation,
    CustomTest,
    LocalSnapshot,
    LocalDownloadStart,
    LocalDownloadProgress,
    LocalDownloadCancel,
    LocalInstallRemove,
    LocalServer,
    LocalByoAdd,
    LocalByoRemove,
    AccountSnapshot,
    AccountConsent,
    AccountRuntimeEnsure,
    AccountRuntimeStop,
    AccountLoginStart,
    AccountLoginPoll,
    AccountLoginCancel,
    AccountRemove,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelManagementState {
    view: View,
    selected: usize,
    custom_entries: Vec<CustomModelView>,
    local_entries: Vec<LocalCatalogEntry>,
    local_profile: Option<LocalProfile>,
    local_server: Option<LocalServerStatus>,
    active_download_id: Option<String>,
    active_download_model_id: Option<String>,
    active_download_state: Option<String>,
    active_download_percentage: Option<f64>,
    account_snapshot: Option<AccountSnapshot>,
    pending: Option<PendingOperation>,
    notice: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ModelManagementEffect {
    Private {
        action: Value,
        purpose: PrivateRuntimePurpose,
    },
    SetModel(String),
    OpenUrl(String),
    Close,
}

impl fmt::Debug for ModelManagementEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Private { action, purpose } => formatter
                .debug_struct("Private")
                .field("action_kind", &action.get("kind").and_then(Value::as_str))
                .field("action", &"<redacted private runtime payload>")
                .field("purpose", purpose)
                .finish(),
            Self::SetModel(model) => formatter.debug_tuple("SetModel").field(model).finish(),
            Self::OpenUrl(url) => formatter.debug_tuple("OpenUrl").field(url).finish(),
            Self::Close => formatter.write_str("Close"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelManagementRow {
    pub(crate) label: String,
    pub(crate) detail: Option<String>,
    pub(crate) disabled: bool,
}

impl ModelManagementState {
    pub(crate) fn open() -> (Self, ModelManagementEffect) {
        (
            Self {
                view: View::Home,
                selected: 0,
                custom_entries: Vec::new(),
                local_entries: Vec::new(),
                local_profile: None,
                local_server: None,
                active_download_id: None,
                active_download_model_id: None,
                active_download_state: None,
                active_download_percentage: None,
                account_snapshot: None,
                pending: Some(PendingOperation::CustomLoad),
                notice: None,
            },
            ModelManagementEffect::Private {
                action: json!({"kind":"model.custom.list"}),
                purpose: PrivateRuntimePurpose::ModelCustomList,
            },
        )
    }

    pub(crate) fn open_local() -> (Self, ModelManagementEffect) {
        let (mut state, _) = Self::open();
        state.view = View::LocalList;
        state.pending = Some(PendingOperation::LocalSnapshot);
        (
            state,
            ModelManagementEffect::Private {
                action: json!({"kind":"model.local.snapshot"}),
                purpose: PrivateRuntimePurpose::ModelLocalSnapshot,
            },
        )
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

    pub(crate) fn title(&self, language: UiLanguage) -> String {
        let (zh, en) = match self.view {
            View::Home => ("模型管理", "Model management"),
            View::CustomList => ("自定义模型", "Custom models"),
            View::CustomEntry(_) => ("自定义模型操作", "Custom model actions"),
            View::CustomDelete(_) => ("确认删除自定义模型", "Confirm custom model deletion"),
            View::Form(FormInput::Custom {
                mode: CustomFormMode::Add,
                ..
            }) => ("添加自定义模型", "Add custom model"),
            View::Form(FormInput::Custom {
                mode: CustomFormMode::Edit { .. },
                ..
            }) => ("编辑自定义模型", "Edit custom model"),
            View::Form(FormInput::ByoPath { .. } | FormInput::ByoName { .. }) => {
                ("添加本地 GGUF 模型", "Add local GGUF model")
            }
            View::Form(FormInput::ServerPort { .. }) => {
                ("启动本地推理服务", "Start local inference server")
            }
            View::LocalList => ("本地模型", "Local models"),
            View::LocalEntry(_) => ("本地模型操作", "Local model actions"),
            View::LocalRemove(_) => ("确认移除本地模型", "Confirm local model removal"),
            View::AccountMain => ("本地账户模型", "Local account models"),
            View::AccountConnect => ("连接本地账户", "Connect local account"),
            View::AccountRemove(_) => ("确认移除账户", "Confirm account removal"),
            View::AccountOauth { .. } => ("等待账户授权", "Waiting for account authorization"),
        };
        language.text(zh, en).to_string()
    }

    pub(crate) fn footer(&self, language: UiLanguage) -> &'static str {
        if self.input(language).is_some() {
            language.text(
                "Enter 下一步 · Esc 上一步/取消 · API 密钥仅以掩码显示",
                "Enter next · Esc back/cancel · API keys are always masked",
            )
        } else if self.pending.is_some() {
            language.text(
                "正在等待直连运行环境完成操作；会话保持打开",
                "Waiting for the direct runtime; the session remains open",
            )
        } else {
            language.text(
                "↑/↓ 选择 · Enter 确认 · Esc 返回 · Ctrl-Q 退出程序",
                "Up/Down select · Enter confirm · Esc back · Ctrl-Q quits",
            )
        }
    }

    pub(crate) fn input(&self, language: UiLanguage) -> Option<(String, String, bool)> {
        match &self.view {
            View::Form(FormInput::Custom { step, draft, .. }) => match step {
                CustomFormStep::BaseUrl => Some((
                    language
                        .text("服务地址（Base URL）", "Base URL")
                        .to_string(),
                    draft.base_url.value.clone(),
                    false,
                )),
                CustomFormStep::ModelId => Some((
                    language.text("模型 ID", "Model ID").to_string(),
                    draft.model_id.value.clone(),
                    false,
                )),
                CustomFormStep::ApiKey => {
                    Some(("API Key".to_string(), draft.api_key.masked(), true))
                }
                CustomFormStep::ContextWindow => Some((
                    language.text("上下文窗口", "Context window").to_string(),
                    draft.context_window.value.clone(),
                    false,
                )),
                CustomFormStep::MaxOutputTokens => Some((
                    language
                        .text("最大输出 Token", "Max output tokens")
                        .to_string(),
                    draft.max_output_tokens.value.clone(),
                    false,
                )),
                _ => None,
            },
            View::Form(FormInput::ByoPath { path }) => Some((
                language.text("GGUF 文件路径", "GGUF path").to_string(),
                path.value.clone(),
                false,
            )),
            View::Form(FormInput::ByoName { name, .. }) => Some((
                language
                    .text("显示名称（可选）", "Display name (optional)")
                    .to_string(),
                name.value.clone(),
                false,
            )),
            View::Form(FormInput::ServerPort { port, .. }) => Some((
                language.text("端口（可选）", "Port (optional)").to_string(),
                port.value.clone(),
                false,
            )),
            _ => None,
        }
    }

    pub(crate) fn details(&self, language: UiLanguage) -> Vec<String> {
        match &self.view {
            View::CustomEntry(id) => self
                .custom_entries
                .iter()
                .find(|entry| &entry.id == id)
                .map(|entry| {
                    vec![
                        format!("{} · {}", entry.model_id, entry.protocol),
                        entry.base_url.clone(),
                        match language {
                            UiLanguage::ZhCn => format!(
                                "上下文 {} · 最大输出 {} · 密钥 {}",
                                entry.context_window,
                                entry.max_output_tokens,
                                if entry.has_stored_credential {
                                    "已安全保存"
                                } else {
                                    "未保存"
                                }
                            ),
                            UiLanguage::EnUs => format!(
                                "Context {} · max output {} · credential {}",
                                entry.context_window,
                                entry.max_output_tokens,
                                if entry.has_stored_credential {
                                    "stored securely"
                                } else {
                                    "not stored"
                                }
                            ),
                        },
                    ]
                })
                .unwrap_or_default(),
            View::LocalList => {
                let mut lines = Vec::new();
                if let Some(profile) = self.local_profile.as_ref() {
                    lines.push(format!(
                        "{}/{} · {} · {}",
                        profile.platform,
                        profile.arch,
                        format_bytes(profile.memory_bytes),
                        profile.recommended_runtime.as_deref().unwrap_or("—")
                    ));
                }
                if let Some(server) = self.local_server.as_ref() {
                    lines.push(match language {
                        UiLanguage::ZhCn => format!(
                            "推理服务：{}{}",
                            server.state,
                            server
                                .model_id
                                .as_deref()
                                .map_or(String::new(), |id| format!(" · {id}"))
                        ),
                        UiLanguage::EnUs => format!(
                            "Inference server: {}{}",
                            server.state,
                            server
                                .model_id
                                .as_deref()
                                .map_or(String::new(), |id| format!(" · {id}"))
                        ),
                    });
                }
                if let Some(state) = self.active_download_state.as_deref() {
                    lines.push(match (language, self.active_download_percentage) {
                        (UiLanguage::ZhCn, Some(percent)) => {
                            format!("下载：{state} · {percent:.1}%")
                        }
                        (UiLanguage::EnUs, Some(percent)) => {
                            format!("Download: {state} · {percent:.1}%")
                        }
                        (UiLanguage::ZhCn, None) => format!("下载：{state}"),
                        (UiLanguage::EnUs, None) => format!("Download: {state}"),
                    });
                }
                lines
            }
            View::LocalEntry(id) => self
                .local_entries
                .iter()
                .find(|entry| &entry.id == id)
                .map(|entry| {
                    let mut lines = vec![format!(
                        "{} · {} · {} · {}",
                        entry.id,
                        entry.runtime,
                        entry.status,
                        format_bytes(entry.size_bytes)
                    )];
                    if let Some(path) = entry.model_path.as_ref() {
                        lines.push(path.clone());
                    }
                    if let Some(reason) = entry.reason.as_ref() {
                        lines.push(reason.clone());
                    }
                    lines
                })
                .unwrap_or_default(),
            View::AccountMain => self.account_details(language),
            View::AccountOauth {
                authorization_url,
                verification_url,
                user_code,
                expires_at,
                ..
            } => {
                let mut lines = vec![language
                    .text(
                        "请在浏览器完成授权；本面板会自动轮询。",
                        "Complete authorization in your browser; this panel polls automatically.",
                    )
                    .to_string()];
                if let Some(url) = authorization_url.as_ref().or(verification_url.as_ref()) {
                    lines.push(url.clone());
                }
                if let Some(code) = user_code {
                    lines.push(match language {
                        UiLanguage::ZhCn => format!("设备代码：{code}"),
                        UiLanguage::EnUs => format!("Device code: {code}"),
                    });
                }
                if let Some(expires) = expires_at {
                    lines.push(match language {
                        UiLanguage::ZhCn => format!("过期时间：{expires}"),
                        UiLanguage::EnUs => format!("Expires: {expires}"),
                    });
                }
                lines
            }
            _ => Vec::new(),
        }
    }

    fn account_details(&self, language: UiLanguage) -> Vec<String> {
        let Some(snapshot) = self.account_snapshot.as_ref() else {
            return Vec::new();
        };
        let mut lines = vec![match language {
            UiLanguage::ZhCn => format!("资格：{}", snapshot.eligibility.state),
            UiLanguage::EnUs => format!("Eligibility: {}", snapshot.eligibility.state),
        }];
        if let Some(reason) = snapshot.eligibility.reason_code.as_ref() {
            lines.push(match language {
                UiLanguage::ZhCn => format!("原因：{reason}"),
                UiLanguage::EnUs => format!("Reason: {reason}"),
            });
        }
        if let Some(runtime) = snapshot.runtime.as_ref() {
            lines.push(match language {
                UiLanguage::ZhCn => format!("账户运行环境：{}", runtime.state),
                UiLanguage::EnUs => format!("Account runtime: {}", runtime.state),
            });
        }
        lines
    }

    pub(crate) fn rows(&self, language: UiLanguage) -> Vec<ModelManagementRow> {
        let row = |zh: &'static str, en: &'static str, detail: Option<String>, disabled: bool| {
            ModelManagementRow {
                label: language.text(zh, en).to_string(),
                detail,
                disabled,
            }
        };
        match &self.view {
            View::Home => vec![
                row("自定义模型", "Custom models", None, false),
                row("本地模型", "Local models", None, false),
                row("本地账户接入", "Local account connections", None, false),
                row("返回对话", "Back to chat", None, false),
            ],
            View::CustomList => {
                let mut rows = self
                    .custom_entries
                    .iter()
                    .map(|entry| ModelManagementRow {
                        label: format!(
                            "{}{}{}",
                            entry.display_name.as_deref().unwrap_or(&entry.model_id),
                            if entry.enabled {
                                ""
                            } else {
                                language.text("（已停用）", " (disabled)")
                            },
                            if entry.is_default == Some(true) {
                                " ★"
                            } else {
                                ""
                            }
                        ),
                        detail: Some(entry.base_url.clone()),
                        disabled: false,
                    })
                    .collect::<Vec<_>>();
                rows.push(row("＋ 添加自定义模型", "+ Add custom model", None, false));
                rows.push(row("返回", "Back", None, false));
                rows
            }
            View::CustomEntry(id) => {
                let entry = self.custom_entries.iter().find(|entry| &entry.id == id);
                let enabled = entry.is_some_and(|entry| entry.enabled);
                vec![
                    row("选择此模型", "Select this model", None, !enabled),
                    row("测试连通性", "Test connection", None, entry.is_none()),
                    row("编辑", "Edit", None, entry.is_none()),
                    row(
                        if enabled { "停用" } else { "启用" },
                        if enabled { "Disable" } else { "Enable" },
                        None,
                        entry.is_none(),
                    ),
                    row("删除", "Delete", None, entry.is_none()),
                    row("返回", "Back", None, false),
                ]
            }
            View::CustomDelete(_) | View::LocalRemove(_) | View::AccountRemove(_) => vec![
                row("取消", "Cancel", None, false),
                row("确认", "Confirm", None, false),
            ],
            View::Form(FormInput::Custom { step, draft, .. }) => match step {
                CustomFormStep::Protocol => vec![
                    ModelManagementRow {
                        label: language
                            .text("Anthropic 兼容", "Anthropic compatible")
                            .to_string(),
                        detail: None,
                        disabled: false,
                    },
                    ModelManagementRow {
                        label: language
                            .text("OpenAI 兼容", "OpenAI compatible")
                            .to_string(),
                        detail: None,
                        disabled: false,
                    },
                ],
                CustomFormStep::SupportsThinking => bool_rows(language, draft.supports_thinking),
                CustomFormStep::SupportsVision => bool_rows(language, draft.supports_vision),
                CustomFormStep::Enabled => bool_rows(language, draft.enabled),
                CustomFormStep::Submit => vec![
                    row("保存", "Save", None, false),
                    row(
                        "测试当前表单连接",
                        "Test current form connection",
                        None,
                        draft.api_key.expose_for_submission().is_empty(),
                    ),
                    row("返回修改", "Back to edit", None, false),
                ],
                _ => Vec::new(),
            },
            View::Form(
                FormInput::ByoPath { .. }
                | FormInput::ByoName { .. }
                | FormInput::ServerPort { .. },
            ) => Vec::new(),
            View::LocalList => {
                let mut rows = self
                    .local_entries
                    .iter()
                    .map(|entry| ModelManagementRow {
                        label: format!(
                            "{}{}",
                            entry.display_name,
                            if entry.installed { " ✓" } else { "" }
                        ),
                        detail: Some(format!("{} · {}", entry.runtime, entry.status)),
                        disabled: false,
                    })
                    .collect::<Vec<_>>();
                rows.push(row("＋ 添加本机 GGUF", "+ Add local GGUF", None, false));
                rows.push(row("刷新状态", "Refresh status", None, false));
                if self.active_download_id.is_some() || self.active_download_model_id.is_some() {
                    rows.push(row(
                        "刷新下载进度",
                        "Refresh download progress",
                        None,
                        false,
                    ));
                    rows.push(row("取消下载", "Cancel download", None, false));
                }
                if self
                    .local_server
                    .as_ref()
                    .is_some_and(|server| matches!(server.state.as_str(), "running" | "starting"))
                {
                    rows.push(row("停止推理服务", "Stop inference server", None, false));
                } else {
                    rows.push(row("读取服务状态", "Read server status", None, false));
                }
                rows.push(row("返回", "Back", None, false));
                rows
            }
            View::LocalEntry(id) => {
                let entry = self.local_entries.iter().find(|entry| &entry.id == id);
                let installed = entry.is_some_and(|entry| entry.installed);
                let byo = entry.is_some_and(|entry| entry.source == "user-local-path");
                let server_for_entry =
                    entry
                        .zip(self.local_server.as_ref())
                        .is_some_and(|(entry, server)| {
                            server.model_id.as_deref() == Some(entry.id.as_str())
                                && matches!(server.state.as_str(), "running" | "starting")
                        });
                vec![
                    row("选择此模型", "Select this model", None, !installed),
                    row("下载安装", "Download and install", None, installed || byo),
                    row(
                        "启动推理服务",
                        "Start inference server",
                        None,
                        !installed || server_for_entry,
                    ),
                    row(
                        "停止推理服务",
                        "Stop inference server",
                        None,
                        !server_for_entry,
                    ),
                    row(
                        if byo {
                            "注销条目（不删除文件）"
                        } else {
                            "移除安装文件"
                        },
                        if byo {
                            "Unregister (keep file)"
                        } else {
                            "Remove installed files"
                        },
                        None,
                        !installed,
                    ),
                    row("刷新状态", "Refresh status", None, false),
                    row("返回", "Back", None, false),
                ]
            }
            View::AccountMain => self.account_rows(language),
            View::AccountConnect => {
                let mut rows = self
                    .account_snapshot
                    .as_ref()
                    .map(|snapshot| {
                        snapshot
                            .connectors
                            .iter()
                            .map(|connector| ModelManagementRow {
                                label: connector.display_name.clone(),
                                detail: Some(if connector.enabled {
                                    connector.auth_mode.clone()
                                } else {
                                    connector
                                        .disabled_reason_code
                                        .clone()
                                        .unwrap_or_else(|| connector.terms_status.clone())
                                }),
                                disabled: !connector.enabled,
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                rows.push(row("返回", "Back", None, false));
                rows
            }
            View::AccountOauth { .. } => vec![
                row("立即检查授权状态", "Check authorization now", None, false),
                row("取消登录", "Cancel login", None, false),
            ],
        }
    }

    fn account_rows(&self, language: UiLanguage) -> Vec<ModelManagementRow> {
        let row = |zh: &'static str, en: &'static str, detail: Option<String>, disabled: bool| {
            ModelManagementRow {
                label: language.text(zh, en).to_string(),
                detail,
                disabled,
            }
        };
        let Some(snapshot) = self.account_snapshot.as_ref() else {
            return vec![
                row("刷新", "Refresh", None, false),
                row("返回", "Back", None, false),
            ];
        };
        if snapshot.eligibility.reason_code.as_deref() == Some("consent-required") {
            return vec![
                row(
                    "同意地区资格检测",
                    "Allow regional eligibility check",
                    None,
                    false,
                ),
                row("不同意并保持锁定", "Decline and remain locked", None, false),
                row("返回", "Back", None, false),
            ];
        }
        let protected = snapshot.eligibility.state == "allowed"
            || (snapshot.eligibility.state == "blocked-cn"
                && snapshot
                    .connectors
                    .iter()
                    .any(|connector| connector.enabled));
        if !protected {
            return vec![
                row("重新检测资格", "Recheck eligibility", None, false),
                row("返回", "Back", None, false),
            ];
        }
        let runtime_ready = snapshot
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.state == "ready");
        let mut rows = vec![
            row("连接账户", "Connect account", None, !runtime_ready),
            row(
                if runtime_ready {
                    "停止账户运行环境"
                } else {
                    "启动账户运行环境"
                },
                if runtime_ready {
                    "Stop account runtime"
                } else {
                    "Start account runtime"
                },
                None,
                false,
            ),
        ];
        for route in &snapshot.routes {
            let usage = snapshot
                .usage
                .iter()
                .find(|usage| usage.route_id == route.route_id);
            rows.push(ModelManagementRow {
                label: route
                    .display_name
                    .as_deref()
                    .unwrap_or(&route.model_id)
                    .to_string(),
                detail: Some(match usage.and_then(|usage| usage.remaining_percent) {
                    Some(percent) => format!(
                        "{} · {} · {percent:.1}%",
                        route.connector_label, route.account_label
                    ),
                    None => format!("{} · {}", route.connector_label, route.account_label),
                }),
                disabled: route.chat_runtime_supported == Some(false),
            });
        }
        for account in &snapshot.accounts {
            rows.push(ModelManagementRow {
                label: match language {
                    UiLanguage::ZhCn => format!("移除账户：{}", account.display_label),
                    UiLanguage::EnUs => format!("Remove account: {}", account.display_label),
                },
                detail: Some(account.status.clone()),
                disabled: false,
            });
        }
        rows.push(row(
            "刷新账户、用量与路由",
            "Refresh accounts, usage, and routes",
            None,
            false,
        ));
        rows.push(row("返回", "Back", None, false));
        rows
    }

    pub(crate) fn paste(&mut self, text: &str) {
        match &mut self.view {
            View::Form(FormInput::Custom { step, draft, .. }) => match step {
                CustomFormStep::BaseUrl => draft.base_url.insert(text),
                CustomFormStep::ModelId => draft.model_id.insert(text),
                CustomFormStep::ApiKey => draft.api_key.insert(text),
                CustomFormStep::ContextWindow => draft.context_window.insert(text),
                CustomFormStep::MaxOutputTokens => draft.max_output_tokens.insert(text),
                _ => {}
            },
            View::Form(FormInput::ByoPath { path }) => path.insert(text),
            View::Form(FormInput::ByoName { name, .. }) => name.insert(text),
            View::Form(FormInput::ServerPort { port, .. }) => port.insert(text),
            _ => {}
        }
    }

    pub(crate) fn key(
        &mut self,
        key: KeyEvent,
        language: UiLanguage,
    ) -> Vec<ModelManagementEffect> {
        if self.pending.is_some() {
            if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
                return vec![ModelManagementEffect::Close];
            }
            return Vec::new();
        }
        if let View::Form(form) = &mut self.view
            && Self::edit_form_key(form, key)
        {
            return Vec::new();
        }
        match key {
            KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let len = self.rows(language).len();
                self.selected = self.selected.saturating_sub(1);
                self.clamp_selection(len);
                Vec::new()
            }
            KeyEvent {
                code: KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let len = self.rows(language).len();
                self.selected = self.selected.saturating_add(1);
                self.clamp_selection(len);
                Vec::new()
            }
            KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.back(),
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => self.accept(language),
            _ => Vec::new(),
        }
    }

    fn edit_form_key(form: &mut FormInput, key: KeyEvent) -> bool {
        match form {
            FormInput::Custom { step, draft, .. } => match step {
                CustomFormStep::BaseUrl => draft.base_url.key(key),
                CustomFormStep::ModelId => draft.model_id.key(key),
                CustomFormStep::ApiKey => draft.api_key.key(key),
                CustomFormStep::ContextWindow => draft.context_window.key(key),
                CustomFormStep::MaxOutputTokens => draft.max_output_tokens.key(key),
                _ => false,
            },
            FormInput::ByoPath { path } => path.key(key),
            FormInput::ByoName { name, .. } => name.key(key),
            FormInput::ServerPort { port, .. } => port.key(key),
        }
    }

    fn clamp_selection(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    fn back(&mut self) -> Vec<ModelManagementEffect> {
        self.notice = None;
        match &mut self.view {
            View::Home => vec![ModelManagementEffect::Close],
            View::CustomList | View::LocalList | View::AccountMain => {
                self.view = View::Home;
                self.selected = 0;
                Vec::new()
            }
            View::CustomEntry(_) | View::CustomDelete(_) => {
                self.view = View::CustomList;
                self.selected = 0;
                Vec::new()
            }
            View::LocalEntry(_) | View::LocalRemove(_) => {
                self.view = View::LocalList;
                self.selected = 0;
                Vec::new()
            }
            View::AccountConnect | View::AccountRemove(_) => {
                self.view = View::AccountMain;
                self.selected = 0;
                Vec::new()
            }
            View::AccountOauth { session_id, .. } => {
                let session_id = session_id.clone();
                self.pending = Some(PendingOperation::AccountLoginCancel);
                vec![ModelManagementEffect::Private {
                    action: json!({"kind":"model.account.login_cancel","sessionId":session_id}),
                    purpose: PrivateRuntimePurpose::ModelAccountLoginCancel,
                }]
            }
            View::Form(FormInput::Custom { mode, step, .. }) => {
                if let Some(previous) = step.previous() {
                    *step = previous;
                    self.selected = 0;
                } else {
                    self.view = match mode {
                        CustomFormMode::Add => View::CustomList,
                        CustomFormMode::Edit { id } => View::CustomEntry(id.clone()),
                    };
                }
                Vec::new()
            }
            View::Form(FormInput::ByoPath { .. }) => {
                self.view = View::LocalList;
                Vec::new()
            }
            View::Form(FormInput::ByoName { path, .. }) => {
                self.view = View::Form(FormInput::ByoPath {
                    path: PlainText::new(path.clone()),
                });
                Vec::new()
            }
            View::Form(FormInput::ServerPort { entry_id, .. }) => {
                self.view = View::LocalEntry(entry_id.clone());
                Vec::new()
            }
        }
    }

    fn accept(&mut self, language: UiLanguage) -> Vec<ModelManagementEffect> {
        let rows = self.rows(language);
        if rows.get(self.selected).is_some_and(|row| row.disabled) {
            self.notice = Some(
                language
                    .text("此操作当前不可用", "This action is currently unavailable")
                    .to_string(),
            );
            return Vec::new();
        }
        match self.view.clone() {
            View::Home => self.accept_home(),
            View::CustomList => self.accept_custom_list(),
            View::CustomEntry(id) => self.accept_custom_entry(id),
            View::CustomDelete(id) => self.accept_custom_delete(id),
            View::Form(form) => self.accept_form(form, language),
            View::LocalList => self.accept_local_list(),
            View::LocalEntry(id) => self.accept_local_entry(id),
            View::LocalRemove(id) => self.accept_local_remove(id),
            View::AccountMain => self.accept_account_main(),
            View::AccountConnect => self.accept_account_connect(),
            View::AccountRemove(id) => self.accept_account_remove(id),
            View::AccountOauth { session_id, .. } => {
                if self.selected == 0 {
                    self.start_account_poll(session_id)
                } else {
                    self.pending = Some(PendingOperation::AccountLoginCancel);
                    vec![ModelManagementEffect::Private {
                        action: json!({"kind":"model.account.login_cancel","sessionId":session_id}),
                        purpose: PrivateRuntimePurpose::ModelAccountLoginCancel,
                    }]
                }
            }
        }
    }

    fn accept_home(&mut self) -> Vec<ModelManagementEffect> {
        let selected = self.selected;
        self.selected = 0;
        match selected {
            0 => {
                self.view = View::CustomList;
                Vec::new()
            }
            1 => self.request_local_snapshot(),
            2 => self.request_account_snapshot(false),
            _ => vec![ModelManagementEffect::Close],
        }
    }

    fn accept_custom_list(&mut self) -> Vec<ModelManagementEffect> {
        let entry_count = self.custom_entries.len();
        if self.selected < entry_count {
            self.view = View::CustomEntry(self.custom_entries[self.selected].id.clone());
            self.selected = 0;
        } else if self.selected == entry_count {
            self.view = View::Form(FormInput::Custom {
                mode: CustomFormMode::Add,
                step: CustomFormStep::Protocol,
                draft: CustomDraft::empty(),
            });
            self.selected = 0;
        } else {
            self.view = View::Home;
            self.selected = 0;
        }
        Vec::new()
    }

    fn accept_custom_entry(&mut self, id: String) -> Vec<ModelManagementEffect> {
        let Some(entry) = self
            .custom_entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
        else {
            self.notice = Some("custom-model-entry-not-found".to_string());
            return Vec::new();
        };
        match self.selected {
            0 => vec![ModelManagementEffect::SetModel(entry.model_reference)],
            1 => {
                self.pending = Some(PendingOperation::CustomTest);
                vec![ModelManagementEffect::Private {
                    action: json!({"kind":"model.custom.test_saved","id":entry.id}),
                    purpose: PrivateRuntimePurpose::ModelCustomTest,
                }]
            }
            2 => {
                self.view = View::Form(FormInput::Custom {
                    mode: CustomFormMode::Edit {
                        id: entry.id.clone(),
                    },
                    step: CustomFormStep::BaseUrl,
                    draft: CustomDraft::from_entry(&entry),
                });
                self.selected = 0;
                Vec::new()
            }
            3 => {
                self.pending = Some(PendingOperation::CustomMutation);
                vec![ModelManagementEffect::Private {
                    action: json!({"kind":"model.custom.toggle","id":entry.id,"enabled":!entry.enabled}),
                    purpose: PrivateRuntimePurpose::ModelCustomMutation,
                }]
            }
            4 => {
                self.view = View::CustomDelete(entry.id);
                self.selected = 0;
                Vec::new()
            }
            _ => {
                self.view = View::CustomList;
                self.selected = 0;
                Vec::new()
            }
        }
    }

    fn accept_custom_delete(&mut self, id: String) -> Vec<ModelManagementEffect> {
        if self.selected == 0 {
            self.view = View::CustomEntry(id);
            self.selected = 0;
            Vec::new()
        } else {
            self.pending = Some(PendingOperation::CustomMutation);
            vec![ModelManagementEffect::Private {
                action: json!({"kind":"model.custom.remove","id":id}),
                purpose: PrivateRuntimePurpose::ModelCustomMutation,
            }]
        }
    }

    fn accept_form(&mut self, form: FormInput, language: UiLanguage) -> Vec<ModelManagementEffect> {
        match form {
            FormInput::Custom {
                mode,
                step,
                mut draft,
            } => {
                match step {
                    CustomFormStep::Protocol => {
                        draft.protocol = if self.selected == 0 {
                            "anthropic-compatible"
                        } else {
                            "openai-compatible"
                        }
                        .to_string();
                    }
                    CustomFormStep::SupportsThinking => {
                        draft.supports_thinking = self.selected == 0
                    }
                    CustomFormStep::SupportsVision => draft.supports_vision = self.selected == 0,
                    CustomFormStep::Enabled => draft.enabled = self.selected == 0,
                    CustomFormStep::BaseUrl if draft.base_url.value.trim().is_empty() => {
                        self.notice = Some(
                            language
                                .text("Base URL 不能为空", "Base URL cannot be empty")
                                .to_string(),
                        );
                        return Vec::new();
                    }
                    CustomFormStep::ModelId if draft.model_id.value.trim().is_empty() => {
                        self.notice = Some(
                            language
                                .text("Model ID 不能为空", "Model ID cannot be empty")
                                .to_string(),
                        );
                        return Vec::new();
                    }
                    CustomFormStep::ApiKey
                        if matches!(mode, CustomFormMode::Add)
                            && draft.api_key.expose_for_submission().is_empty() =>
                    {
                        self.notice = Some(
                            language
                                .text(
                                    "新增模型必须输入 API Key",
                                    "An API key is required for a new model",
                                )
                                .to_string(),
                        );
                        return Vec::new();
                    }
                    CustomFormStep::ContextWindow
                        if draft
                            .context_window
                            .value
                            .trim()
                            .parse::<u64>()
                            .ok()
                            .filter(|value| *value > 0)
                            .is_none() =>
                    {
                        self.notice = Some(
                            language
                                .text(
                                    "上下文窗口必须是正整数",
                                    "Context window must be a positive integer",
                                )
                                .to_string(),
                        );
                        return Vec::new();
                    }
                    CustomFormStep::MaxOutputTokens
                        if draft
                            .max_output_tokens
                            .value
                            .trim()
                            .parse::<u64>()
                            .ok()
                            .filter(|value| *value > 0)
                            .is_none() =>
                    {
                        self.notice = Some(
                            language
                                .text(
                                    "最大输出必须是正整数",
                                    "Max output tokens must be a positive integer",
                                )
                                .to_string(),
                        );
                        return Vec::new();
                    }
                    CustomFormStep::Submit if self.selected == 0 => {
                        let include_empty_key = matches!(mode, CustomFormMode::Add);
                        let Ok(input) = draft.action_input(include_empty_key) else {
                            self.notice = Some(
                                language
                                    .text("表单字段无效", "The form contains invalid fields")
                                    .to_string(),
                            );
                            return Vec::new();
                        };
                        let action = match mode {
                            CustomFormMode::Add => json!({"kind":"model.custom.add","input":input}),
                            CustomFormMode::Edit { id } => {
                                json!({"kind":"model.custom.update","id":id,"input":input})
                            }
                        };
                        self.pending = Some(PendingOperation::CustomMutation);
                        return vec![ModelManagementEffect::Private {
                            action,
                            purpose: PrivateRuntimePurpose::ModelCustomMutation,
                        }];
                    }
                    CustomFormStep::Submit if self.selected == 1 => {
                        let base_url = draft.base_url.value.trim();
                        let model_id = draft.model_id.value.trim();
                        let api_key = draft.api_key.expose_for_submission();
                        if base_url.is_empty() || model_id.is_empty() || api_key.is_empty() {
                            self.notice = Some(
                                language
                                    .text(
                                        "测试草稿需要 Base URL、Model ID 和 API Key",
                                        "Draft testing requires Base URL, Model ID, and API Key",
                                    )
                                    .to_string(),
                            );
                            return Vec::new();
                        }
                        self.pending = Some(PendingOperation::CustomTest);
                        return vec![ModelManagementEffect::Private {
                            action: json!({
                                "kind":"model.custom.test_draft",
                                "baseUrl":base_url,
                                "protocol":draft.protocol,
                                "modelId":model_id,
                                "apiKey":api_key
                            }),
                            purpose: PrivateRuntimePurpose::ModelCustomTest,
                        }];
                    }
                    CustomFormStep::Submit => {
                        self.view = View::Form(FormInput::Custom {
                            mode,
                            step: CustomFormStep::Enabled,
                            draft,
                        });
                        self.selected = 0;
                        return Vec::new();
                    }
                    _ => {}
                }
                let next = step.next().unwrap_or(step);
                self.view = View::Form(FormInput::Custom {
                    mode,
                    step: next,
                    draft,
                });
                self.selected = 0;
                self.notice = None;
                Vec::new()
            }
            FormInput::ByoPath { path } => {
                let path = path.value.trim().to_string();
                if path.is_empty() {
                    self.notice = Some(
                        language
                            .text("GGUF 路径不能为空", "GGUF path cannot be empty")
                            .to_string(),
                    );
                    return Vec::new();
                }
                self.view = View::Form(FormInput::ByoName {
                    path,
                    name: PlainText::new(""),
                });
                Vec::new()
            }
            FormInput::ByoName { path, name } => {
                self.pending = Some(PendingOperation::LocalByoAdd);
                let name = name.value.trim();
                vec![ModelManagementEffect::Private {
                    action: json!({"kind":"model.local.byo_add","ggufPath":path,"displayName":if name.is_empty() { Value::Null } else { json!(name) }}),
                    purpose: PrivateRuntimePurpose::ModelLocalByoAdd,
                }]
            }
            FormInput::ServerPort {
                entry_id,
                path,
                runtime,
                port,
            } => {
                let parsed_port = if port.value.trim().is_empty() {
                    None
                } else {
                    port.value
                        .trim()
                        .parse::<u16>()
                        .ok()
                        .filter(|value| *value > 0)
                };
                if !port.value.trim().is_empty() && parsed_port.is_none() {
                    self.notice = Some(
                        language
                            .text("端口必须是 1 到 65535", "Port must be between 1 and 65535")
                            .to_string(),
                    );
                    return Vec::new();
                }
                self.pending = Some(PendingOperation::LocalServer);
                vec![ModelManagementEffect::Private {
                    action: json!({"kind":"model.local.server_start","modelId":entry_id,"modelPath":path,"runtime":runtime,"port":parsed_port,"contextSize":Value::Null,"gpuLayers":Value::Null}),
                    purpose: PrivateRuntimePurpose::ModelLocalServer,
                }]
            }
        }
    }

    fn request_local_snapshot(&mut self) -> Vec<ModelManagementEffect> {
        self.view = View::LocalList;
        self.selected = 0;
        self.pending = Some(PendingOperation::LocalSnapshot);
        vec![ModelManagementEffect::Private {
            action: json!({"kind":"model.local.snapshot"}),
            purpose: PrivateRuntimePurpose::ModelLocalSnapshot,
        }]
    }

    fn accept_local_list(&mut self) -> Vec<ModelManagementEffect> {
        let count = self.local_entries.len();
        if self.selected < count {
            self.view = View::LocalEntry(self.local_entries[self.selected].id.clone());
            self.selected = 0;
            return Vec::new();
        }
        let mut cursor = count;
        if self.selected == cursor {
            self.view = View::Form(FormInput::ByoPath {
                path: PlainText::new(""),
            });
            self.selected = 0;
            return Vec::new();
        }
        cursor += 1;
        if self.selected == cursor {
            return self.request_local_snapshot();
        }
        cursor += 1;
        if self.active_download_id.is_some() || self.active_download_model_id.is_some() {
            if self.selected == cursor {
                self.pending = Some(PendingOperation::LocalDownloadProgress);
                return vec![ModelManagementEffect::Private {
                    action: json!({"kind":"model.local.download_progress","downloadId":self.active_download_id,"modelId":self.active_download_model_id}),
                    purpose: PrivateRuntimePurpose::ModelLocalDownload,
                }];
            }
            cursor += 1;
            if self.selected == cursor {
                self.pending = Some(PendingOperation::LocalDownloadCancel);
                return vec![ModelManagementEffect::Private {
                    action: json!({"kind":"model.local.download_cancel","downloadId":self.active_download_id,"modelId":self.active_download_model_id}),
                    purpose: PrivateRuntimePurpose::ModelLocalDownload,
                }];
            }
            cursor += 1;
        }
        if self.selected == cursor {
            let running = self
                .local_server
                .as_ref()
                .is_some_and(|server| matches!(server.state.as_str(), "running" | "starting"));
            self.pending = Some(PendingOperation::LocalServer);
            return vec![ModelManagementEffect::Private {
                action: if running {
                    json!({"kind":"model.local.server_stop","modelId":self.local_server.as_ref().and_then(|server| server.model_id.clone()),"modelPath":self.local_server.as_ref().and_then(|server| server.model_path.clone())})
                } else {
                    json!({"kind":"model.local.server_status"})
                },
                purpose: PrivateRuntimePurpose::ModelLocalServer,
            }];
        }
        self.view = View::Home;
        self.selected = 0;
        Vec::new()
    }

    fn accept_local_entry(&mut self, id: String) -> Vec<ModelManagementEffect> {
        let Some(entry) = self
            .local_entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
        else {
            self.notice = Some("local-model-entry-not-found".to_string());
            return Vec::new();
        };
        match self.selected {
            0 => vec![ModelManagementEffect::SetModel(entry.model_reference)],
            1 => {
                self.pending = Some(PendingOperation::LocalDownloadStart);
                self.active_download_model_id = Some(entry.id.clone());
                vec![ModelManagementEffect::Private {
                    action: json!({"kind":"model.local.download_start","modelId":entry.id}),
                    purpose: PrivateRuntimePurpose::ModelLocalDownload,
                }]
            }
            2 => {
                self.view = View::Form(FormInput::ServerPort {
                    entry_id: entry.id,
                    path: entry.model_path,
                    runtime: entry.runtime,
                    port: PlainText::new(""),
                });
                Vec::new()
            }
            3 => {
                self.pending = Some(PendingOperation::LocalServer);
                vec![ModelManagementEffect::Private {
                    action: json!({"kind":"model.local.server_stop","modelId":entry.id,"modelPath":entry.model_path}),
                    purpose: PrivateRuntimePurpose::ModelLocalServer,
                }]
            }
            4 => {
                self.view = View::LocalRemove(entry.id);
                self.selected = 0;
                Vec::new()
            }
            5 => self.request_local_snapshot(),
            _ => {
                self.view = View::LocalList;
                self.selected = 0;
                Vec::new()
            }
        }
    }

    fn accept_local_remove(&mut self, id: String) -> Vec<ModelManagementEffect> {
        if self.selected == 0 {
            self.view = View::LocalEntry(id);
            self.selected = 0;
            return Vec::new();
        }
        let Some(entry) = self.local_entries.iter().find(|entry| entry.id == id) else {
            return Vec::new();
        };
        if entry.source == "user-local-path" {
            self.pending = Some(PendingOperation::LocalByoRemove);
            vec![ModelManagementEffect::Private {
                action: json!({"kind":"model.local.byo_remove","id":entry.id}),
                purpose: PrivateRuntimePurpose::ModelLocalByoRemove,
            }]
        } else {
            self.pending = Some(PendingOperation::LocalInstallRemove);
            vec![ModelManagementEffect::Private {
                action: json!({"kind":"model.local.install_remove","modelId":entry.id,"modelPath":entry.model_path,"removeFiles":true}),
                purpose: PrivateRuntimePurpose::ModelLocalInstallRemove,
            }]
        }
    }

    fn request_account_snapshot(&mut self, force_refresh: bool) -> Vec<ModelManagementEffect> {
        self.view = View::AccountMain;
        self.selected = 0;
        self.pending = Some(PendingOperation::AccountSnapshot);
        vec![ModelManagementEffect::Private {
            action: json!({"kind":"model.account.snapshot","forceRefresh":force_refresh}),
            purpose: PrivateRuntimePurpose::ModelAccountSnapshot,
        }]
    }

    fn accept_account_main(&mut self) -> Vec<ModelManagementEffect> {
        let Some(snapshot) = self.account_snapshot.clone() else {
            if self.selected == 0 {
                return self.request_account_snapshot(true);
            }
            self.view = View::Home;
            return Vec::new();
        };
        if snapshot.eligibility.reason_code.as_deref() == Some("consent-required") {
            match self.selected {
                0 | 1 => {
                    let granted = self.selected == 0;
                    self.pending = Some(PendingOperation::AccountConsent);
                    return vec![ModelManagementEffect::Private {
                        action: json!({"kind":"model.account.consent","granted":granted}),
                        purpose: PrivateRuntimePurpose::ModelAccountConsent,
                    }];
                }
                _ => {
                    self.view = View::Home;
                    self.selected = 0;
                    return Vec::new();
                }
            }
        }
        let protected = snapshot.eligibility.state == "allowed"
            || (snapshot.eligibility.state == "blocked-cn"
                && snapshot
                    .connectors
                    .iter()
                    .any(|connector| connector.enabled));
        if !protected {
            if self.selected == 0 {
                return self.request_account_snapshot(true);
            }
            self.view = View::Home;
            self.selected = 0;
            return Vec::new();
        }
        let runtime_ready = snapshot
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.state == "ready");
        if self.selected == 0 {
            self.view = View::AccountConnect;
            self.selected = 0;
            return Vec::new();
        }
        if self.selected == 1 {
            self.pending = Some(if runtime_ready {
                PendingOperation::AccountRuntimeStop
            } else {
                PendingOperation::AccountRuntimeEnsure
            });
            return vec![ModelManagementEffect::Private {
                action: if runtime_ready {
                    json!({"kind":"model.account.runtime_stop"})
                } else {
                    json!({"kind":"model.account.runtime_ensure"})
                },
                purpose: PrivateRuntimePurpose::ModelAccountRuntime,
            }];
        }
        let route_start = 2;
        if self.selected < route_start + snapshot.routes.len() {
            return vec![ModelManagementEffect::SetModel(
                snapshot.routes[self.selected - route_start]
                    .model_reference
                    .clone(),
            )];
        }
        let account_start = route_start + snapshot.routes.len();
        if self.selected < account_start + snapshot.accounts.len() {
            self.view = View::AccountRemove(
                snapshot.accounts[self.selected - account_start]
                    .account_id
                    .clone(),
            );
            self.selected = 0;
            return Vec::new();
        }
        if self.selected == account_start + snapshot.accounts.len() {
            return self.request_account_snapshot(true);
        }
        self.view = View::Home;
        self.selected = 0;
        Vec::new()
    }

    fn accept_account_connect(&mut self) -> Vec<ModelManagementEffect> {
        let connectors = self
            .account_snapshot
            .as_ref()
            .map(|snapshot| snapshot.connectors.clone())
            .unwrap_or_default();
        if self.selected >= connectors.len() {
            self.view = View::AccountMain;
            self.selected = 0;
            return Vec::new();
        }
        self.pending = Some(PendingOperation::AccountLoginStart);
        vec![ModelManagementEffect::Private {
            action: json!({"kind":"model.account.login_start","connectorId":connectors[self.selected].connector_id}),
            purpose: PrivateRuntimePurpose::ModelAccountLoginStart,
        }]
    }

    fn accept_account_remove(&mut self, id: String) -> Vec<ModelManagementEffect> {
        if self.selected == 0 {
            self.view = View::AccountMain;
            self.selected = 0;
            Vec::new()
        } else {
            self.pending = Some(PendingOperation::AccountRemove);
            vec![ModelManagementEffect::Private {
                action: json!({"kind":"model.account.remove","accountId":id}),
                purpose: PrivateRuntimePurpose::ModelAccountRemove,
            }]
        }
    }

    fn start_account_poll(&mut self, session_id: String) -> Vec<ModelManagementEffect> {
        self.pending = Some(PendingOperation::AccountLoginPoll);
        vec![ModelManagementEffect::Private {
            action: json!({"kind":"model.account.login_poll","sessionId":session_id}),
            purpose: PrivateRuntimePurpose::ModelAccountLoginPoll,
        }]
    }

    pub(crate) fn poll_due(&mut self, now: Instant) -> Vec<ModelManagementEffect> {
        let View::AccountOauth {
            session_id,
            next_poll_at,
            ..
        } = &self.view
        else {
            return Vec::new();
        };
        if self.pending.is_some() || now < *next_poll_at {
            return Vec::new();
        }
        self.start_account_poll(session_id.clone())
    }

    pub(crate) fn has_poll_work(&self) -> bool {
        matches!(self.view, View::AccountOauth { .. })
    }

    pub(crate) fn apply_result(
        &mut self,
        purpose: &PrivateRuntimePurpose,
        result_kind: &str,
        value: &Value,
        language: UiLanguage,
    ) -> Result<Vec<ModelManagementEffect>, String> {
        let result = value.get("result").ok_or("private-model-result-missing")?;
        let expected = expected_result_kind(purpose).ok_or("private-model-purpose-invalid")?;
        if result_kind != expected {
            return Err(format!(
                "private-model-result-kind-mismatch:{expected}:{result_kind}"
            ));
        }
        let pending = self
            .pending
            .take()
            .ok_or("private-model-result-without-pending-operation")?;
        self.notice = None;
        match (pending, result_kind) {
            (
                operation @ (PendingOperation::CustomLoad | PendingOperation::CustomMutation),
                "model.custom.list",
            ) => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    entries: Vec<CustomModelView>,
                    version: Option<u64>,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("custom-list-kind-mismatch".to_string());
                }
                let _version = parsed.version;
                self.custom_entries = parsed.entries;
                self.view = if operation == PendingOperation::CustomLoad {
                    View::Home
                } else {
                    View::CustomList
                };
                self.selected = 0;
                self.notice = Some(
                    language
                        .text("自定义模型已刷新", "Custom models refreshed")
                        .to_string(),
                );
            }
            (PendingOperation::CustomTest, "model.custom.test") => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Test {
                    ok: bool,
                    http_status: Option<u16>,
                    latency_ms: Option<f64>,
                    error_reason: Option<String>,
                }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    result: Test,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("custom-test-kind-mismatch".to_string());
                }
                self.notice = Some(if parsed.result.ok {
                    match language {
                        UiLanguage::ZhCn => format!(
                            "连接成功{}{}",
                            parsed
                                .result
                                .http_status
                                .map_or(String::new(), |status| format!(" · HTTP {status}")),
                            parsed
                                .result
                                .latency_ms
                                .map_or(String::new(), |ms| format!(" · {ms:.0}ms"))
                        ),
                        UiLanguage::EnUs => format!(
                            "Connection succeeded{}{}",
                            parsed
                                .result
                                .http_status
                                .map_or(String::new(), |status| format!(" · HTTP {status}")),
                            parsed
                                .result
                                .latency_ms
                                .map_or(String::new(), |ms| format!(" · {ms:.0}ms"))
                        ),
                    }
                } else {
                    parsed.result.error_reason.unwrap_or_else(|| {
                        language.text("连接失败", "Connection failed").to_string()
                    })
                });
            }
            (PendingOperation::LocalSnapshot, "model.local.snapshot") => {
                let parsed: LocalSnapshotResult =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("local-snapshot-kind-mismatch".to_string());
                }
                self.local_entries = parsed.catalog.data;
                self.local_profile = Some(parsed.profile);
                self.local_server = Some(parsed.server.status);
                self.view = View::LocalList;
                self.selected = 0;
            }
            (
                PendingOperation::LocalDownloadStart
                | PendingOperation::LocalDownloadProgress
                | PendingOperation::LocalDownloadCancel,
                "model.local.download",
            ) => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Download {
                    state: String,
                    reason: Option<String>,
                    download_id: Option<String>,
                    model_id: Option<String>,
                    bytes_received: Option<f64>,
                    total_bytes: Option<f64>,
                    percentage: Option<f64>,
                    error: Option<String>,
                }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Wrapped {
                    status: Download,
                }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    result: Wrapped,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("local-download-kind-mismatch".to_string());
                }
                self.active_download_id = parsed.result.status.download_id;
                self.active_download_model_id = parsed
                    .result
                    .status
                    .model_id
                    .or(self.active_download_model_id.take());
                self.active_download_state = Some(parsed.result.status.state.clone());
                self.active_download_percentage = parsed.result.status.percentage;
                let _progress_facts = (
                    parsed.result.status.bytes_received,
                    parsed.result.status.total_bytes,
                );
                self.notice = parsed
                    .result
                    .status
                    .error
                    .or(parsed.result.status.reason)
                    .or_else(|| {
                        Some(match language {
                            UiLanguage::ZhCn => format!("下载状态：{}", parsed.result.status.state),
                            UiLanguage::EnUs => format!("Download: {}", parsed.result.status.state),
                        })
                    });
                if matches!(
                    parsed.result.status.state.as_str(),
                    "completed" | "failed" | "cancelled" | "not-found" | "unavailable"
                ) {
                    self.active_download_id = None;
                }
            }
            (PendingOperation::LocalInstallRemove, "model.local.install_remove") => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Removal {
                    state: String,
                    reason: Option<String>,
                    model_id: Option<String>,
                    model_path: Option<String>,
                }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    result: Removal,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("local-remove-kind-mismatch".to_string());
                }
                let _removed = (parsed.result.model_id, parsed.result.model_path);
                self.notice = parsed.result.reason.or(Some(parsed.result.state));
                return Ok(self.request_local_snapshot());
            }
            (PendingOperation::LocalServer, "model.local.server") => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    result: LocalServerResult,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("local-server-kind-mismatch".to_string());
                }
                self.local_server = Some(parsed.result.status);
                self.view = View::LocalList;
                self.selected = 0;
            }
            (PendingOperation::LocalByoAdd, "model.local.byo_add") => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    entry: LocalCatalogEntry,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("local-byo-add-kind-mismatch".to_string());
                }
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!("已添加本地模型 {}", parsed.entry.id),
                    UiLanguage::EnUs => format!("Added local model {}", parsed.entry.id),
                });
                return Ok(self.request_local_snapshot());
            }
            (PendingOperation::LocalByoRemove, "model.local.byo_remove") => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    removed: bool,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("local-byo-remove-kind-mismatch".to_string());
                }
                self.notice = Some(match (language, parsed.removed) {
                    (UiLanguage::ZhCn, true) => "已注销本地模型条目；原文件未删除".to_string(),
                    (UiLanguage::ZhCn, false) => "未找到本地模型条目".to_string(),
                    (UiLanguage::EnUs, true) => {
                        "Local model entry removed; source file was kept".to_string()
                    }
                    (UiLanguage::EnUs, false) => "Local model entry was not found".to_string(),
                });
                return Ok(self.request_local_snapshot());
            }
            (PendingOperation::AccountSnapshot, "model.account.snapshot") => {
                let parsed: AccountSnapshotResult =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("account-snapshot-kind-mismatch".to_string());
                }
                self.account_snapshot = Some(parsed.snapshot);
                self.view = View::AccountMain;
                self.selected = 0;
            }
            (PendingOperation::AccountConsent, "model.account.consent") => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    granted: bool,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("account-consent-kind-mismatch".to_string());
                }
                if parsed.granted {
                    return Ok(self.request_account_snapshot(true));
                }
                self.notice = Some(
                    language
                        .text(
                            "未授权资格检测，账户接入保持锁定",
                            "Eligibility check declined; account access remains locked",
                        )
                        .to_string(),
                );
            }
            (
                PendingOperation::AccountRuntimeEnsure | PendingOperation::AccountRuntimeStop,
                "model.account.runtime",
            ) => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    runtime: AccountRuntime,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("account-runtime-kind-mismatch".to_string());
                }
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!("账户运行环境：{}", parsed.runtime.state),
                    UiLanguage::EnUs => format!("Account runtime: {}", parsed.runtime.state),
                });
                return Ok(self.request_account_snapshot(true));
            }
            (PendingOperation::AccountLoginStart, "model.account.login_start") => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct Session {
                    session_id: String,
                    auth_mode: String,
                    authorization_url: Option<String>,
                    user_code: Option<String>,
                    verification_url: Option<String>,
                    expires_at: Option<String>,
                }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    session: Session,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("account-login-start-kind-mismatch".to_string());
                }
                let Session {
                    session_id,
                    auth_mode: _auth_mode,
                    authorization_url,
                    user_code,
                    verification_url,
                    expires_at,
                } = parsed.session;
                let automatic_url = authorization_url
                    .as_ref()
                    .or(verification_url.as_ref())
                    .cloned();
                self.view = View::AccountOauth {
                    session_id,
                    authorization_url,
                    verification_url,
                    user_code,
                    expires_at,
                    next_poll_at: Instant::now(),
                };
                self.selected = 0;
                if let Some(url) = automatic_url {
                    return Ok(vec![ModelManagementEffect::OpenUrl(url)]);
                }
            }
            (PendingOperation::AccountLoginPoll, "model.account.login_poll") => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    state: String,
                    account_id: Option<String>,
                    error_code: Option<String>,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("account-login-poll-kind-mismatch".to_string());
                }
                if parsed.state == "pending" {
                    if let View::AccountOauth { next_poll_at, .. } = &mut self.view {
                        *next_poll_at = Instant::now() + ACCOUNT_LOGIN_POLL_INTERVAL;
                    }
                } else if parsed.state == "succeeded" && parsed.account_id.is_some() {
                    self.notice = Some(
                        language
                            .text("账户连接成功", "Account connected")
                            .to_string(),
                    );
                    return Ok(self.request_account_snapshot(true));
                } else {
                    self.notice = Some(parsed.error_code.unwrap_or(parsed.state));
                    self.view = View::AccountMain;
                    self.selected = 0;
                }
            }
            (PendingOperation::AccountLoginCancel, "model.account.login_cancel") => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    cancelled: bool,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("account-login-cancel-kind-mismatch".to_string());
                }
                self.notice = Some(
                    if parsed.cancelled {
                        language.text("登录已取消", "Login cancelled")
                    } else {
                        language.text("登录已结束", "Login was already finished")
                    }
                    .to_string(),
                );
                self.view = View::AccountMain;
                self.selected = 0;
            }
            (PendingOperation::AccountRemove, "model.account.remove") => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ResultValue {
                    kind: String,
                    removed: bool,
                }
                let parsed: ResultValue =
                    serde_json::from_value(result.clone()).map_err(|error| error.to_string())?;
                if parsed.kind != result_kind {
                    return Err("account-remove-kind-mismatch".to_string());
                }
                self.notice = Some(
                    if parsed.removed {
                        language.text("账户已移除", "Account removed")
                    } else {
                        language.text("账户未找到", "Account not found")
                    }
                    .to_string(),
                );
                return Ok(self.request_account_snapshot(true));
            }
            _ => return Err("private-model-pending-operation-mismatch".to_string()),
        }
        Ok(Vec::new())
    }

    pub(crate) fn apply_error(&mut self, language: UiLanguage, code: &str) {
        self.pending = None;
        self.notice = Some(match language {
            UiLanguage::ZhCn => format!("模型管理操作失败：{code}；会话继续运行"),
            UiLanguage::EnUs => {
                format!("Model management action failed: {code}; session remains active")
            }
        });
    }
}

fn expected_result_kind(purpose: &PrivateRuntimePurpose) -> Option<&'static str> {
    Some(match purpose {
        PrivateRuntimePurpose::ModelCustomList | PrivateRuntimePurpose::ModelCustomMutation => {
            "model.custom.list"
        }
        PrivateRuntimePurpose::ModelCustomTest => "model.custom.test",
        PrivateRuntimePurpose::ModelLocalSnapshot => "model.local.snapshot",
        PrivateRuntimePurpose::ModelLocalDownload => "model.local.download",
        PrivateRuntimePurpose::ModelLocalInstallRemove => "model.local.install_remove",
        PrivateRuntimePurpose::ModelLocalServer => "model.local.server",
        PrivateRuntimePurpose::ModelLocalByoAdd => "model.local.byo_add",
        PrivateRuntimePurpose::ModelLocalByoRemove => "model.local.byo_remove",
        PrivateRuntimePurpose::ModelAccountSnapshot => "model.account.snapshot",
        PrivateRuntimePurpose::ModelAccountConsent => "model.account.consent",
        PrivateRuntimePurpose::ModelAccountRuntime => "model.account.runtime",
        PrivateRuntimePurpose::ModelAccountLoginStart => "model.account.login_start",
        PrivateRuntimePurpose::ModelAccountLoginPoll => "model.account.login_poll",
        PrivateRuntimePurpose::ModelAccountLoginCancel => "model.account.login_cancel",
        PrivateRuntimePurpose::ModelAccountRemove => "model.account.remove",
        PrivateRuntimePurpose::HealthSnapshot
        | PrivateRuntimePurpose::BugReportSubmit
        | PrivateRuntimePurpose::RetainedIdentitySnapshot
        | PrivateRuntimePurpose::RetainedColorApply
        | PrivateRuntimePurpose::RetainedRenameApply
        | PrivateRuntimePurpose::UsageRead { .. }
        | PrivateRuntimePurpose::UsagePreferenceWrite { .. }
        | PrivateRuntimePurpose::PluginInventoryRead { .. }
        | PrivateRuntimePurpose::MarketplaceInventoryRead { .. }
        | PrivateRuntimePurpose::MarketplaceCatalogRead { .. }
        | PrivateRuntimePurpose::PluginInstall { .. }
        | PrivateRuntimePurpose::PluginUninstall { .. }
        | PrivateRuntimePurpose::PluginEnabledWrite { .. }
        | PrivateRuntimePurpose::PluginUpdate { .. }
        | PrivateRuntimePurpose::MarketplaceAdd { .. }
        | PrivateRuntimePurpose::MarketplaceRemove { .. }
        | PrivateRuntimePurpose::MarketplaceUpdate { .. }
        | PrivateRuntimePurpose::MarketplaceAutoUpdateWrite { .. }
        | PrivateRuntimePurpose::PluginValidate { .. } => return None,
    })
}

fn bool_rows(language: UiLanguage, value: bool) -> Vec<ModelManagementRow> {
    vec![
        ModelManagementRow {
            label: format!(
                "{}{}",
                language.text("开启", "On"),
                if value { " ✓" } else { "" }
            ),
            detail: None,
            disabled: false,
        },
        ModelManagementRow {
            label: format!(
                "{}{}",
                language.text("关闭", "Off"),
                if value { "" } else { " ✓" }
            ),
            detail: None,
            disabled: false,
        },
    ]
}

fn format_bytes(value: Option<f64>) -> String {
    let Some(mut value) = value.filter(|value| value.is_finite() && *value >= 0.0) else {
        return "—".to_string();
    };
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", units[unit])
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_and_visible_value_are_redacted() {
        let mut secret = SecretText::empty();
        secret.insert("sk-super-secret");
        assert_eq!(secret.masked(), "•".repeat(15));
        let debug = format!("{secret:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("sk-super-secret"));
    }

    #[test]
    fn custom_form_defaults_match_shared_custom_model_defaults_symbols() {
        let draft = CustomDraft::empty();
        assert_eq!(draft.brand, "custom");
        assert_eq!(draft.context_window.value, "1000000");
        assert_eq!(draft.max_output_tokens.value, "64000");
        assert!(draft.supports_thinking);
        assert!(!draft.supports_vision);
        assert_eq!(draft.supports_tools, None);
        assert_eq!(draft.supports_json_mode, None);
        assert!(draft.enabled);
    }

    #[test]
    fn custom_form_action_contains_secret_only_at_submission_boundary() {
        let mut draft = CustomDraft::empty();
        draft.base_url = PlainText::new("https://example.invalid/v1");
        draft.model_id = PlainText::new("example-model");
        draft.api_key.insert("top-secret");
        assert!(!format!("{draft:?}").contains("top-secret"));
        let action = draft.action_input(true).expect("valid action");
        assert_eq!(
            action.get("apiKey").and_then(Value::as_str),
            Some("top-secret")
        );
        let effect = ModelManagementEffect::Private {
            action: json!({"kind":"model.custom.add","input":action}),
            purpose: PrivateRuntimePurpose::ModelCustomMutation,
        };
        let debug = format!("{effect:?}");
        assert!(!debug.contains("top-secret"));
        assert!(debug.contains("redacted private runtime payload"));
    }

    #[test]
    fn home_routes_to_each_model_management_surface_without_exiting() {
        for (selected, expected_kind) in [
            (0, None),
            (1, Some("model.local.snapshot")),
            (2, Some("model.account.snapshot")),
        ] {
            let (mut state, _) = ModelManagementState::open();
            state.selected = selected;
            let effects = state.accept(UiLanguage::ZhCn);
            if let Some(expected_kind) = expected_kind {
                let ModelManagementEffect::Private { action, .. } = &effects[0] else {
                    panic!("private action")
                };
                assert_eq!(
                    action.get("kind").and_then(Value::as_str),
                    Some(expected_kind)
                );
            } else {
                assert!(effects.is_empty());
            }
        }
    }

    #[test]
    fn malformed_result_fails_closed_and_retains_modal() {
        let (mut state, _) = ModelManagementState::open();
        state.pending = Some(PendingOperation::CustomLoad);
        let error = state
            .apply_result(
                &PrivateRuntimePurpose::ModelCustomList,
                "model.custom.list",
                &json!({"result":{"kind":"model.custom.list","entries":[{"apiKey":"leak"}]}}),
                UiLanguage::ZhCn,
            )
            .expect_err("malformed payload rejected");
        assert!(!error.is_empty());
        assert!(matches!(state.view, View::Home));
    }

    #[test]
    fn only_explicit_close_effect_closes_the_modal() {
        let (mut state, _) = ModelManagementState::open();
        assert_eq!(state.back(), vec![ModelManagementEffect::Close]);
        state.view = View::LocalList;
        assert!(state.back().is_empty());
        assert!(matches!(state.view, View::Home));
    }
}
