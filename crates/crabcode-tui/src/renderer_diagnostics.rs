//! Bounded, privacy-conscious renderer diagnostics.
//!
//! Metadata is recorded by default so a presentation failure can be replayed
//! without preserving prompts or tool output. A redacted raw-event journal is
//! available only when `CRABCODE_TUI_RAW_EVENT_DUMP=1` is set. Diagnostic I/O
//! is deliberately fail-soft: it must never become a second runtime failure.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value, json};

use crate::sdk_runtime::RawEnvelope;

const DIAGNOSTIC_DIR_ENV: &str = "CRABCODE_TUI_DIAGNOSTIC_DIR";
const RAW_EVENT_DUMP_ENV: &str = "CRABCODE_TUI_RAW_EVENT_DUMP";
const METADATA_FILE: &str = "tui-renderer-metadata.jsonl";
const RAW_FILE: &str = "tui-renderer-raw-ring.jsonl";
const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_METADATA_STRING_BYTES: usize = 512;

static DIAGNOSTIC_SINK: OnceLock<Option<Mutex<DiagnosticSink>>> = OnceLock::new();

pub(crate) fn record_envelope(
    envelope: &RawEnvelope,
    turn_generation: u64,
    block_generation: Option<u64>,
    compatibility: &[String],
) {
    let Some(sink) = DIAGNOSTIC_SINK
        .get_or_init(|| DiagnosticSink::from_environment().ok().map(Mutex::new))
        .as_ref()
    else {
        return;
    };
    let Ok(mut sink) = sink.lock() else {
        return;
    };
    let _ = sink.record(envelope, turn_generation, block_generation, compatibility);
}

struct DiagnosticSink {
    directory: PathBuf,
    raw_enabled: bool,
    max_bytes: u64,
}

impl DiagnosticSink {
    fn from_environment() -> io::Result<Self> {
        let directory = std::env::var_os(DIAGNOSTIC_DIR_ENV)
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".crabcode").join("debug")))
            .ok_or_else(|| io::Error::other("renderer diagnostic directory is unavailable"))?;
        Self::new(
            directory,
            std::env::var(RAW_EVENT_DUMP_ENV).as_deref() == Ok("1"),
            MAX_JOURNAL_BYTES,
        )
    }

    fn new(directory: PathBuf, raw_enabled: bool, max_bytes: u64) -> io::Result<Self> {
        prepare_private_directory(&directory)?;
        Ok(Self {
            directory,
            raw_enabled,
            max_bytes,
        })
    }

    fn record(
        &mut self,
        envelope: &RawEnvelope,
        turn_generation: u64,
        block_generation: Option<u64>,
        compatibility: &[String],
    ) -> io::Result<()> {
        let metadata = metadata_record(envelope, turn_generation, block_generation, compatibility);
        append_bounded_json_line(
            &self.directory.join(METADATA_FILE),
            &metadata,
            self.max_bytes,
        )?;

        if self.raw_enabled {
            let raw = json!({
                "sequence": envelope.sequence,
                "classification": format!("{:?}", envelope.classification),
                "value": redact_value(&envelope.value),
            });
            append_bounded_json_line(&self.directory.join(RAW_FILE), &raw, self.max_bytes)?;
        }
        Ok(())
    }
}

fn metadata_record(
    envelope: &RawEnvelope,
    turn_generation: u64,
    block_generation: Option<u64>,
    compatibility: &[String],
) -> Value {
    let value = &envelope.value;
    let event = value.get("event");
    let message_id = event
        .and_then(|event| string_pointer(event, "/message/id"))
        .or_else(|| event.and_then(|event| string_pointer(event, "/message_id")))
        .or_else(|| string_pointer(value, "/message/id"));
    let source_index = event
        .and_then(|event| event.get("index"))
        .and_then(Value::as_u64);
    let tool_use_id = first_string(
        value,
        &[
            "/tool_use_id",
            "/toolUseId",
            "/message/content/0/id",
            "/attachment/tool_use_id",
        ],
    );
    let tool_name = first_string(
        value,
        &[
            "/tool_name",
            "/toolName",
            "/message/content/0/name",
            "/attachment/tool_name",
        ],
    );
    let parent = first_string(value, &["/parent_tool_use_id", "/parentToolUseId"]);
    let subtype = first_string(value, &["/subtype", "/message/subtype"]);
    let event_type = event.and_then(|event| string_pointer(event, "/type"));
    let context = if value.get("session_id").is_some() {
        "sdk"
    } else if event.is_some() {
        "direct_query"
    } else {
        "structured_io"
    };

    json!({
        "sequence": envelope.sequence,
        "encoded_len": envelope.encoded_len,
        "type": safe_string(value.get("type").and_then(Value::as_str)),
        "subtype": safe_string(subtype),
        "event_type": safe_string(event_type),
        "message_id": safe_string(message_id),
        "source_index": source_index,
        "turn_generation": turn_generation,
        "block_generation": block_generation,
        "tool_use_id": safe_string(tool_use_id),
        "tool_name": safe_string(tool_name),
        "parent_tool_use_id": safe_string(parent),
        "session_id": safe_string(value.get("session_id").and_then(Value::as_str)),
        "context": context,
        "compatibility": compatibility
            .iter()
            .map(|entry| sanitize_metadata_string(entry))
            .collect::<Vec<_>>(),
    })
}

