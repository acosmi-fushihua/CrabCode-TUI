// Copyright (c) 2026 UHMS Team. Licensed under Apache-2.0.
//! Process-in-memory search engine backed by acosmi-segment.
//!
//! This crate wraps the acosmi-segment engine to provide a high-level
//! search API (payload indexing, filtering, optional HNSW semantic search)
//! that can be called in-process via FFI.
//!
//! # Architecture
//!
//! ```text
//! TS (业务层)
//!   └── FFI (C ABI)
//!        └── native TUI memory orchestrator
//!             └── acosmi-memory-se (this crate)
//!                  └── acosmi-segment (search engine core)
//! ```

pub mod indexer;
pub mod segment_store;
pub mod vector_store_adapter;

// Re-export commonly needed engine types for FFI layer.
pub use acosmi_segment::types::{
    Distance, HnswConfig, Payload, PayloadSchemaType, QuantizationConfig, SearchParams,
    VectorStorageDatatype, VectorStorageType,
};
pub use vector_store_adapter::{
    filter_map_to_filter_json, filter_map_to_search_filter, phase1_collection_config,
    SearchEngineVectorStore,
};

/// Parse a JSON object string into a [`Payload`].
///
/// This helper is primarily for FFI callers who cannot construct
/// `Payload` directly.
pub fn payload_from_json_str(
    json_str: &str,
) -> Result<Payload, Box<dyn std::error::Error + Send + Sync>> {
    let val: serde_json::Value = serde_json::from_str(json_str)?;
    let map = val.as_object().ok_or("payload must be a JSON object")?;
    Ok(Payload::from(map.clone()))
}

#[cfg(test)]
mod tests;
