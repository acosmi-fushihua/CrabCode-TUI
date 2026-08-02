// Copyright (c) 2026 UHMS Team. Licensed under Apache-2.0.
//! Adapter for TS-produced memory markdown files.
//!
//! This crate is intentionally IO-free. It mirrors the tolerant TS frontmatter
//! parser, maps valid memory types to Rust categories, and exposes helpers the
//! Phase 1 indexer can use before calling `SearchEngine`.

mod category_map;
mod frontmatter;
mod overview;
mod payload_conv;
mod point_id;

use std::fmt;
use std::path::Path;

use acosmi_memory_core::session_types::CandidateMemory;

pub use category_map::map_type;
pub use frontmatter::{frontmatter_ts_compat, TsFrontmatter};
pub use overview::truncate_overview;
pub use payload_conv::fields_to_payload;
pub use point_id::{scoped_path_to_point_id, NAMESPACE_MEMORY};

const OVERVIEW_MAX_CHARS: usize = 200;

/// Parsed representation of a TS-produced memory markdown file.
///
/// `candidate` is present only when the frontmatter `type` maps to a known
/// TS memory type. Missing or invalid type values remain non-fatal here so the
/// indexer can record precise skip reasons.
#[derive(Debug, Clone)]
pub struct ParsedMemory {
    pub candidate: Option<CandidateMemory>,
    pub name_hint: Option<String>,
    pub file_stem: String,
    pub has_frontmatter: bool,
    pub raw_type: Option<String>,
    pub body: String,
    /// W-MEMORY-KB-UPLIFT P0 — knowledge review-gate fields (see
    /// [`TsFrontmatter`]); the indexer skips un-enabled knowledge entries and
    /// stores `injection` in the point payload.
    pub enabled: Option<bool>,
    pub pending_review: Option<bool>,
    pub injection: Option<String>,
}

#[derive(Debug)]
pub enum AdapterError {
    MissingFileStem { path: String },
    NonUtf8FileStem { path: String },
    YamlParse { message: String },
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFileStem { path } => write!(f, "memory path has no file stem: {path}"),
            Self::NonUtf8FileStem { path } => {
                write!(f, "memory path file stem is not valid UTF-8: {path}")
            }
            Self::YamlParse { message } => {
                write!(f, "failed to parse TS memory frontmatter: {message}")
            }
        }
    }
}

impl std::error::Error for AdapterError {}

impl From<serde_yaml::Error> for AdapterError {
    fn from(value: serde_yaml::Error) -> Self {
        Self::YamlParse {
            message: value.to_string(),
        }
    }
}

/// Parse a TS-produced `.md` memory file into indexer-ready data.
pub fn parse_ts_memory(
    file_path: &str,
    content: &str,
    user: &str,
) -> Result<ParsedMemory, AdapterError> {
    let file_stem = file_stem(file_path)?;
    let (frontmatter, body, has_frontmatter) = frontmatter_ts_compat(content)?;
    let raw_type = frontmatter.type_.clone();
    let candidate = raw_type
        .as_deref()
        .and_then(map_type)
        .map(|category| CandidateMemory {
            category,
            abstract_text: frontmatter.description.clone().unwrap_or_default(),
            overview: truncate_overview(&body, OVERVIEW_MAX_CHARS),
            content: body.clone(),
            source_session: String::new(),
            user: user.to_owned(),
            language: "auto".to_owned(),
        });

    Ok(ParsedMemory {
        candidate,
        name_hint: frontmatter.name,
        file_stem,
        has_frontmatter,
        raw_type,
        body,
        enabled: frontmatter.enabled,
        pending_review: frontmatter.pending_review,
        injection: frontmatter.injection,
    })
}

fn file_stem(file_path: &str) -> Result<String, AdapterError> {
    let path = Path::new(file_path);
    let stem = path
        .file_stem()
        .ok_or_else(|| AdapterError::MissingFileStem {
            path: file_path.to_owned(),
        })?;
    let stem = stem.to_str().ok_or_else(|| AdapterError::NonUtf8FileStem {
        path: file_path.to_owned(),
    })?;
    Ok(stem.to_owned())
}