fn first_string<'a>(value: &'a Value, pointers: &[&str]) -> Option<&'a str> {
    pointers
        .iter()
        .find_map(|pointer| string_pointer(value, pointer))
}

fn string_pointer<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn safe_string(value: Option<&str>) -> Option<String> {
    value.map(sanitize_metadata_string)
}

fn sanitize_metadata_string(value: &str) -> String {
    let redacted = redact_string(value);
    if redacted.len() <= MAX_METADATA_STRING_BYTES {
        return redacted;
    }
    let mut boundary = MAX_METADATA_STRING_BYTES;
    while !redacted.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &redacted[..boundary])
}

fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let value = if sensitive_key(key) {
                        Value::String("<redacted>".to_string())
                    } else {
                        redact_value(value)
                    };
                    (key.clone(), value)
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_value).collect()),
        Value::String(value) => Value::String(redact_string(value)),
        other => other.clone(),
    }
}

fn sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "authorization",
        "accesstoken",
        "refreshtoken",
        "apikey",
        "password",
        "passwd",
        "secret",
        "cookie",
        "sessiontoken",
        "privatekey",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn redact_string(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("bearer ")
        || lower.starts_with("basic ")
        || lower.starts_with("sk-")
        || lower.starts_with("xai-")
        || lower.starts_with("ghp_")
        || lower.starts_with("github_pat_")
    {
        "<redacted>".to_string()
    } else {
        value.to_string()
    }
}

fn prepare_private_directory(path: &Path) -> io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(io::Error::other(
            "renderer diagnostic path must be a real directory",
        ));
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn append_bounded_json_line(path: &Path, value: &Value, max_bytes: u64) -> io::Result<()> {
    reject_unsafe_file(path)?;
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > max_bytes {
        return Ok(());
    }

    let should_roll = fs::metadata(path)
        .map(|metadata| metadata.len().saturating_add(bytes.len() as u64) > max_bytes)
        .unwrap_or(false);
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if should_roll {
        options.truncate(true);
    } else {
        options.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    enforce_private_file(&file)?;
    file.write_all(&bytes)?;
    file.flush()
}

fn reject_unsafe_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            io::Error::other("renderer diagnostic journal must be a regular file"),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn enforce_private_file(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;
    use crate::sdk_runtime::EnvelopeClass;

    fn envelope(sequence: u64, value: Value) -> RawEnvelope {
        RawEnvelope {
            sequence,
            encoded_len: serde_json::to_vec(&value).unwrap().len(),
            value,
            classification: EnvelopeClass::StreamEvent {
                event_type: Some("content_block_start".to_string()),
            },
            correlation: None,
        }
    }

    #[test]
    fn metadata_is_an_allowlist_and_excludes_prompt_content() {
        let raw = envelope(
            12,
            json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 2,
                    "message": {"id": "message-1"},
                    "content_block": {"type": "text", "text": "private prompt"}
                },
                "api_key": "sk-secret"
            }),
        );
        let encoded = serde_json::to_string(&metadata_record(&raw, 4, Some(1), &[])).unwrap();
        assert!(encoded.contains("message-1"));
        assert!(encoded.contains("block_generation"));
        assert!(!encoded.contains("private prompt"));
        assert!(!encoded.contains("sk-secret"));
    }

    #[test]
    fn raw_dump_redacts_secret_keys_and_token_shaped_values() {
        let redacted = redact_value(&json!({
            "authorization": "Bearer secret",
            "nested": {"apiKey": "sk-secret"},
            "text": "Bearer another-secret",
            "ordinary": "visible"
        }));
        let encoded = serde_json::to_string(&redacted).unwrap();
        assert!(!encoded.contains("secret"));
        assert!(encoded.contains("visible"));
    }

    #[test]
    fn journals_are_private_and_bounded() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("diagnostics");
        let mut sink = DiagnosticSink::new(directory.clone(), true, 700).unwrap();
        for sequence in 0..40 {
            sink.record(
                &envelope(
                    sequence,
                    json!({
                        "type": "stream_event",
                        "event": {"type": "ping"},
                        "token": "sk-secret"
                    }),
                ),
                3,
                None,
                &[],
            )
            .unwrap();
        }
        for file_name in [METADATA_FILE, RAW_FILE] {
            let path = directory.join(file_name);
            let metadata = fs::metadata(&path).unwrap();
            assert!(metadata.len() <= 700);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            }
        }
    }
}
