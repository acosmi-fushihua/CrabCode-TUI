// Copyright (c) 2026 UHMS Team. Licensed under Apache-2.0.
//! Memory extraction — 6-category memory classification from sessions.
//!
//! Ported from `openviking/session/memory_extractor.py`.

use log::{debug, error, info, warn};
use regex::Regex;

use acosmi_memory_core::message::{Message, Role};
use serde::Deserialize;
use uuid::Uuid;

use acosmi_memory_core::context::{Context, Vectorize};
use acosmi_memory_core::session_types::{
    category_dir, CandidateMemory, MemoryCategory, MergedMemoryPayload,
};
use acosmi_memory_core::user::UserIdentifier;

use crate::json_utils::parse_json_from_response;
use crate::prompts;
use crate::traits::{BoxError, FileSystem, LlmProvider};

// ---------------------------------------------------------------------------
// MemoryExtractor
// ---------------------------------------------------------------------------

/// Extracts 6-category candidate memories from session messages via LLM.
pub struct MemoryExtractor<LLM: LlmProvider, FS: FileSystem> {
    llm: LLM,
    fs: FS,
}

/// Internal LLM response schema for extraction.
#[derive(Debug, Deserialize)]
struct LlmExtractResponse {
    #[serde(default)]
    memories: Vec<LlmMemoryItem>,
}

#[derive(Debug, Deserialize)]
struct LlmMemoryItem {
    #[serde(default)]
    category: String,
    #[serde(default, rename = "abstract")]
    abstract_text: String,
    #[serde(default)]
    overview: String,
    #[serde(default)]
    content: String,
}

/// Internal LLM response for memory merge.
#[derive(Debug, Deserialize)]
struct LlmMergeResponse {
    #[serde(default, rename = "abstract")]
    abstract_text: String,
    #[serde(default)]
    overview: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    decision: String,
}

impl<LLM: LlmProvider, FS: FileSystem> MemoryExtractor<LLM, FS> {
    /// Create a new `MemoryExtractor`.
    pub fn new(llm: LLM, fs: FS) -> Self {
        Self { llm, fs }
    }

