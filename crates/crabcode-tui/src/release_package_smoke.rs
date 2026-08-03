use std::ffi::{OsStr, OsString};

use anyhow::{Context as _, ensure};
use serde_json::{Value, json};

use crate::sdk_projection::{Projection, ProjectionEffect};
use crate::sdk_runtime::{RawEnvelope, classify_envelope};

const ARGUMENT: &str = "__release-package-smoke";
const AUTHORITY_ENV: &str = "CRABCODE_RELEASE_PACKAGE_SMOKE";
const INCIDENT_FIXTURE: &str =
    include_str!("../../../tests/fixtures/renderer/empty-sources-sequence-6034.jsonl");

pub(crate) fn maybe_run(args: impl IntoIterator<Item = OsString>) -> Option<anyhow::Result<()>> {
    let args = args.into_iter().collect::<Vec<_>>();
    if args.get(1).map(OsString::as_os_str) != Some(OsStr::new(ARGUMENT)) {
        return None;
    }
    if args.len() != 2 || std::env::var(AUTHORITY_ENV).as_deref() != Ok("1") {
        return Some(Err(anyhow::anyhow!(
            "release package smoke route requires its exact private authority"
        )));
    }
    Some(run())
}

fn run() -> anyhow::Result<()> {
    let incident = INCIDENT_FIXTURE.trim_end_matches(['\r', '\n']);
    ensure!(
        incident.as_bytes().len() == 63,
        "incident fixture must remain the exact 63-byte envelope"
    );

    let mut projection = Projection::default();
    let effect = ingest(&mut projection, 6034, serde_json::from_str(incident)?)?;
    ensure!(
        effect == ProjectionEffect::None,
        "empty sources incident must be a presentation no-op: {effect:?}"
    );

    for turn in 1..=2_u64 {
        let assistant = json!({
            "type": "assistant",
            "uuid": format!("release-smoke-assistant-{turn}"),
            "session_id": "release-package-smoke",
            "parent_tool_use_id": null,
            "message": {
                "id": format!("release-smoke-message-{turn}"),
                "content": [{"type": "text", "text": format!("turn-{turn}-ok")}]
            }
        });
        ensure!(
            ingest(&mut projection, 6034 + turn * 2 - 1, assistant)? == ProjectionEffect::None,
            "assistant projection failed after incident"
        );
        let result = json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": turn,
            "result": format!("turn-{turn}-ok"),
            "stop_reason": "end_turn",
            "total_cost_usd": 0,
            "usage": {},
            "modelUsage": {},
            "permission_denials": [],
            "session_id": "release-package-smoke",
            "uuid": format!("release-smoke-result-{turn}")
        });
        ensure!(
            matches!(
                ingest(&mut projection, 6034 + turn * 2, result)?,
                ProjectionEffect::TurnCompleted {
                    is_error: false,
                    ..
                }
            ),
            "turn result failed after incident"
        );
    }

    ensure!(
        projection
            .items()
            .iter()
            .any(|item| item.text == "turn-1-ok")
            && projection
                .items()
                .iter()
                .any(|item| item.text == "turn-2-ok"),
        "both post-incident turns must remain visible"
    );
    println!(
        "{}",
        json!({
            "schema_version": 1,
            "incident_sequence": 6034,
            "incident_bytes": 63,
            "incident_disposition": "presentation_noop",
            "turns_completed": 2,
            "runtime_stop": false
        })
    );
    Ok(())
}

fn ingest(
    projection: &mut Projection,
    sequence: u64,
    value: Value,
) -> anyhow::Result<ProjectionEffect> {
    let classification = classify_envelope(&value)
        .map_err(anyhow::Error::new)
        .context("release smoke envelope classification failed")?;
    let encoded_len = serde_json::to_vec(&value)?.len();
    Ok(projection.ingest(RawEnvelope {
        sequence,
        encoded_len,
        value,
        classification,
        correlation: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_incident_replay_keeps_two_successor_turns_alive() {
        run().expect("release package replay");
    }
}
