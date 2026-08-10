//! Root renderer view for CrabCode's direct terminal lifecycle.
//!
//! The event/terminal layer owns one persistent [`AppView`]. Product chrome is
//! delegated to `tui_ui`, while the only transcript callback is
//! [`AgentView::draw`]. This is the fixed upstream ownership direction:
//! terminal lifecycle → `AppView` → `AgentView` → `ScrollbackPane`.
//!
//! Fixed-source anchors:
//! - repository commit: `a5727c5960452e7527a154b25cb5bf00cda0545e`
//! - monorepo source revision: `30192d2eef5d91a8fff0e53957de5bd05b43398c`
//! - `AppView::draw` / `AppView::draw_inner` (the persistent root-view owner
//!   and sole per-frame dispatch into the active view).
//!
//! This file adapts that ownership spine; it is not a claim that the fixed
//! source's multi-agent dashboard, authentication welcome, cloud, ACP,
//! announcement, voice, or backend session branches were transplanted.
//! CrabCode already owns those product/backend decisions in its unchanged
//! direct StructuredIO runtime. Only their existing product chrome remains
//! outside the transcript callback.

use std::time::{Duration, Instant};

use crabcode_pager_render::scrollback::ScratchBuffer;
use crabcode_ratatui_inline::Terminal;
use crossterm::event::Event;
use ratatui::Frame;
use ratatui::backend::Backend;

use crate::agent_view::{AgentView, TranscriptViewAction};
use crate::app_event_loop::PasteProvenance;
use crate::scrollback_projection::ProjectionScrollbackError;
use crate::tui_app::{TerminalEventOutcome, TuiApp};
use crate::tui_ui::RenderOutcome;

/// Fixed slow cadence used only by low-frequency renderer-local polling.
///
/// This matches the pinned upstream `AppView` cadence and keeps a resting
/// pointer out of the ordinary fast animation path.
pub(crate) const SLOW_TICK_INTERVAL: Duration = Duration::from_millis(83);

pub(crate) struct AppView {
    agent: AgentView,
    scratch: ScratchBuffer,
}

impl Default for AppView {
    fn default() -> Self {
        Self::new()
    }
}

impl AppView {
    pub(crate) fn new() -> Self {
        Self {
            agent: AgentView::new(),
            scratch: ScratchBuffer::new(),
        }
    }

    /// Route one timestamped terminal event through the persistent root view.
    ///
    /// `TuiApp` remains CrabCode's existing product/backend adapter. This
    /// method restores the fixed root-view ownership direction without moving
    /// any request, session, command, or protocol authority into the
    /// renderer.
    pub(crate) fn handle_input(
        &mut self,
        app: &mut TuiApp,
        event: Event,
        arrived_at: Instant,
        paste_provenance: PasteProvenance,
    ) -> TerminalEventOutcome {
        app.handle_event_at_with_paste_provenance(event, arrived_at, paste_provenance)
    }

    /// Drain semantic renderer input and atomically synchronize the product
    /// projection before viewport resize or native-scrollback insertion.
    pub(crate) fn prepare(&mut self, app: &mut TuiApp) -> Result<(), ProjectionScrollbackError> {
        if app.session_picker_active() {
            return Ok(());
        }
        for action in app.take_transcript_view_actions() {
            self.agent.enqueue(action);
        }
        if let Some(matched) = app.take_transcript_search_reveal() {
            self.agent.enqueue(TranscriptViewAction::RevealMatch {
                key: matched.key,
                line_in_entry: matched.line_in_item,
            });
        }
        self.agent.prepare(app)
    }

    pub(crate) fn minimal_viewport_height(
        &self,
        app: &TuiApp,
        terminal_width: u16,
        terminal_height: u16,
    ) -> u16 {
        if app.session_picker_active() {
            return terminal_height.max(1);
        }
        // Minimal committed blocks and the live tail both paint at the inline
        // viewport's full width, so wrapping cannot change at the frontier.
        let content_width = terminal_width.max(1);
        let tail_rows = self.agent.minimal_tail_rows(content_width, app.busy());
        let panel_rows = self.agent.minimal_side_question_rows(content_width);
        let transcript_rows = tail_rows.max(4).saturating_add(panel_rows);
        crate::tui_ui::minimal_viewport_height_for_transcript_rows(
            app,
            terminal_width,
            terminal_height,
            transcript_rows,
        )
    }

    pub(crate) fn minimal_will_commit(&self, app: &TuiApp) -> bool {
        self.agent.minimal_will_commit(app.busy())
    }