    /// Extract candidate memories from messages via LLM.
    pub async fn extract(
        &self,
        messages: &[Message],
        user: &UserIdentifier,
        session_id: &str,
    ) -> Result<Vec<CandidateMemory>, BoxError> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }

        let formatted: String = messages
            .iter()
            .filter(|m| !m.content().is_empty())
            .map(|m| format!("[{:?}]: {}", m.role, m.content()))
            .collect::<Vec<_>>()
            .join("\n");

        if formatted.is_empty() {
            return Ok(Vec::new());
        }

        let output_language =
            Self::detect_output_language(messages, user.language.as_deref().unwrap_or("en"));

        let prompt = prompts::apply(
            prompts::MEMORY_EXTRACT,
            &[
                ("user_id", &user.user_id),
                ("output_language", &output_language),
                ("messages", &formatted),
            ],
        );

        debug!("Memory extraction LLM request len={}", formatted.len());
        let response = self.llm.completion(&prompt).await?;
        debug!("Memory extraction LLM response len={}", response.len());

        let data: LlmExtractResponse =
            parse_json_from_response(&response).unwrap_or(LlmExtractResponse {
                memories: Vec::new(),
            });

        let candidates = data
            .memories
            .into_iter()
            .map(|mem| {
                let category = match mem.category.as_str() {
                    "profile" => MemoryCategory::Profile,
                    "preferences" => MemoryCategory::Preferences,
                    "entities" => MemoryCategory::Entities,
                    "events" => MemoryCategory::Events,
                    "cases" => MemoryCategory::Cases,
                    _ => MemoryCategory::Patterns,
                };
                CandidateMemory {
                    category,
                    abstract_text: mem.abstract_text,
                    overview: mem.overview,
                    content: mem.content,
                    source_session: session_id.to_owned(),
                    user: user.user_id.clone(),
                    language: output_language.clone(),
                }
            })
            .collect::<Vec<_>>();

        info!(
            "Extracted {} candidate memories (lang={})",
            candidates.len(),
            output_language
        );
        Ok(candidates)
    }

    /// Create a `Context` from a candidate and persist to FS.
    pub async fn create_memory(
        &self,
        candidate: &CandidateMemory,
        user: &UserIdentifier,
        session_id: &str,
    ) -> Result<Option<Context>, BoxError> {
        // Profile: special merge handling
        if candidate.category == MemoryCategory::Profile {
            let payload = self.append_to_profile(candidate).await?;
            if let Some(p) = payload {
                let mut ctx =
                    Context::new("viking://user/memories/profile.md", p.abstract_text.clone())
                        .with_parent("viking://user/memories")
                        .as_leaf();
                ctx.session_id = Some(session_id.to_owned());
                ctx.user = Some(user.clone());
                ctx.set_vectorize(Vectorize::new(p.content));
                return Ok(Some(ctx));
            }
            return Ok(None);
        }

        // Determine parent URI
        let dir = category_dir(candidate.category);
        let parent_uri = match candidate.category {
            MemoryCategory::Cases | MemoryCategory::Patterns => {
                format!("viking://agent/{dir}")
            }
            _ => format!("viking://user/{dir}"),
        };

        let memory_id = format!("mem_{}", Uuid::new_v4());
        let memory_uri = format!("{parent_uri}/{memory_id}.md");

        if let Err(e) = self.fs.write(&memory_uri, &candidate.content).await {
            error!("Failed to write memory to FS: {e}");
            return Ok(None);
        }
        info!("Created memory file: {memory_uri}");

        let mut ctx = Context::new(&memory_uri, &candidate.abstract_text)
            .with_parent(&parent_uri)
            .as_leaf();
        ctx.session_id = Some(session_id.to_owned());
        ctx.user = Some(user.clone());
        ctx.set_vectorize(Vectorize::new(&candidate.content));

        Ok(Some(ctx))
    }

    /// Merge memory bundle via LLM (used by compressor for merge operations).
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_memory_bundle(
        &self,
        existing_abstract: &str,
        existing_overview: &str,
        existing_content: &str,
        new_abstract: &str,
        new_overview: &str,
        new_content: &str,
        category: &str,
        output_language: &str,
    ) -> Result<Option<MergedMemoryPayload>, BoxError> {
        let prompt = prompts::apply(
            prompts::MEMORY_MERGE,
            &[
                ("category", category),
                ("output_language", output_language),
                ("existing_abstract", existing_abstract),
                ("existing_overview", existing_overview),
                ("existing_content", existing_content),
                ("new_abstract", new_abstract),
                ("new_overview", new_overview),
                ("new_content", new_content),
            ],
        );

        let response = self.llm.completion(&prompt).await?;
        let data: LlmMergeResponse = match parse_json_from_response(&response) {
            Some(d) => d,
            None => {
                error!("Memory merge bundle parse failed");
                return Ok(None);
            }
        };

        if !data.decision.is_empty() && data.decision.to_lowercase() != "merge" {
            error!("Memory merge bundle invalid decision={}", data.decision);
            return Ok(None);
        }
        if data.abstract_text.trim().is_empty() || data.content.trim().is_empty() {
            error!("Memory merge bundle missing required fields");
            return Ok(None);
        }

        Ok(Some(MergedMemoryPayload {
            abstract_text: data.abstract_text,
            overview: data.overview,
            content: data.content,
            reason: data.reason,
        }))
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    async fn append_to_profile(
        &self,
        candidate: &CandidateMemory,
    ) -> Result<Option<MergedMemoryPayload>, BoxError> {
        let uri = "viking://user/memories/profile.md";
        // Step 2 Phase D.7 — closes Step 1 §六 R1 ④ / HIGH-extractor.rs:259:
        // the previous code used `unwrap_or_default()`, treating *any* read
        // failure (NotFound, permission denied, transient IO, vfs network
        // blip) as if the profile were empty, then taking the "create"
        // path which **overwrites the entire existing profile** with the
        // new candidate. Real-world impact: a one-time IO blip during
        // profile read drops every memory the user has accumulated.
        //
        // After this fix:
        // - NotFound -> existing == None -> create branch (correct).
        // - Empty content (file exists, was empty) -> create branch.
        // - Real IO error -> propagate via `?`, caller decides; the
        //   profile is never overwritten on uncertain reads.
        let existing: Option<String> = match self.fs.read(uri).await {
            Ok(content) => Some(content),
            Err(ref e) if crate::traits::is_not_found_error(e) => None,
            Err(e) => return Err(e),
        };

        let needs_create = match existing.as_deref() {
            None => true,
            Some(s) if s.trim().is_empty() => true,
            Some(_) => false,
        };

        if needs_create {
            self.fs.write(uri, &candidate.content).await?;
            info!("Created profile at {uri}");
            return Ok(Some(MergedMemoryPayload {
                abstract_text: candidate.abstract_text.clone(),
                overview: candidate.overview.clone(),
                content: candidate.content.clone(),
                reason: "created".to_owned(),
            }));
        }

        // unwrap is safe here: the only path that reaches this point has
        // `existing == Some(non_empty)` (the `needs_create` branch above
        // covers None and Some(empty)). Use `expect` rather than `unwrap`
        // so a future logic change here surfaces a clear panic message.
        let existing_text =
            existing.expect("non-empty existing profile must be Some after needs_create branch");

        let payload = self
            .merge_memory_bundle(
                "",
                "",
                &existing_text,
                &candidate.abstract_text,
                &candidate.overview,
                &candidate.content,
                "profile",
                &candidate.language,
            )
            .await?;

        if let Some(ref p) = payload {
            self.fs.write(uri, &p.content).await?;
            info!("Merged profile info to {uri}");
        } else {
            warn!("Profile merge failed; keeping existing profile unchanged");
        }
        Ok(payload)
    }

    /// Detect dominant language from user messages (pure function).
    pub fn detect_output_language(messages: &[Message], fallback: &str) -> String {
        let user_text: String = messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content().to_owned())
            .collect::<Vec<_>>()
            .join("\n");

        if user_text.is_empty() {
            return fallback.to_owned();
        }

        let ko = Regex::new(r"[\uac00-\ud7af]").unwrap();
        let ru = Regex::new(r"[\u0400-\u04ff]").unwrap();
        let ar = Regex::new(r"[\u0600-\u06ff]").unwrap();

        let counts = [
            ("ko", ko.find_iter(&user_text).count()),
            ("ru", ru.find_iter(&user_text).count()),
            ("ar", ar.find_iter(&user_text).count()),
        ];
        if let Some((lang, score)) = counts.iter().max_by_key(|c| c.1) {
            if *score > 0 {
                return lang.to_string();
            }
        }

        let kana = Regex::new(r"[\u3040-\u30ff\u31f0-\u31ff\uff66-\uff9f]").unwrap();
        let han = Regex::new(r"[\u4e00-\u9fff]").unwrap();

        if kana.find(&user_text).is_some() {
            return "ja".to_owned();
        }
        if han.find(&user_text).is_some() {
            return "zh-CN".to_owned();
        }

        fallback.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::BoxError;
    use async_trait::async_trait;

    struct MockLlm {
        response: String,
    }
    #[async_trait]
    impl LlmProvider for MockLlm {
        async fn completion(&self, _prompt: &str) -> Result<String, BoxError> {
            Ok(self.response.clone())
        }
    }

    struct MockFs;
    #[async_trait]
    impl FileSystem for MockFs {
        async fn read(&self, _: &str) -> Result<String, BoxError> {
            Ok(String::new())
        }
        async fn read_bytes(&self, _: &str) -> Result<Vec<u8>, BoxError> {
            Ok(Vec::new())
        }
        async fn write(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn write_bytes(&self, _: &str, _: &[u8]) -> Result<(), BoxError> {
            Ok(())
        }
        async fn mkdir(&self, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn ls(&self, _: &str) -> Result<Vec<crate::traits::FsEntry>, BoxError> {
            Ok(Vec::new())
        }
        async fn rm(&self, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn mv(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn stat(&self, _: &str) -> Result<crate::traits::FsStat, BoxError> {
            Err("not implemented".into())
        }
        async fn grep(
            &self,
            _: &str,
            _: &str,
            _: bool,
            _: bool,
        ) -> Result<Vec<crate::traits::GrepMatch>, BoxError> {
            Ok(Vec::new())
        }
        async fn exists(&self, _: &str) -> Result<bool, BoxError> {
            Ok(false)
        }
        async fn append(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn link(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
    }

    #[test]
    fn detect_lang_fallback() {
        let msgs: Vec<Message> = vec![];
        assert_eq!(
            MemoryExtractor::<MockLlm, MockFs>::detect_output_language(&msgs, "en"),
            "en"
        );
    }

    #[tokio::test]
    async fn extract_empty_messages() {
        let ext = MemoryExtractor::new(
            MockLlm {
                response: String::new(),
            },
            MockFs,
        );
        let user = UserIdentifier::new("acme", "test", "agent").unwrap();
        let result = ext.extract(&[], &user, "s1").await.unwrap();
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Step 2 Phase D.7 / Step 1 §六 R1 ④ / HIGH-extractor.rs:259 regression:
    // before the fix, `append_to_profile` did `fs.read(uri).await.unwrap_or_default()`
    // which silently turned any read failure into the empty-string fallback,
    // then took the "create" branch and overwrote the entire existing profile.
    // After the fix, only NotFound (or genuine empty content) takes "create";
    // any other Err propagates without writing.
    // -----------------------------------------------------------------------

    /// Configurable FileSystem mock for the extractor regressions.
    /// State lives behind `Arc<Mutex<...>>` so the test can hand a
    /// `clone()` to `MemoryExtractor::new` (consumes the FS) and keep
    /// another clone to read back recorded writes.
    #[derive(Clone)]
    struct ProfileFsMock {
        /// Result the next `read` call should return. `None` after
        /// drained; subsequent reads return a synthetic error to make
        /// double-read regressions visible.
        next_read: std::sync::Arc<std::sync::Mutex<Option<Result<String, BoxError>>>>,
        /// Records every `write(uri, content)` invocation so tests can
        /// assert "write was called" / "write was not called".
        writes: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    impl ProfileFsMock {
        fn new(read_result: Result<String, BoxError>) -> Self {
            Self {
                next_read: std::sync::Arc::new(std::sync::Mutex::new(Some(read_result))),
                writes: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn write_log(&self) -> Vec<(String, String)> {
            self.writes.lock().expect("test mutex").clone()
        }
    }

    #[async_trait]
    impl FileSystem for ProfileFsMock {
        async fn read(&self, _uri: &str) -> Result<String, BoxError> {
            let mut slot = self.next_read.lock().expect("test mutex");
            slot.take()
                .unwrap_or_else(|| Err("test: next_read drained twice".into()))
        }
        async fn read_bytes(&self, _: &str) -> Result<Vec<u8>, BoxError> {
            Err("not used in this regression".into())
        }
        async fn write(&self, uri: &str, content: &str) -> Result<(), BoxError> {
            self.writes
                .lock()
                .expect("test mutex")
                .push((uri.to_owned(), content.to_owned()));
            Ok(())
        }
        async fn write_bytes(&self, _: &str, _: &[u8]) -> Result<(), BoxError> {
            Ok(())
        }
        async fn mkdir(&self, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn ls(&self, _: &str) -> Result<Vec<crate::traits::FsEntry>, BoxError> {
            Ok(Vec::new())
        }
        async fn rm(&self, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn mv(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn stat(&self, _: &str) -> Result<crate::traits::FsStat, BoxError> {
            Err("not implemented".into())
        }
        async fn grep(
            &self,
            _: &str,
            _: &str,
            _: bool,
            _: bool,
        ) -> Result<Vec<crate::traits::GrepMatch>, BoxError> {
            Ok(Vec::new())
        }
        async fn exists(&self, _: &str) -> Result<bool, BoxError> {
            Ok(false)
        }
        async fn append(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
        async fn link(&self, _: &str, _: &str) -> Result<(), BoxError> {
            Ok(())
        }
    }

    fn make_candidate() -> CandidateMemory {
        CandidateMemory {
            category: acosmi_memory_core::session_types::MemoryCategory::Profile,
            abstract_text: "abstract".into(),
            overview: "overview".into(),
            content: "new candidate body".into(),
            source_session: "test-session".into(),
            user: "test-user".into(),
            language: "en".into(),
        }
    }

    /// A real IO error on read **must not** trigger the create branch; the
    /// existing profile is unknown and must not be overwritten. The fix
    /// propagates the Err.
    #[tokio::test]
    async fn profile_overwrite_blocked_on_real_io_error() {
        let io_err: BoxError = Box::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "simulated permission denied",
        ));
        let fs = ProfileFsMock::new(Err(io_err));
        let fs_for_check = fs.clone();
        let ext = MemoryExtractor::new(
            MockLlm {
                response: String::new(),
            },
            fs,
        );

        let candidate = make_candidate();
        let result = ext.append_to_profile(&candidate).await;

        assert!(
            result.is_err(),
            "real IO error must propagate, not silently overwrite"
        );
        assert!(
            fs_for_check.write_log().is_empty(),
            "no write must occur on uncertain read — would overwrite user profile",
        );
    }

    /// NotFound is the **only** condition that should trigger the create
    /// branch (alongside genuine empty content). The fix uses
    /// `is_not_found_error` to gate this.
    #[tokio::test]
    async fn profile_create_on_not_found_only() {
        let not_found: BoxError = Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "simulated not found",
        ));
        let fs = ProfileFsMock::new(Err(not_found));
        let fs_for_check = fs.clone();
        let ext = MemoryExtractor::new(
            MockLlm {
                response: String::new(),
            },
            fs,
        );

        let candidate = make_candidate();
        let payload = ext
            .append_to_profile(&candidate)
            .await
            .expect("not found must take the create branch successfully");

        let payload = payload.expect("create branch returns Some(payload)");
        assert_eq!(payload.reason, "created");

        let writes = fs_for_check.write_log();
        assert_eq!(writes.len(), 1, "exactly one write on create branch");
        assert_eq!(writes[0].0, "viking://user/memories/profile.md");
        assert_eq!(writes[0].1, "new candidate body");
    }

    /// Empty existing content (file present but empty) also takes the
    /// create branch — preserving the prior behaviour for that case.
    #[tokio::test]
    async fn profile_create_on_empty_existing() {
        let fs = ProfileFsMock::new(Ok(String::from("   \n  ")));
        let fs_for_check = fs.clone();
        let ext = MemoryExtractor::new(
            MockLlm {
                response: String::new(),
            },
            fs,
        );

        let candidate = make_candidate();
        let payload = ext
            .append_to_profile(&candidate)
            .await
            .expect("empty existing must take create branch");
        assert_eq!(payload.unwrap().reason, "created");
        assert_eq!(fs_for_check.write_log().len(), 1);
    }
}
