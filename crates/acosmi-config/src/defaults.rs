//! Default configuration value application.
//!
//! Applies sensible defaults to various sections of the Acosmi config.
//! These are intentionally kept as pass-through stubs for now, as the full
//! logic depends on many other modules (model selection, agent limits, etc.).

use acosmi_types::config::CrabClawConfig;

/// Apply session defaults to the configuration.
///
/// Currently a pass-through stub. The full implementation normalizes
/// `session.mainKey` and warns on non-standard values.
#[must_use]
pub const fn apply_session_defaults(cfg: CrabClawConfig) -> CrabClawConfig {
    cfg
}

/// Apply agent defaults to the configuration.
///
/// Currently a pass-through stub. The full implementation sets
/// `agents.defaults.maxConcurrent` and `agents.defaults.subagents.maxConcurrent`
/// when not explicitly configured.
#[must_use]
pub const fn apply_agent_defaults(cfg: CrabClawConfig) -> CrabClawConfig {
    cfg
}

/// Apply model defaults to the configuration.
///
/// Currently a pass-through stub. The full implementation normalizes
/// model definitions (reasoning, input, cost, context window, max tokens)
/// and applies default model aliases.
#[must_use]
pub const fn apply_model_defaults(cfg: CrabClawConfig) -> CrabClawConfig {
    cfg
}

/// Apply message defaults to the configuration.
///
/// Currently a pass-through stub. The full implementation sets
/// `messages.ackReactionScope` to `"group-mentions"` when not specified.
#[must_use]
pub const fn apply_message_defaults(cfg: CrabClawConfig) -> CrabClawConfig {
    cfg
}

/// Apply logging defaults to the configuration.
///
/// Currently a pass-through stub. The full implementation sets
/// `logging.redactSensitive` to `"tools"` when logging is configured
/// but `redactSensitive` is not set.
#[must_use]
pub const fn apply_logging_defaults(cfg: CrabClawConfig) -> CrabClawConfig {
    cfg
}

/// Apply compaction defaults to the configuration.
///
/// Currently a pass-through stub. The full implementation sets
/// `agents.defaults.compaction.mode` to `"safeguard"` when not specified.
#[must_use]
pub const fn apply_compaction_defaults(cfg: CrabClawConfig) -> CrabClawConfig {
    cfg
}

/// Apply context pruning defaults to the configuration.
///
/// Currently a pass-through stub. The full implementation configures
/// context pruning mode, TTL, heartbeat intervals, and cache retention
/// based on the detected authentication mode.
#[must_use]
pub const fn apply_context_pruning_defaults(cfg: CrabClawConfig) -> CrabClawConfig {
    cfg
}