    pub(crate) fn commit_minimal<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &TuiApp,
        hold_native_commits: bool,
    ) -> std::io::Result<bool> {
        self.agent
            .commit_minimal(terminal, app.busy(), hold_native_commits)
    }

    /// Return the next renderer-owned low-frequency deadline.
    ///
    /// Only macOS needs an OS modifier poll; other platforms receive Control
    /// transitions through terminal input and keep this clock parked.
    pub(crate) fn renderer_animation_deadline(&mut self, now: Instant) -> Option<Instant> {
        self.agent
            .renderer_animation_deadline(now, SLOW_TICK_INTERVAL)
    }

    /// Advance one due renderer-owned low-frequency poll.
    pub(crate) fn tick_renderer_animation(&mut self, now: Instant) -> bool {
        self.agent.tick_renderer_animation(now, SLOW_TICK_INTERVAL)
    }

    pub(crate) fn retire_input_side_channels_for_terminal_generation(&mut self) {
        self.agent
            .retire_input_side_channels_for_terminal_generation();
    }

    pub(crate) fn draw_prepared(
        &mut self,
        frame: &mut Frame<'_>,
        app: &mut TuiApp,
    ) -> Result<RenderOutcome, ProjectionScrollbackError> {
        if app.session_picker_active() {
            app.render_session_picker(frame);
            return Ok(RenderOutcome {
                cursor: None,
                hyperlinks: Vec::new(),
            });
        }
        let Self { agent, scratch } = self;
        crate::tui_ui::render_with_transcript(frame, app, |frame, area, app, theme| {
            Ok::<_, ProjectionScrollbackError>(
                agent.draw_prepared(frame, area, app, theme, scratch),
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        app: &mut TuiApp,
    ) -> Result<RenderOutcome, ProjectionScrollbackError> {
        self.prepare(app)?;
        self.draw_prepared(frame, app)
    }

    #[cfg(test)]
    pub(crate) fn agent(&self) -> &AgentView {
        &self.agent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sdk_runtime::{RawEnvelope, RuntimeEvent};
    use crate::tui_app::{InitialSessionRequest, OutboundPurpose};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::{Value, json};

    fn ingest_json(
        app: &mut TuiApp,
        sequence: u64,
        value: Value,
    ) -> Vec<crate::tui_app::HostAction> {
        let classification =
            crate::sdk_runtime::classify_envelope(&value).expect("classify legal runtime event");
        let encoded_len = serde_json::to_vec(&value)
            .expect("encode legal runtime event")
            .len();
        app.handle_runtime_event(RuntimeEvent::Envelope(RawEnvelope {
            sequence,
            encoded_len,
            value,
            classification,
            correlation: None,
        }))
    }

    fn scrollback_searchable(view: &AppView) -> String {
        (0..view.agent().scrollback().len())
            .filter_map(|index| {
                view.agent()
                    .scrollback()
                    .entry(index)?
                    .block
                    .searchable_text()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn draw_prepared_buffer(
        view: &mut AppView,
        app: &mut TuiApp,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                view.draw_prepared(frame, app)
                    .expect("prepared task-history draw");
            })
            .expect("task-history draw");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_presentable(app: &mut TuiApp, view: &mut AppView, label: &str) {
        view.prepare(app)
            .unwrap_or_else(|error| panic!("{label} failed during AppView::prepare: {error}"));
        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                view.draw_prepared(frame, app).unwrap_or_else(|error| {
                    panic!("{label} failed during AppView::draw_prepared: {error}")
                });
            })
            .unwrap_or_else(|error| panic!("{label} failed during terminal draw: {error}"));
        assert!(!app.should_quit, "{label} must not request process exit");
        assert!(
            app.fatal.is_none(),
            "{label} manufactured a renderer fatal: {:?}",
            app.fatal
        );
    }

    fn successful_control_response(request_id: &str, body: Option<Value>) -> Value {
        let mut response = json!({
            "subtype": "success",
            "request_id": request_id
        });
        if let Some(body) = body {
            response["response"] = body;
        }
        json!({
            "type": "control_response",
            "response": response
        })
    }

    fn assert_test_only_declaration(source: &str, marker: &str) {
        let declaration = source
            .find(marker)
            .unwrap_or_else(|| panic!("missing legacy declaration marker: {marker}"));
        let paragraph_start = source[..declaration]
            .rfind("\n\n")
            .map_or(0, |index| index + 2);
        assert!(
            source[paragraph_start..declaration].contains("#[cfg(test)]"),
            "{marker} must not enter the production compilation closure"
        );
    }

    #[test]
    fn empty_frame_uses_the_single_app_to_agent_callback_chain() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();

        terminal
            .draw(|frame| {
                let output = view.draw(frame, &mut app).expect("empty projection closes");
                assert!(output.hyperlinks.is_empty());
            })
            .expect("draw");

        assert!(view.agent().scrollback().is_empty());
        let painted = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(painted.contains("CrabCode"), "{painted}");
        assert!(painted.replace(' ', "").contains("快速开始"), "{painted}");
    }

    #[test]
    fn empty_welcome_is_compact_and_responsive_at_supported_terminal_sizes() {
        for (width, height) in [(120_u16, 30_u16), (80, 24), (60, 20)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("responsive welcome terminal");
            let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
            app.release_startup_barrier_for_test();
            let mut view = AppView::new();

            terminal
                .draw(|frame| {
                    view.draw(frame, &mut app)
                        .expect("responsive empty welcome must render");
                })
                .expect("responsive welcome draw");

            let buffer = terminal.backend().buffer();
            let rows = buffer
                .content()
                .chunks(usize::from(width))
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>();
            let compact = rows
                .iter()
                .map(|row| row.replace(' ', ""))
                .collect::<Vec<_>>();
            let diagnostic = format!("{width}x{height}: {rows:#?}");

            assert_eq!(rows.len(), usize::from(height), "{diagnostic}");
            assert!(compact[3].contains("CrabCode原生RustTUI"), "{diagnostic}");
            assert!(compact[4].contains("●已就绪，可以开始"), "{diagnostic}");
            assert!(compact[6].contains("快速开始"), "{diagnostic}");
            assert!(
                compact[7].contains("›直接描述目标，或粘贴错误信息与日志"),
                "{diagnostic}"
            );
            assert!(
                compact[8].contains("Enter发送·/help操作说明·/model选择模型"),
                "{diagnostic}"
            );

            let joined = rows.join("\n");
            assert!(!joined.contains(['▀', '▄']), "{diagnostic}");
            assert!(!joined.contains("请在下方输入提示词"), "{diagnostic}");
            for row in 0..height {
                for column in 0..width {
                    let symbol_width =
                        unicode_width::UnicodeWidthStr::width(buffer[(column, row)].symbol());
                    assert!(
                        symbol_width <= usize::from(width - column),
                        "wide glyph outside buffer at ({column},{row}): {diagnostic}"
                    );
                }
            }
        }
    }

    #[test]
    fn task_card_compacts_successful_raw_history_but_keeps_failures_and_verbose_audit() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();

        let create = json!({
            "type":"assistant",
            "uuid":"task-create-use",
            "session_id":"task-history-session",
            "parent_tool_use_id":null,
            "message":{
                "id":"task-create-message",
                "content":[{
                    "type":"tool_use",
                    "id":"task-create",
                    "name":"TaskCreate",
                    "input":{
                        "subject":"审计任务",
                        "description":"TASK-CREATE-AUDIT-DETAIL"
                    }
                }]
            }
        });
        assert!(ingest_json(&mut app, 1, create).is_empty());
        view.prepare(&mut app)
            .expect("pending TaskCreate remains visible");
        assert_eq!(view.agent().scrollback().len(), 1);
        let pending_create = scrollback_searchable(&view);
        assert!(pending_create.contains("TaskCreate"), "{pending_create}");
        assert!(
            pending_create.contains("TASK-CREATE-AUDIT-DETAIL"),
            "{pending_create}"
        );
        for (width, height) in [(120, 30), (80, 24), (60, 20)] {
            let rendered = draw_prepared_buffer(&mut view, &mut app, width, height);
            let compact = rendered.replace(' ', "");
            assert!(
                compact.contains("TaskCreate"),
                "{width}x{height}: pending call disappeared: {rendered}"
            );
        }

        let create_result = json!({
            "type":"user",
            "uuid":"task-create-result",
            "timestamp":"2026-08-10T00:00:00.000Z",
            "toolUseResult":{"task":{"id":"1","subject":"审计任务"}},
            "message":{
                "role":"user",
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":"task-create",
                    "content":"TASK-CREATE-RESULT-AUDIT-DETAIL",
                    "is_error":false
                }]
            }
        });
        assert!(ingest_json(&mut app, 2, create_result).is_empty());
        view.prepare(&mut app)
            .expect("successful TaskCreate compaction");
        assert!(view.agent().scrollback().is_empty());
        assert_eq!(
            app.projection.items().len(),
            2,
            "compaction must not delete the authoritative audit projection"
        );
        assert_eq!(app.projection.raw_envelopes().len(), 2);
        assert!(app.projection.raw_envelopes().iter().any(|envelope| {
            envelope.value["message"]["content"][0]["content"] == "TASK-CREATE-RESULT-AUDIT-DETAIL"
        }));

        for (width, height) in [(120, 30), (80, 24), (60, 20)] {
            let rendered = draw_prepared_buffer(&mut view, &mut app, width, height);
            let compact = rendered.replace(' ', "");
            assert!(compact.contains("审计任务"), "{width}x{height}: {rendered}");
            assert!(
                !compact.contains("TaskCreate")
                    && !compact.contains("TASK-CREATE-RESULT-AUDIT-DETAIL"),
                "{width}x{height}: duplicate raw task history leaked beside the card: {rendered}"
            );
        }

        let update = json!({
            "type":"assistant",
            "uuid":"task-update-use",
            "session_id":"task-history-session",
            "parent_tool_use_id":null,
            "message":{
                "id":"task-update-message",
                "content":[{
                    "type":"tool_use",
                    "id":"task-update",
                    "name":"TaskUpdate",
                    "input":{"taskId":"1","status":"in_progress"}
                }]
            }
        });
        assert!(ingest_json(&mut app, 3, update).is_empty());
        view.prepare(&mut app)
            .expect("pending TaskUpdate remains visible");
        assert_eq!(view.agent().scrollback().len(), 1);
        let pending_update = scrollback_searchable(&view);
        assert!(pending_update.contains("TaskUpdate"), "{pending_update}");
        let update_result = json!({
            "type":"user",
            "uuid":"task-update-result",
            "timestamp":"2026-08-10T00:00:01.000Z",
            "toolUseResult":{
                "success":true,
                "taskId":"1",
                "updatedFields":["status"],
                "statusChange":{"from":"pending","to":"in_progress"}
            },
            "message":{
                "role":"user",
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":"task-update",
                    "content":"TASK-UPDATE-RESULT-AUDIT-DETAIL",
                    "is_error":false
                }]
            }
        });
        assert!(ingest_json(&mut app, 4, update_result).is_empty());
        view.prepare(&mut app)
            .expect("successful TaskUpdate compaction");
        assert!(view.agent().scrollback().is_empty());

        let failed_update = json!({
            "type":"assistant",
            "uuid":"task-update-failed-use",
            "session_id":"task-history-session",
            "parent_tool_use_id":null,
            "message":{
                "id":"task-update-failed-message",
                "content":[{
                    "type":"tool_use",
                    "id":"task-update-failed",
                    "name":"TaskUpdate",
                    "input":{"taskId":"1","status":"completed"}
                }]
            }
        });
        assert!(ingest_json(&mut app, 5, failed_update).is_empty());
        view.prepare(&mut app)
            .expect("pending failed TaskUpdate remains visible");
        assert_eq!(view.agent().scrollback().len(), 1);
        let pending_failed_update = scrollback_searchable(&view);
        assert!(
            pending_failed_update.contains("TaskUpdate"),
            "{pending_failed_update}"
        );
        let failed_result = json!({
            "type":"user",
            "uuid":"task-update-failed-result",
            "timestamp":"2026-08-10T00:00:02.000Z",
            "toolUseResult":{
                "success":false,
                "taskId":"1",
                "updatedFields":[],
                "error":"TASK-UPDATE-FAILURE-AUDIT"
            },
            "message":{
                "role":"user",
                "content":[{
                    "type":"tool_result",
                    "tool_use_id":"task-update-failed",
                    "content":"TASK-UPDATE-FAILURE-AUDIT",
                    "is_error":true
                }]
            }
        });
        assert!(ingest_json(&mut app, 6, failed_result).is_empty());
        view.prepare(&mut app)
            .expect("failed TaskUpdate remains visible");
        assert_eq!(view.agent().scrollback().len(), 1);
        let failed_searchable = scrollback_searchable(&view);
        assert!(
            failed_searchable.contains("TaskUpdate"),
            "{failed_searchable}"
        );
        assert!(
            failed_searchable.contains("TASK-UPDATE-FAILURE-AUDIT"),
            "a failed task update must retain its sanitized audit detail: {failed_searchable}"
        );

        app.set_presentation_verbose(true);
        view.prepare(&mut app)
            .expect("verbose mode restores compacted task audit rows");
        let searchable = scrollback_searchable(&view);
        assert!(searchable.contains("TaskCreate"), "{searchable}");
        assert!(
            searchable.contains("TASK-CREATE-AUDIT-DETAIL"),
            "{searchable}"
        );
        assert!(
            searchable.contains("TASK-CREATE-RESULT-AUDIT-DETAIL"),
            "{searchable}"
        );
        assert!(searchable.contains("TaskUpdate"), "{searchable}");
        assert!(
            searchable.contains("TASK-UPDATE-RESULT-AUDIT-DETAIL"),
            "{searchable}"
        );

        app.set_presentation_verbose(false);
        view.prepare(&mut app)
            .expect("quiet mode retires successful task audit rows again");
        let quiet_searchable = scrollback_searchable(&view);
        assert!(!quiet_searchable.contains("TASK-CREATE-AUDIT-DETAIL"));
        assert!(!quiet_searchable.contains("TASK-UPDATE-RESULT-AUDIT-DETAIL"));
        assert!(quiet_searchable.contains("TASK-UPDATE-FAILURE-AUDIT"));
    }

    #[test]
    fn malformed_task_result_keeps_pending_call_and_result_visible() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();
        assert!(
            ingest_json(
                &mut app,
                1,
                json!({
                    "type":"assistant",
                    "uuid":"malformed-create-use",
                    "session_id":"malformed-task-session",
                    "parent_tool_use_id":null,
                    "message":{
                        "id":"malformed-create-message",
                        "content":[{
                            "type":"tool_use",
                            "id":"malformed-create",
                            "name":"TaskCreate",
                            "input":{"subject":"expected subject","description":"details"}
                        }]
                    }
                }),
            )
            .is_empty()
        );
        view.prepare(&mut app)
            .expect("valid pending TaskCreate remains visible");
        assert_eq!(view.agent().scrollback().len(), 1);
        let pending_searchable = scrollback_searchable(&view);
        assert!(
            pending_searchable.contains("TaskCreate"),
            "{pending_searchable}"
        );
        assert!(
            pending_searchable.contains("expected subject"),
            "{pending_searchable}"
        );

        assert!(
            ingest_json(
                &mut app,
                2,
                json!({
                    "type":"user",
                    "uuid":"malformed-create-result",
                    "timestamp":"2026-08-10T00:00:00.000Z",
                    "toolUseResult":{
                        "task":{"id":"1","subject":"MALFORMED-TASK-RESULT-AUDIT"}
                    },
                    "message":{
                        "role":"user",
                        "content":[{
                            "type":"tool_result",
                            "tool_use_id":"malformed-create",
                            "content":"MALFORMED-TASK-RESULT-AUDIT",
                            "is_error":false
                        }]
                    }
                }),
            )
            .is_empty()
        );
        view.prepare(&mut app)
            .expect("malformed TaskCreate result remains renderable");
        assert!(app.task_panel_projection_state().degraded);
        assert_eq!(view.agent().scrollback().len(), 1);
        let malformed_searchable = scrollback_searchable(&view);
        assert!(
            malformed_searchable.contains("TaskCreate")
                && malformed_searchable.contains("expected subject")
                && malformed_searchable.contains("MALFORMED-TASK-RESULT-AUDIT"),
            "a compatibility failure must retain its call and bounded raw result: {malformed_searchable}"
        );
    }

    #[test]
    fn task_history_compaction_identity_is_reset_across_sessions() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();
        assert!(
            ingest_json(
                &mut app,
                1,
                json!({
                    "type":"assistant",
                    "uuid":"old-session-create",
                    "session_id":"old-task-session",
                    "parent_tool_use_id":null,
                    "message":{
                        "id":"old-session-message",
                        "content":[{
                            "type":"tool_use",
                            "id":"reused-task-id",
                            "name":"TaskCreate",
                            "input":{"subject":"old","description":"old"}
                        }]
                    }
                }),
            )
            .is_empty()
        );
        view.prepare(&mut app)
            .expect("old-session pending TaskCreate remains visible");
        assert_eq!(view.agent().scrollback().len(), 1);
        assert!(scrollback_searchable(&view).contains("TaskCreate"));

        assert!(
            ingest_json(
                &mut app,
                2,
                json!({
                    "type":"user",
                    "uuid":"old-session-create-result",
                    "timestamp":"2026-08-10T00:00:00.000Z",
                    "toolUseResult":{"task":{"id":"1","subject":"old"}},
                    "message":{
                        "role":"user",
                        "content":[{
                            "type":"tool_result",
                            "tool_use_id":"reused-task-id",
                            "content":"OLD-SESSION-CREATE-RESULT",
                            "is_error":false
                        }]
                    }
                }),
            )
            .is_empty()
        );
        view.prepare(&mut app)
            .expect("old-session successful TaskCreate is compacted");
        assert!(view.agent().scrollback().is_empty());

        assert!(
            ingest_json(
                &mut app,
                3,
                json!({
                    "type":"assistant",
                    "uuid":"new-session-update",
                    "session_id":"new-task-session",
                    "parent_tool_use_id":null,
                    "message":{
                        "id":"new-session-message",
                        "content":[{
                            "type":"tool_use",
                            "id":"reused-task-id",
                            "name":"TaskUpdate",
                            "input":{"taskId":"1","status":"in_progress"}
                        }]
                    }
                }),
            )
            .is_empty()
        );
        view.prepare(&mut app)
            .expect("new-session TaskUpdate does not inherit old compaction identity");
        assert_eq!(view.agent().scrollback().len(), 1);
        let new_session_searchable = scrollback_searchable(&view);
        assert!(new_session_searchable.contains("TaskUpdate"));
        assert!(
            !new_session_searchable.contains("TaskCreate")
                && !new_session_searchable.contains("OLD-SESSION-CREATE-RESULT"),
            "a reused tool_use_id in a new session must remain independent: {new_session_searchable}"
        );
    }

    #[test]
    fn real_model_switch_replay_remains_renderable_through_the_root_view() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        let mut view = AppView::new();
        let value = json!({
            "type":"user",
            "message":{
                "role":"user",
                "content":"<local-command-stdout>Set model to deepseek-v4-flash</local-command-stdout>"
            },
            "session_id":"session-model-switch",
            "parent_tool_use_id":null,
            "uuid":"model-output",
            "timestamp":"2026-07-30T02:46:00.002Z",
            "isReplay":true
        });
        let classification =
            crate::sdk_runtime::classify_envelope(&value).expect("classify SDK replay");
        let encoded_len = serde_json::to_vec(&value).expect("encode SDK replay").len();

        assert!(
            app.handle_runtime_event(RuntimeEvent::Envelope(RawEnvelope {
                sequence: 0,
                encoded_len,
                value,
                classification,
                correlation: None,
            }))
            .is_empty()
        );
        terminal
            .draw(|frame| {
                view.draw(frame, &mut app)
                    .expect("the real set_model replay must not stop presentation");
            })
            .expect("draw");

        assert!(!app.should_quit);
        assert!(app.fatal.is_none());
        assert_eq!(view.agent().scrollback().len(), 1);
        assert_eq!(
            view.agent()
                .scrollback()
                .entry(0)
                .and_then(|entry| entry.block.searchable_text())
                .as_deref(),
            Some("Set model to deepseek-v4-flash")
        );
    }

    #[test]
    fn shared_local_slash_command_sequence_remains_renderable_through_the_root_view() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();
        let values = [
            json!({
                "type":"user",
                "uuid":"command-caveat",
                "timestamp":"2026-07-30T02:45:53.000Z",
                "isMeta":true,
                "message":{
                    "role":"user",
                    "content":"<local-command-caveat>Caveat: generated locally.</local-command-caveat>"
                }
            }),
            json!({
                "type":"system",
                "subtype":"local_command",
                "content":concat!(
                    "<command-name>/doctor</command-name>\n",
                    "<command-message>doctor</command-message>\n",
                    "<command-args></command-args>"
                ),
                "level":"info",
                "uuid":"command-input",
                "timestamp":"2026-07-30T02:45:53.001Z",
                "isMeta":false
            }),
            json!({
                "type":"system",
                "subtype":"local_command",
                "content":concat!(
                    "<local-command-stdout>",
                    "**诊断完成**\n网络：可达",
                    "</local-command-stdout>"
                ),
                "level":"info",
                "uuid":"command-output",
                "timestamp":"2026-07-30T02:45:53.002Z",
                "isMeta":false
            }),
        ];

        for (sequence, value) in values.into_iter().enumerate() {
            let classification =
                crate::sdk_runtime::classify_envelope(&value).expect("classify direct event");
            let encoded_len = serde_json::to_vec(&value)
                .expect("encode direct event")
                .len();
            assert!(
                app.handle_runtime_event(RuntimeEvent::Envelope(RawEnvelope {
                    sequence: sequence as u64,
                    encoded_len,
                    value,
                    classification,
                    correlation: None,
                }))
                .is_empty()
            );
        }

        terminal
            .draw(|frame| {
                view.draw(frame, &mut app)
                    .expect("shared local-command sequence must not stop presentation");
            })
            .expect("draw");

        assert!(!app.should_quit);
        assert!(app.fatal.is_none());
        assert_eq!(
            view.agent().scrollback().len(),
            2,
            "the meta caveat is hidden while command and output remain"
        );
        let searchable = (0..view.agent().scrollback().len())
            .filter_map(|index| {
                view.agent()
                    .scrollback()
                    .entry(index)?
                    .block
                    .searchable_text()
            })
            .collect::<Vec<_>>();
        assert_eq!(searchable, ["/doctor", "**诊断完成**\n网络：可达"]);
    }

    #[test]
    fn normal_direct_event_consumers_prepare_without_quit_or_fatal() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();
        let values = vec![
            json!({
                "type":"system","subtype":"stop_hook_summary","hookCount":1,
                "hookInfos":[{"hookName":"Stop","durationMs":12}],
                "hookErrors":["blocked"],"preventedContinuation":true,
                "stopReason":"policy","hasOutput":false,"level":"warning",
                "uuid":"stop","timestamp":"2026-07-30T03:00:00.000Z"
            }),
            json!({
                "type":"system","subtype":"turn_duration","durationMs":66_000,
                "budgetTokens":1_250,"budgetLimit":2_000,"budgetNudges":2,
                "uuid":"turn-duration","timestamp":"2026-07-30T03:00:00.500Z",
                "isMeta":false
            }),
            json!({
                "type":"attachment",
                "attachment":{
                    "type":"queued_command","prompt":"queued prompt",
                    "imagePasteIds":[7],"commandMode":"prompt","isMeta":true,
                    "origin":{"kind":"coordinator"}
                },
                "uuid":"queued","timestamp":"2026-07-30T03:00:01.000Z"
            }),
            json!({
                "type":"attachment",
                "attachment":{
                    "type":"task_status","taskId":"teammate-1",
                    "taskType":"in_process_teammate","status":"completed",
                    "description":"review"
                },
                "uuid":"teammate","timestamp":"2026-07-30T03:00:02.000Z"
            }),
            json!({
                "type":"system","subtype":"api_error","level":"error",
                "error":{"message":"temporary failure","status":503},
                "retryInMs":5000,"retryAttempt":4,"maxRetries":6,
                "uuid":"api","timestamp":"2026-07-30T03:00:03.000Z"
            }),
            json!({
                "type":"stream_event","uuid":"stream-0","session_id":"session",
                "parent_tool_use_id":null,
                "event":{"type":"message_start","message":{"id":"message"}}
            }),
            json!({
                "type":"stream_event","uuid":"stream-1","session_id":"session",
                "parent_tool_use_id":null,
                "event":{"type":"content_block_start","index":0,
                    "content_block":{"type":"compaction","content":"internal summary"}}
            }),
            json!({
                "type":"stream_event","uuid":"stream-2","session_id":"session",
                "parent_tool_use_id":null,
                "event":{"type":"content_block_stop","index":0}
            }),
            json!({
                "type":"stream_event","uuid":"stream-3","session_id":"session",
                "parent_tool_use_id":null,
                "event":{"type":"message_stop"}
            }),
        ];

        for (sequence, value) in values.into_iter().enumerate() {
            let classification =
                crate::sdk_runtime::classify_envelope(&value).expect("classify normal event");
            let encoded_len = serde_json::to_vec(&value).expect("encode event").len();
            assert!(
                app.handle_runtime_event(RuntimeEvent::Envelope(RawEnvelope {
                    sequence: sequence as u64,
                    encoded_len,
                    value,
                    classification,
                    correlation: None,
                }))
                .is_empty()
            );
            view.prepare(&mut app)
                .expect("normal event must have a closed renderer consumer");
            assert!(!app.should_quit);
            assert!(app.fatal.is_none());
        }
        assert!(
            view.agent().scrollback().len() >= 3,
            "visible stop-hook, queue, and teammate rows remain after the API tail is removed"
        );
    }

    #[test]
    fn sparse_direct_assistant_usage_survives_prepare_and_draw() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();

        assert!(
            ingest_json(
                &mut app,
                47,
                json!({
                    "type":"assistant",
                    "uuid":"assistant-sparse-usage",
                    "timestamp":"2026-07-30T18:12:47.000Z",
                    "message":{
                        "id":"message-sparse-usage",
                        "model":"fixed-model",
                        "content":[{"type":"text","text":"第二轮仍可使用"}],
                        "usage":{"input_tokens":101,"output_tokens":7}
                    }
                }),
            )
            .is_empty()
        );
        assert_presentable(&mut app, &mut view, "sparse direct assistant usage");
        assert!(app.take_runtime_stop_action().is_none());
    }

    #[test]
    fn every_legal_direct_system_variant_survives_the_full_root_view_transaction() {
        let timestamp = "2026-07-27T00:00:00.000Z";
        let values = vec![
            json!({
                "type":"system","subtype":"informational","content":"notice","level":"info",
                "toolUseID":"tool-info","preventContinuation":true,
                "uuid":"system-0","timestamp":timestamp,"isMeta":true
            }),
            json!({
                "type":"system","subtype":"permission_retry","content":"allowed","level":"info",
                "commands":["git status","pwd"],"uuid":"system-1","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"scheduled_task_fire","content":"scheduled",
                "uuid":"system-3","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"stop_hook_summary","hookCount":1,
                "hookInfos":[{"hookName":"prompt","durationMs":12,"output":"prompt text"}],
                "hookErrors":["failed"],"preventedContinuation":true,"stopReason":"blocked",
                "hasOutput":true,"level":"warning","toolUseID":"tool-stop","hookLabel":"stop",
                "totalDurationMs":12,"uuid":"system-4","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"turn_duration","durationMs":1500,
                "budgetTokens":100,"budgetLimit":200,"budgetNudges":2,"messageCount":3,
                "uuid":"system-5","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"away_summary","content":"away",
                "uuid":"system-6","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"memory_saved","writtenPaths":["/tmp/a","/tmp/b"],
                "teamCount":1,"uuid":"system-7","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"agents_killed",
                "uuid":"system-8","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"api_metrics","ttftMs":10,"otps":20,"isP50":true,
                "hookDurationMs":1,"turnDurationMs":2,"toolDurationMs":3,
                "classifierDurationMs":4,"toolCount":5,"hookCount":6,
                "classifierCount":7,"configWriteCount":8,
                "uuid":"system-9","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"local_command","content":"local","level":"suggestion",
                "uuid":"system-10","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"api_error","level":"error",
                "error":{
                    "message":"Connection error.","status":503,
                    "error":{"error":{"message":"deep response"}},
                    "cause":{"code":"ETIMEDOUT"}
                },
                "retryInMs":1000,"retryAttempt":2,"maxRetries":4,
                "uuid":"system-11","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"compact_boundary","content":"compacted","level":"info",
                "compactMetadata":{
                    "trigger":"manual","preTokens":100,"userContext":"keep this",
                    "messagesSummarized":5,"preCompactDiscoveredTools":["Read","Bash"],
                    "preservedSegment":{"headUuid":"head","anchorUuid":"anchor","tailUuid":"tail"}
                },
                "logicalParentUuid":"parent","uuid":"system-12","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"microcompact_boundary","content":"micro","level":"info",
                "microcompactMetadata":{
                    "trigger":"auto","preTokens":80,"tokensSaved":20,
                    "compactedToolIds":["tool-a"],"clearedAttachmentUUIDs":["attachment-a"]
                },
                "uuid":"system-13","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"command_input","content":"/status",
                "uuid":"system-14","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"thinking","content":"working",
                "uuid":"system-15","timestamp":timestamp
            }),
            json!({
                "type":"system","subtype":"file_snapshot","content":"snapshot","level":"info",
                "snapshotFiles":[{"key":"a","path":"/tmp/a","content":"large source body"}],
                "uuid":"system-16","timestamp":timestamp
            }),
        ];
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();

        for (sequence, value) in values.into_iter().enumerate() {
            let subtype = value["subtype"]
                .as_str()
                .expect("direct system subtype")
                .to_string();
            assert!(
                ingest_json(&mut app, sequence as u64, value).is_empty(),
                "{subtype} must not invent a host action"
            );
            assert_presentable(&mut app, &mut view, &format!("direct system `{subtype}`"));
        }
    }

    #[test]
    fn every_legal_sdk_system_variant_survives_the_full_root_view_transaction() {
        let values = vec![
            json!({
                "type":"system","subtype":"init",
                "apiKeySource":"none","crab_code_version":"1.0.0",
                "cwd":"/workspace","tools":[],"mcp_servers":[],
                "model":"model","permissionMode":"default",
                "slash_commands":[],"output_style":"default",
                "skills":[],"plugins":[],"uuid":"0","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"compact_boundary",
                "compact_metadata":{"trigger":"auto","pre_tokens":100},
                "uuid":"1","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"status","status":null,
                "uuid":"2","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"post_turn_summary",
                "summarizes_uuid":"assistant","status_category":"completed",
                "status_detail":"done","is_noteworthy":false,"title":"Done",
                "description":"Completed","recent_action":"Tested","needs_action":"",
                "artifact_urls":[],"uuid":"3","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"api_retry","attempt":1,"max_retries":3,
                "retry_delay_ms":100,"error_status":null,"error":"server_error",
                "uuid":"4","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"local_command_output","content":"x",
                "uuid":"5","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"hook_started","hook_id":"h","hook_name":"command",
                "hook_event":"PreToolUse","uuid":"6","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"hook_progress","hook_id":"hp","hook_name":"command",
                "hook_event":"PreToolUse","stdout":"","stderr":"","output":"running",
                "uuid":"7","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"hook_response","hook_id":"hr","hook_name":"command",
                "hook_event":"PreToolUse","stdout":"","stderr":"","output":"done",
                "outcome":"success","uuid":"8","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"task_notification","task_id":"tn",
                "status":"completed","output_file":"/tmp/task","summary":"done",
                "uuid":"9","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"task_started","task_id":"ts",
                "description":"start","uuid":"10","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"task_progress","task_id":"tp",
                "description":"work","usage":{"total_tokens":1,"tool_uses":2,"duration_ms":3},
                "uuid":"11","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"session_state_changed","state":"idle",
                "uuid":"12","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"files_persisted","files":[],"failed":[],
                "processed_at":"2026-07-28T00:00:00.000Z","uuid":"13","session_id":"s"
            }),
            json!({
                "type":"system","subtype":"elicitation_complete","mcp_server_name":"server",
                "elicitation_id":"elicitation","uuid":"14","session_id":"s"
            }),
        ];
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();

        for (sequence, value) in values.into_iter().enumerate() {
            let subtype = value["subtype"]
                .as_str()
                .expect("SDK system subtype")
                .to_string();
            assert!(
                ingest_json(&mut app, sequence as u64, value).is_empty(),
                "{subtype} must not invent a host action"
            );
            assert_presentable(&mut app, &mut view, &format!("SDK system `{subtype}`"));
        }
    }

    #[test]
    fn every_control_purpose_failure_is_non_terminal_and_presentable() {
        let purposes = vec![
            ("initialize", OutboundPurpose::Initialize),
            ("interrupt", OutboundPurpose::Interrupt),
            (
                "generic",
                OutboundPurpose::Generic("future_control".to_string()),
            ),
            ("set model", OutboundPurpose::SetModel),
            ("login start", OutboundPurpose::LoginStart),
            ("login wait", OutboundPurpose::LoginWait),
            ("MCP status", OutboundPurpose::McpStatus),
            (
                "MCP status for bulk enable",
                OutboundPurpose::McpStatusForBulkToggle { enabled: true },
            ),
            ("MCP status refresh", OutboundPurpose::McpStatusRefresh),
            (
                "MCP authenticate",
                OutboundPurpose::McpAuthenticate {
                    server_name: "fixture".to_string(),
                },
            ),
            (
                "MCP clear auth",
                OutboundPurpose::McpClearAuth {
                    server_name: "fixture".to_string(),
                },
            ),
            (
                "MCP toggle",
                OutboundPurpose::McpToggle {
                    server_name: "fixture".to_string(),
                    enabled: true,
                },
            ),
            (
                "MCP bulk toggle",
                OutboundPurpose::McpBulkToggleStep {
                    server_name: "fixture".to_string(),
                    enabled: true,
                    remaining: Vec::new(),
                    completed: 0,
                },
            ),
            (
                "MCP reconnect",
                OutboundPurpose::McpReconnect {
                    server_name: "fixture".to_string(),
                },
            ),
            ("context usage", OutboundPurpose::ContextUsage),
            ("reload plugins", OutboundPurpose::ReloadPlugins),
            (
                "side question",
                OutboundPurpose::SideQuestion {
                    question: "fixture question".to_string(),
                },
            ),
            (
                "stop task",
                OutboundPurpose::StopTask {
                    task_id: "fixture-task".to_string(),
                },
            ),
        ];

        for (sequence, (label, purpose)) in purposes.into_iter().enumerate() {
            let request_id = format!("failure-{sequence}");
            let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
            app.release_startup_barrier_for_test();
            app.record_control_request(request_id.clone(), purpose);
            let actions = ingest_json(
                &mut app,
                sequence as u64,
                json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "error",
                        "request_id": request_id,
                        "error": "deterministic fixture failure"
                    }
                }),
            );
            assert!(
                actions.iter().all(|action| !matches!(
                    action,
                    crate::tui_app::HostAction::StopRuntime { .. }
                )),
                "{label} failure must not stop the direct runtime"
            );
            let mut view = AppView::new();
            assert_presentable(
                &mut app,
                &mut view,
                &format!("failed control purpose `{label}`"),
            );
        }
    }

    #[test]
    fn every_successful_control_consumer_is_non_terminal_and_presentable() {
        let current_dir = std::env::current_dir().expect("current directory");
        let mut initialize = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        initialize
            .configure_pre_initialize_setup(current_dir)
            .expect("configure fixed startup lifecycle");
        assert_eq!(
            initialize.initial_actions(),
            vec![crate::tui_app::HostAction::Initialize]
        );
        initialize.record_control_request(
            "success-initialize".to_string(),
            OutboundPurpose::Initialize,
        );
        assert_eq!(
            ingest_json(
                &mut initialize,
                0,
                successful_control_response(
                    "success-initialize",
                    Some(json!({
                        "commands": [],
                        "agents": [],
                        "available_output_styles": [],
                        "models": [],
                        "output_style": "default",
                        "account": {},
                        "pid": 1
                    })),
                ),
            ),
            vec![
                crate::tui_app::HostAction::SendPrivateRuntimeAction {
                    request_id: "crabcode-tui-runtime-1".to_string(),
                    action: json!({"kind":"health_snapshot"}),
                    purpose: crate::tui_app::PrivateRuntimePurpose::HealthSnapshot,
                },
                crate::tui_app::HostAction::SendPrivateRuntimeAction {
                    request_id: "crabcode-tui-runtime-2".to_string(),
                    action: json!({"kind":"retained.identity.snapshot"}),
                    purpose: crate::tui_app::PrivateRuntimePurpose::RetainedIdentitySnapshot,
                },
                crate::tui_app::HostAction::SendControl {
                    request: json!({"subtype":"get_context_usage"}),
                    purpose: OutboundPurpose::ContextUsageRefresh,
                },
            ],
            "initialize releases private probes and one silent context refresh"
        );
        let mut initialize_view = AppView::new();
        assert_presentable(
            &mut initialize,
            &mut initialize_view,
            "successful initialize control",
        );

        let context = crate::context_visualization::minimal_test_control_response();
        let cases = vec![
            ("interrupt", OutboundPurpose::Interrupt, None),
            (
                "generic",
                OutboundPurpose::Generic("future_control".to_string()),
                Some(json!({"opaque": true})),
            ),
            ("set model", OutboundPurpose::SetModel, None),
            (
                "login start",
                OutboundPurpose::LoginStart,
                Some(json!({
                    "manualUrl": "https://acosmi.com/oauth/authorize",
                    "automaticUrl": "https://acosmi.com/oauth/authorize"
                })),
            ),
            (
                "login wait",
                OutboundPurpose::LoginWait,
                Some(json!({"account": {"email": "fixture@example.invalid"}})),
            ),
            (
                "MCP status",
                OutboundPurpose::McpStatus,
                Some(json!({"mcpServers": []})),
            ),
            (
                "MCP status for bulk enable",
                OutboundPurpose::McpStatusForBulkToggle { enabled: true },
                Some(json!({"mcpServers": []})),
            ),
            (
                "MCP status refresh",
                OutboundPurpose::McpStatusRefresh,
                Some(json!({"mcpServers": []})),
            ),
            (
                "MCP authenticate",
                OutboundPurpose::McpAuthenticate {
                    server_name: "fixture".to_string(),
                },
                Some(json!({"requiresUserAction": false})),
            ),
            (
                "MCP clear auth",
                OutboundPurpose::McpClearAuth {
                    server_name: "fixture".to_string(),
                },
                None,
            ),
            (
                "MCP toggle",
                OutboundPurpose::McpToggle {
                    server_name: "fixture".to_string(),
                    enabled: true,
                },
                None,
            ),
            (
                "MCP bulk toggle",
                OutboundPurpose::McpBulkToggleStep {
                    server_name: "fixture".to_string(),
                    enabled: true,
                    remaining: Vec::new(),
                    completed: 0,
                },
                None,
            ),
            (
                "MCP reconnect",
                OutboundPurpose::McpReconnect {
                    server_name: "fixture".to_string(),
                },
                None,
            ),
            (
                "context usage",
                OutboundPurpose::ContextUsage,
                Some(context["response"].clone()),
            ),
            (
                "reload plugins",
                OutboundPurpose::ReloadPlugins,
                Some(json!({
                    "commands": [],
                    "agents": [],
                    "plugins": [],
                    "mcpServers": [],
                    "error_count": 0
                })),
            ),
            (
                "side question",
                OutboundPurpose::SideQuestion {
                    question: "fixture question".to_string(),
                },
                Some(json!({"response": "fixture answer"})),
            ),
            (
                "stop task",
                OutboundPurpose::StopTask {
                    task_id: "fixture-task".to_string(),
                },
                Some(json!({})),
            ),
        ];

        for (sequence, (label, purpose, body)) in cases.into_iter().enumerate() {
            let request_id = format!("success-{sequence}");
            let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
            app.release_startup_barrier_for_test();
            app.record_control_request(request_id.clone(), purpose);
            let actions = ingest_json(
                &mut app,
                (sequence + 1) as u64,
                successful_control_response(&request_id, body),
            );
            assert!(
                actions.iter().all(|action| !matches!(
                    action,
                    crate::tui_app::HostAction::StopRuntime { .. }
                )),
                "{label} success must not stop the direct runtime"
            );
            let mut view = AppView::new();
            assert_presentable(
                &mut app,
                &mut view,
                &format!("successful control purpose `{label}`"),
            );
        }
    }

    #[test]
    fn api_retry_root_clock_redraws_without_input_and_parks_when_message_is_removed() {
        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();
        let api_error = json!({
            "type":"system","subtype":"api_error","level":"error",
            "error":{"message":"temporary failure","status":503},
            "retryInMs":2500,"retryAttempt":4,"maxRetries":6,
            "uuid":"api-clock","timestamp":"2026-07-30T03:00:03.000Z"
        });
        let classification =
            crate::sdk_runtime::classify_envelope(&api_error).expect("classify API error");
        let encoded_len = serde_json::to_vec(&api_error)
            .expect("encode API error")
            .len();
        assert!(
            app.handle_runtime_event(RuntimeEvent::Envelope(RawEnvelope {
                sequence: 0,
                encoded_len,
                value: api_error,
                classification,
                correlation: None,
            }))
            .is_empty()
        );
        view.prepare(&mut app).expect("project API error");
        assert!(
            !view.minimal_will_commit(&app),
            "the historical dynamic API retry must not leak into immutable native scrollback, even while the app is idle"
        );

        let queried_at = Instant::now();
        let first = view
            .renderer_animation_deadline(queried_at)
            .expect("visible retry attempt four arms the root clock");
        assert!(first > queried_at);
        assert!(!view.tick_renderer_animation(first - Duration::from_nanos(1)));
        assert!(view.tick_renderer_animation(first));
        assert_eq!(
            view.renderer_animation_deadline(first),
            Some(first + Duration::from_secs(1)),
            "a due no-input tick schedules the next absolute second boundary"
        );

        let later_message = json!({
            "type":"system","subtype":"informational","level":"info",
            "content":"later message",
            "uuid":"after-api","timestamp":"2026-07-30T03:00:04.000Z"
        });
        let classification =
            crate::sdk_runtime::classify_envelope(&later_message).expect("classify later message");
        let encoded_len = serde_json::to_vec(&later_message)
            .expect("encode later message")
            .len();
        assert!(
            app.handle_runtime_event(RuntimeEvent::Envelope(RawEnvelope {
                sequence: 1,
                encoded_len,
                value: later_message,
                classification,
                correlation: None,
            }))
            .is_empty()
        );
        view.prepare(&mut app)
            .expect("later message removes historical API error row");
        assert_eq!(
            view.renderer_animation_deadline(first),
            None,
            "removing the API tail parks the root renderer clock"
        );
        assert!(!view.tick_renderer_animation(first + Duration::from_secs(10)));
    }

    #[test]
    fn root_input_owner_routes_product_actions_into_the_same_agent_view() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = TuiApp::new(&json!({}), InitialSessionRequest::New, None);
        app.release_startup_barrier_for_test();
        let mut view = AppView::new();
        let outcome = view.handle_input(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Instant::now(),
            PasteProvenance::Terminal,
        );
        assert!(outcome.actions.is_empty());
        assert!(!app.composer_focused());

        view.prepare(&mut app)
            .expect("root input action reaches persistent AgentView");
        assert!(view.agent().scrollback_focused_for_test());
    }

    #[test]
    fn legacy_transcript_owners_are_excluded_from_production_compilation() {
        let ui = include_str!("tui_ui.rs");
        for marker in [
            "struct VisibleTranscript",
            "enum NoticeKind",
            "struct NoticeRecord",
            "struct DirectNestedProgressIdentity",
            "struct DirectNestedProgressLayoutPlan",
            "struct ItemLayoutRevision",
            "struct TranscriptLayoutEntry",
            "enum LayoutAnchor",
            "struct TranscriptLayoutCache",
            "struct CrabCodeMarkdownStream",
            "struct RenderedTranscriptPart",
            "fn render_transcript(",
            "fn render_projected_item_with_state(",
            "fn render_projected_item_with_state_context(",
            "fn render_projected_item_with_state_mode_context(",
            "fn append_projected_item(",
            "fn append_mermaid_affordance_row(",
            "fn terminal_lines_for_projected_item_with_theme(",
            "fn collapse_rendered_item(",
            "fn truncate_rendered_thinking(",
            "fn remap_rendered_rows(",
        ] {
            assert_test_only_declaration(ui, marker);
        }

        let app = include_str!("tui_app.rs");
        for marker in [
            "pub(crate) enum TranscriptDisplayMode",
            "pub(crate) struct TranscriptItemInteraction",
            "pub(crate) enum TranscriptSelectionDirection",
            "pub scroll: ScrollState",
            "pub(crate) transcript_layout: TranscriptLayoutCache",
        ] {
            assert_test_only_declaration(app, marker);
        }
    }
}
