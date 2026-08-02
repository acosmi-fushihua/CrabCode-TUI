use acosmi_memory_core::session_types::MemoryCategory;

/// Map TS memory type values to Rust memory categories.
///
/// W-MEMORY-LIFECYCLE K2 (2026-07-09): the evolution-artifact types are now
/// first-class citizens of the SE index. Before this, `type: insight`
/// (dream insights, `tier3_auto_dream.rs`), `type: imagined` (promoted
/// imagination drafts), `type: report` (evolution reports,
/// `tier3_imagination.rs`) and `type: knowledge` (personal knowledge-base
/// entries, K9) all fell through to `None` → `InvalidType` skip in the
/// indexer, so the agent's three memory channels (MEMORY.md injection /
/// `memory.search` recall / MemorySearchTool) could never see them. The
/// `MemoryCategory` enum itself is deliberately untouched (FFI legacy +
/// merge semantics stay zero-impact); new types map onto existing variants.
pub fn map_type(ts_type: &str) -> Option<MemoryCategory> {
    match ts_type {
        "user" => Some(MemoryCategory::Profile),
        "feedback" => Some(MemoryCategory::Preferences),
        "project" => Some(MemoryCategory::Events),
        "reference" => Some(MemoryCategory::Entities),
        // K2 — evolution artifacts (dream insights + confirmed imagination
        // drafts are behavioural patterns the agent inferred).
        "insight" => Some(MemoryCategory::Patterns),
        "imagined" => Some(MemoryCategory::Patterns),
        // K2 — evolution reports read like case studies of an evolution cycle.
        "report" => Some(MemoryCategory::Cases),
        // K2/K9 — knowledge-base entries are reference-like entity material.
        "knowledge" => Some(MemoryCategory::Entities),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_legacy_four_types_unchanged() {
        assert_eq!(map_type("user"), Some(MemoryCategory::Profile));
        assert_eq!(map_type("feedback"), Some(MemoryCategory::Preferences));
        assert_eq!(map_type("project"), Some(MemoryCategory::Events));
        assert_eq!(map_type("reference"), Some(MemoryCategory::Entities));
    }

    #[test]
    fn maps_evolution_artifact_types_onto_existing_variants() {
        // K2: dream insights + confirmed imagination drafts → Patterns.
        assert_eq!(map_type("insight"), Some(MemoryCategory::Patterns));
        assert_eq!(map_type("imagined"), Some(MemoryCategory::Patterns));
        // K2: evolution reports → Cases.
        assert_eq!(map_type("report"), Some(MemoryCategory::Cases));
        // K2/K9: knowledge-base entries → Entities.
        assert_eq!(map_type("knowledge"), Some(MemoryCategory::Entities));
    }

    #[test]
    fn unknown_types_still_map_to_none() {
        assert_eq!(map_type("fact"), None);
        assert_eq!(map_type("patterns"), None);
        assert_eq!(
            map_type("Insight"),
            None,
            "type matching stays case-sensitive"
        );
        assert_eq!(map_type(""), None);
    }
}
