//! Dependency-closed fixed-source scrollback block implementations.
//!
//! The fixed `RenderBlock` enum and owner lifecycle host the dependency-closed
//! fixed block leaves below. `crabcode_projection` adds typed renderer-only
//! product rows; it does not classify backend payloads or define a wire model.

mod agent;
mod bg_task;
mod btw;
mod context_info;
mod crabcode_projection;
mod credit_limit;
mod local_command_output;
mod markdown_content;
pub mod mermaid_content;
mod quote_bar;
mod session_event;
mod subagent;
mod system;
mod thinking;
pub mod tool;
mod user;
mod workflow;

pub use agent::AgentMessageBlock;
pub use bg_task::{BgTaskBlock, BgTaskKind};
pub use btw::BtwBlock;
pub use context_info::ContextInfoBlock;
pub use crabcode_projection::{
    CrabCodeAdvisorBlock, CrabCodeAdvisorInvocationState, CrabCodeDiagnostic,
    CrabCodeDiagnosticFile, CrabCodeDiagnosticSeverity, CrabCodeDirectApiError,
    CrabCodeDirectAttachmentBlock, CrabCodeDirectFileContent, CrabCodeDirectNestedProgressBlock,
    CrabCodeDirectProgressBlock, CrabCodeDirectSystemBlock, CrabCodeHookPermissionDecision,
    CrabCodeMessageLevel, CrabCodeProjectionBlock, CrabCodeProjectionKind, CrabCodeRelevantMemory,
    CrabCodeSdkImageBlock, CrabCodeSdkImageMediaType, CrabCodeSdkSystemBlock,
    CrabCodeSdkSystemSubtype, CrabCodeSdkSystemTone, CrabCodeSourceNullBlock, CrabCodeTaskStatus,
    CrabCodeToolBlock, CrabCodeToolPayload, CrabCodeToolResultTone,
};
pub use credit_limit::{CreditLimitBlock, CreditLimitCardAction};
pub use local_command_output::LocalCommandOutputBlock;
pub use markdown_content::{MarkdownContent, WrappedLines};
pub use session_event::{SessionEvent, SessionEventBlock};
pub use subagent::{SubagentBlock, SubagentBlockKind};
pub use system::SystemMessageBlock;
pub use thinking::ThinkingBlock;
pub use tool::{
    DiffLineOutput, DiffRenderConfig, DiscoveredTool, EditToolCallBlock, ExecuteToolCallBlock,
    IntegrationSearchToolCallBlock, LineRange, ListDirToolCallBlock, OtherToolCallBlock,
    ReadToolCallBlock, SearchFileMatch, SearchLineMatch, SearchToolCallBlock, ToolCallBlock,
    UseToolCallBlock, discovered_tool_action, render_diff_hunk_highlighted,
    render_diff_hunks_highlighted,
};
pub use user::UserPromptBlock;
pub use workflow::{WorkflowBlock, WorkflowBlockPhase, WorkflowBlockStatus};

/// Compatibility alias retained by the fixed component tree.
pub type EditBlock = EditToolCallBlock;
