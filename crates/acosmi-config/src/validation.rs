//! Configuration validation.
//!
//! Validates a raw JSON value against the `CrabClawConfig` schema.
//! Currently implemented as a simple deserialization stub; full validation
//! with plugin support will be added in a later pass.

use anyhow::{Context, Result};
use serde_json::Value;

use acosmi_types::config::CrabClawConfig;

/// Validate a raw JSON value and deserialize it into an `CrabClawConfig`.
///
/// Currently performs only structural deserialization. The full implementation
/// will include plugin-based validation, field-level checks, and warning
/// collection.
pub fn validate_config_object(raw: &Value) -> Result<CrabClawConfig> {
    serde_json::from_value(raw.clone()).context("Config validation failed: invalid structure")
}
