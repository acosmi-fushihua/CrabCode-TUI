# CrabCode modifications

Relative to the fixed source identified in `PROVENANCE.md`:

1. The selected presentation algorithms were moved into a
   backend-independent crate. This crate does not own sessions, model turns,
   tools, permissions, configuration authority, telemetry, AppServer
   transport, or GUI state.
2. Product-facing identifiers, environment variables, tests, URLs, imports,
   and documentation use CrabCode or neutral names. Fixed upstream names
   remain only in attribution/provenance material.
3. `audited_host` replaces shared host/TTY/SSH helpers with local,
   deterministic host and display-server detection. Required platform calls
   have narrow `unsafe_code` allowances.
4. `audited_terminal` removes image/overlay orchestration, feedback,
   telemetry, shared clipboard authority, and upstream host paths. Terminal
   detection, embedded-editor, hyperlinks, keyboard, probe, tmux, XTVERSION,
   and their neutralized tests remain presentation-only.
5. `audited_theme` keeps the fixed Rust TUI theme lifecycle: complete semantic
   role model, neutral day/night mother palettes, capability quantization,
   process-local current-setting cache, automatic system appearance watcher,
   and startup-only OSC 11 fallback. It removes all disk/config/backend
   authority. Six historical CrabCode palettes override only 12 exact semantic
   field matches; the machine-checked 65-field `PRODUCT_ROLE_AUDIT` marks every
   other field as inherited rather than guessing a mapping. ANSI settings cap
   at the basic color level without defeating `NO_COLOR`.
6. `audited_render` preserves the selected color, line, safe-buffer,
   renderable, path-display, wrapping, preview, and video-chrome algorithms.
   Internal paths were redirected locally; path fixtures and documentation
   were neutralized; unsafe offset arithmetic was replaced with safe address
   subtraction. `src/audited_render/wrapping.rs` redirects its Markdown/SafeBuf
   tests to the local renderer crates; the latest byte drift is confined to
   captured-format-argument spelling in one test assertion and does not alter
   production wrapping. Video decoding, filesystem authority, and tool
   execution are not present.
7. `audited_glyphs` retains terminal/legacy-console fallback behavior while
   using audited host/terminal inputs and the
   `CRABCODE_FORCE_LEGACY_CONSOLE` test override.
8. The three `audited_appearance` value modules and the selected
   `line_utils`, `renderable`, and `safe_buf` modules are byte-identical. Their
   local module roots are CrabCode adapters.
9. `link_opener` uses audited terminal/host inputs, local OS detach behavior,
   strict scheme filtering, neutral fixtures, and
   `CRABCODE_TEST_OPEN_URL_FILE`. It owns no remote settings, billing,
   telemetry, or backend routing.
10. `audited_modal_window_state`, `preview_overlay`, and `video_overlay`
    retain renderer-owned data/chrome only. Backend data acquisition and
    execution authority remain outside this crate.
11. `terminal_output` adapts the fixed upstream VTE/cursor/erase model and
    adds the grapheme, wide-cell, continuation-cell, and bounded-cursor
    handling required by the local renderer.
12. `tui_render` is a local composite over the exact fixed source anchors
    listed in `SOURCE_MANIFEST.json`; wrapping, clipping, search projection,
    scrollbar, OSC 8, shortcut, and scroll-state behavior terminate at the
    local Ratatui boundary.
13. `Cargo.toml` was recreated with only dependencies used by these
    presentation modules. Its dependency sources were checked against the
    fixed `xai-grok-pager-render/Cargo.toml` and
    `xai-grok-pager/Cargo.toml`; local package names replace upstream package
    names, while ACP, agent, configuration, telemetry, tool, update, sandbox,
    and other backend/application crates are not declared.
14. The source-shaped scrollback graph now includes `SelectionBox`,
    `RenderOutput`, `ScrollInfo`, the scratch/render-output value layer,
    `EntryId`, the O(log n) paint-window algorithm, the complete
    `BlockContent`/`RenderBlock` contract, `ScrollbackEntry`,
    `ScrollbackState`, `ScrollbackPane`, the scroll render pass, and
    dependency-closed
    system/workflow/credit-limit/background-task/session-event/tool-hook/
    tool-lifecycle blocks plus the dependency-closed execute/edit/read/
    list-dir/search/memory-search/web-fetch/web-search/integration-search/
    integration-dispatch/generic-tool display leaves.
    Background-task command preambles retain the fixed syntax highlighting,
    tree-sitter operator breaks, quote-aware wrapping, and heredoc protection;
    those helpers parse for presentation only and own no permission or command
    execution authority. `src/scrollback/blocks/bg_task.rs` redirects only that
    display helper to the renderer-local `shell_command` module, neutralizes
    fixtures/comments, and keeps fixed lifecycle/navigation semantics. Its
    latest eight-byte drift is exactly three captured-format-argument rewrites
    in the completed/failed summary; output and control flow are unchanged.
    Session events retain the fixed typed display variants, recap
    folding/selection, warning accents, parked-marker eligibility, and
    stop-hook summary/detail rendering. The enum is renderer-internal and
    introduces no backend event or protocol field.
    Execute/read display dependencies are renderer-local copies of the fixed
    shell highlighter/wrapper and exact `SKILL.md` path recognizer; neither
    imports execution, permission, tool, configuration, or protocol authority.
15. The non-byte-identical files in the source-shaped owner graph have these
    exhaustive local patches:
    - `src/input/line_editor.rs`: rename the text-area dependency to
      `crabcode_ratatui_textarea`; remove the unused `reset` entry point; and
      compile the cursor, grapheme-delete, and paste-policy helpers only for
      their remaining tests. The production key-to-edit state machine is
      unchanged.
    - `src/input/mod.rs`: retain the dependency-closed `line_editor`, `mouse`,
      and private `scroll_log` modules under a renderer-owned description;
      unrelated upstream key, terminal-support, and macOS-modifier modules are
      not re-exported.
    - `src/input/mouse.rs`: preserve the fixed source file and stream state
      machine. Redirect terminal detection to `audited_terminal`, rename the
      desktop terminal enum variant, and make only the cross-crate production
      methods public. Replace the unavailable upstream settings-cache
      constructor with the historical direct TUI's renderer-local
      `CRABCODE_SCROLL_SPEED` input, including its JavaScript `parseFloat`
      prefix/default/cap contract. Unsupported fixed-shell scroll settings are
      not guessed and no backend or wire field is added. `new_at` is exposed
      only under the existing `test-support` feature for deterministic
      application-route tests.
    - `src/input/mouse/tests.rs`: preserve the fixed source test module and
      replace only the upstream settings-cache test, for which CrabCode has no
      authority, with an exact historical speed-parser contract test. Product
      environment names and fixed-harness comments are neutralized.
    - `src/input/scroll_log.rs`: preserve the fixed JSONL record schema,
      lazy-open/fail-closed writer, and transition logging. Neutralize the
      environment/path names and obtain the renderer-local default path from
      `dirs::home_dir` instead of importing the upstream product home helper.
    - `src/scrollback/search.rs`: rename the query viewport return type from
      `xai_ratatui_textarea` to `crabcode_ratatui_textarea`; replace two
      poisoned-mutex `unwrap` calls with invariant-specific `expect`
      diagnostics; and compile the paste helper only for its remaining tests.
      Search matching, generation ordering, and daemon ownership are
      unchanged.
    - `src/scrollback/mod.rs`: retain the complete fixed module tree, use a
      backend-independent module description, and narrow/reorganize only its
      public convenience re-exports for this renderer crate.
    - `src/scrollback/state/mod.rs`: redirect the two
      `inline_media_ffmpeg::ffmpeg_available` references to the renderer-local
      `inline_media::ffmpeg_available` adapter and neutralize the adjacent
      backend-name-only comment. Add exactly two renderer-private projection
      mutations: `reorder_entries_exact` accepts only an atomic full
      `EntryId` permutation while retaining entry/view state, and
      `replace_entry_block_preserving_view` replaces one proven `RenderBlock`
      without reallocating its `EntryId`, completing running-to-final changes
      through the fixed `finish_running` lifecycle. Four focused tests cover
      identity/view preservation, turn-index and stale-selection invalidation,
      exact reorder state preservation, and atomic rejection of inexact
      reorders. These methods mutate only renderer-owned `ScrollbackState`;
      they add no backend operation, transport field, or protocol authority.
      `take_expandable_committed` is public only so the outer terminal owner
      can consume the fixed minimal-mode expand ring.
      `is_native_scrollback_committed` is a read-only identity query used by
      the renderer-private projection owner to keep its in-memory order equal
      to already immutable native terminal history when a later snapshot
      reveals an older row; it exposes no commit mutation. Reconnect-only
      ID-floor, append, and block-replacement helpers are test-only in this
      pure-TUI crate; unused reconnect bookmark and finish-all-running wrappers
      are not compiled. The underlying fixed layout, commit, finish-running,
      and turn rebuild algorithms remain the implementation used by the
      retained entry points.
    - `src/scrollback/state/selection.rs`: retain the fixed thinking-block
      expansion/collapse algorithm and extract its state application into a
      private helper. Add the idempotent `set_thinking_expanded(bool)` entry
      point so an already-existing CrabCode presentation setting can set,
      rather than toggle, the same renderer-owned current-block and
      future-block display state. It adds no backend operation, event, wire
      field, transport route, or protocol authority.
    - `src/scrollback/state/timeline.rs`: redirect the test import from
      `crate::views::timeline` to the local `crate::timeline` module.
    - `src/scrollback/state/verb_group.rs`: replace the upstream product word
      in one test query fixture with the neutral word `renderer`, and compile
      the block-kind comparison helper only for its remaining tests.
    - `src/scrollback/wrappers/entry_renderer.rs`: redirect
      `inline_media_ffmpeg` to the local `inline_media` module, replace
      upstream CLI-specific resume wording with neutral bulk-resume wording,
      use the neutral `Theme::dark()` constructor in tests, and localize
      renderer-owned timestamps and group-header count chrome while preserving
      dynamic durations and counts.
    - `src/scrollback/wrappers/mod.rs`: re-export the renderer-private
      localized timestamp helper used by the persistent scrollback owner; no
      backend, transport, or public protocol surface is added.
    - `src/scrollback/blocks/thinking.rs` and
      `src/scrollback/export.rs`: use Rust captured format arguments only.
      Their rendered strings and control flow are byte-for-byte equivalent to
      the fixed formatting expressions.
    - `src/scrollback/entry.rs`: make render-cache eviction test-only, replace
      one cache invariant `unwrap` with a named `expect`, and expose
      `reconstruct_text_drag` so the outer AgentView can ask the fixed
      boundary-aware selection code to reconstruct off-screen drag text
      without receiving the private boundary representation.
    - `src/scrollback/scrollback_pane.rs`: expose one atomic
      `RenderOutputWithSelectionBoundaries` return value and its render entry
      point. This keeps frame geometry and opaque selection boundaries paired;
      it does not move selection interpretation into the application owner.
    - `src/scrollback/state/groups.rs` and
      `src/scrollback/state/layout.rs`: replace bounds-proven `unwrap` calls
      with invariant-specific `expect` diagnostics. Layout additionally
      compiles cache eviction only for tests and redirects its media
      availability test to the local renderer adapter.
    - `src/scrollback/state/nav.rs`: expose an iterator over media paths
      already retained by typed `RenderBlock`s for the fixed link-resolution
      pass; replace a proven-current-turn `unwrap` with a fail-closed
      `let-else`; and compile the follow-preserve inspection only for tests.
      It performs no directory scan or media acquisition.
    - `src/scrollback/state/types.rs`: compile the cache-eviction margin
      constant only with the tests that exercise eviction.
    - `src/search/matcher.rs`: replace both built-in fallback-regex `unwrap`
      calls with exact invariant diagnostics. Query semantics are unchanged.
    - `src/scrollback/render.rs`: redirect Markdown, terminal-hyperlink, and
      URL-opening imports to the renderer-local crates; neutralize source
      fixtures/test names; and use captured format arguments in one assertion.
      Link-map and paint algorithms are unchanged.
    - `src/scrollback/sticky.rs`: add the fixed commit/source-revision/hash
      lineage and use captured format arguments in tests. Production bytes
      otherwise match the fixed sticky-layout source.
    - `src/scrollback/text_selection.rs`: retain the fixed default word
      separators because CrabCode has no corresponding direct-renderer input;
      make the boundary companion opaque-but-public and expose only exact
      full-line/drag reconstruction operations for AgentView. The source
      config lookup is removed rather than creating a second configuration
      authority.
    Every byte-identical file in this graph is listed individually in
    `PROVENANCE.md`; its matching hash and sole fixed source path are enforced
    by the generator.
16. Importing the source graph does not itself assert production-route
    replacement. Production ownership and lifecycle closure remain governed by
    the rendering catalogs and execution tests, and this crate still introduces
    no backend, AppServer, GUI, or protocol authority.
17. `scrollback/blocks/crabcode_projection.rs` is a renderer-private, typed
    product-difference adapter hosted by the fixed `BlockContent`,
    `RenderBlock`, wrapping, fold, selection, search, accent, and
    `ScrollbackState` lifecycle. It consumes only fields already retained by
    CrabCode's read-only projection and never dispatches on a title, prose, a
    JSON key, or a tool-name alias. Its exact source-null variants preserve the
    historical `Message.tsx` default/null branches and the missing-correlation
    null branch in `UserToolResultMessage.tsx`; its SDK-image variants report
    only the already-typed base64/URL/file provenance and acquire no payload,
    filesystem, or media-loading authority.

    The ordinary typed tool variants are renderer field carriers for a
    structurally classified invocation, result, terminal result, or progress
    row. They preserve field semantics and the explicit
    JSON/text/partial-JSON/null/missing distinction, but a complete
    `serde_json::Value` is serialized into a display string; this is not
    original-JSON-byte preservation or a typed-schema closure. They do **not**
    claim parity with the historical CrabCode tool-specific renderer graph.
    That graph resolves each invocation through the existing tool registry and
    input schema before calling
    `userFacingName`, `renderToolUseMessage`, progress renderers, output-schema
    validation, `renderToolResultMessage`, or
    `renderToolUseErrorMessage`. Those per-tool consumers remain a separate RED
    denominator until their existing renderer inputs can be supplied without a
    new backend/wire field. The adapter does not call the fixed
    `ToolCallBlock::from_name`, because doing so would substitute the fixed
    product's tool-name classification for CrabCode's registry semantics.

    The typed `DirectNestedProgress` carrier closes only the historical
    AgentTool/SkillTool presentation rows whose enclosing messages have
    already been classified by CrabCode's existing read-only projection:
    agent prompt, initializing, hidden-tool-use summary, one classified agent
    message, skill hidden-message summary, and the ordered classified blocks
    inside one skill message. It delegates each classified child to the fixed
    `RenderBlock` lifecycle. Agent messages keep their full output; skill
    messages retain the historical one-row outer clip, including one blank row
    when every classified child is source-null. Search/copy text comes from the
    already-retained source text. This carrier does not inspect an open JSON
    value, classify a tool name, correlate messages, or add a backend field.

    No backend operation, protocol field, AppServer route, GUI dependency, or
    tool execution authority is added.

18. `src/side_question_panel.rs` is an adapted, dependency-closed port of the
    fixed `xai-grok-pager/src/views/btw_overlay.rs`, not a new renderer design.
    It retains the fixed three-state side-question presentation, Markdown
    wrapping, twelve-row cap, scroll-position/focus hint, close affordance,
    loading/error chrome, visible/full selection geometry, and hyperlink/plain
    URL overlay. Product-facing names are neutralized and conversions use
    saturating/checked arithmetic. The spinner indexes the renderer tick
    supplied by the existing caller directly instead of applying the fixed
    file's four-tick divisor. The fixed app-owned mutable `HitArea`,
    caller-mutated selection/link accumulators, persistence action, and backend
    correlation are deliberately excluded; one paint returns a
    `SideQuestionPanelRender` value containing only close geometry, links,
    maximum scroll offset, and selection geometry. The existing CrabCode
    `AgentView` consumes that value and remains the sole owner of focus,
    scrolling, dismissal, lifecycle correlation, and request dispatch. The
    local tests cover minimum/narrow geometry, bounded scroll, both link
    sources, visible/full selection, and fail-closed unpaintable layouts. No
    request, response, transport, AppServer, GUI, backend, or protocol
    authority enters this crate.

19. `src/prompt_images.rs` is a deliberately reduced renderer adapter, not a
    claim that all media I/O lives elsewhere. Prompt preview records accept
    caller-supplied bytes/result state and do not fetch them. Scrollback image
    and video references are different: the renderer parses Markdown or bare
    absolute paths already present in transcript text, filters supported
    extensions, verifies regular-file existence, reads and decodes image bytes
    to obtain dimensions, and verifies video-file existence. It does not scan
    directories, follow a backend identifier, fetch a URL, execute a decoder
    process, mutate a session, or define a request/response/wire field. The
    tests bind supported/invalid/missing images, alt-text/deduplication,
    media-only Markdown, bare absolute image paths, and video existence.

20. The remaining files whose bytes changed during the final provenance
    closure have these bounded dispositions:
    - `src/lib.rs` declares the adapted `side_question_panel` module. It adds
      no dependency or authority.
    - `src/modal_window.rs` keeps the fixed modal chrome geometry, close/tab/
      shortcut/fold input outcomes, embedded mode, and tests while replacing
      app-owned theme/config lookups with `CrabCodeTheme` and explicit
      caller-supplied vim state. Hit geometry is cleared through one local
      helper on unpaintable layouts; product modal payloads remain outside.
    - `src/render/osc8.rs` redirects terminal/path helpers to audited local
      modules, replaces the unavailable shell-expansion test dependency with
      `dirs::home_dir`, neutralizes product path fixtures, and uses captured
      constants in the two compiled regex formats. URL/path detection,
      wrapped-row mapping, safety filtering, and OSC 8 presentation are
      otherwise the fixed algorithms.
    - `src/scrollback/blocks/mermaid_content.rs` is the dependency-closed
      Mermaid presentation subset: fixed closed-fence detection, source and
      output-line ranges, theme/quality/width cache identity, static-commit
      suppression, affordance layout, and non-selectable row insertion. The
      local Markdown/theme types replace upstream product types; PNG work
      remains in the outer renderer worker.
    - `src/scrollback/blocks/tool/edit.rs` changes only product-specific test
      paths/theme names and captured assertion formatting. The nine fixed edit
      snapshots and production diff/highlight implementation remain bound by
      the manifest.
    - `src/scrollback/blocks/tool/hook.rs` retains the complete fixed hook
      presentation API before all consumers are wired, neutralizes
      documentation glyph spelling, uses captured format arguments, and adds
      tests for skipped/content gating, every expanded status, output, and
      stop-summary counts. The module-wide dead-code allowance is explicitly
      limited to that staged fixed API; it adds no hook execution.
    - `src/scrollback/blocks/tool/other.rs` redirects the Markdown-code color
      field to its local semantic equivalent and uses a captured label format.
      `src/scrollback/blocks/tool/read.rs` copies only the fixed exact
      `SKILL.md` parent-name recognizer instead of importing the tools crate,
      neutralizes path fixtures, and uses captured range formats.
    - `src/scrollback/minimal.rs` composes the fixed minimal commit and
      live-tail algorithms behind a terminal-insertion callback: committable
      frontier, retry-on-write-failure, fixed display modes, committed
      expansion ring, capped paint, bottom-anchored live tail, and height
      calculation. Process control and terminal insertion remain at the TUI
      owner.

### Typed-tool parity denominator

The typed adapter's fixed lifecycle anchors are
`xai-grok-pager/src/scrollback/block.rs`,
`xai-grok-pager/src/scrollback/blocks/tool/mod.rs`,
`xai-grok-pager/src/scrollback/blocks/tool/other.rs`, and
`xai-grok-pager/src/scrollback/state/mod.rs` at the pinned upstream commit.
The historical CrabCode product-difference anchors are
`src/components/Message.tsx`,
`src/components/messages/AssistantToolUseMessage.tsx`,
`src/components/messages/UserToolResultMessage/UserToolResultMessage.tsx`,
`UserToolSuccessMessage.tsx`, `UserToolErrorMessage.tsx`, and `src/Tool.ts`
at the pinned historical direct-TUI commit.

The historical source denominator contains 40 exact `ToolDef`
objects/factories plus the `createMcpAuthTool` `Tool` factory:
Agent, AskUserQuestion, Bash, Brief, Config, EnterPlanMode, EnterWorktree,
ExitPlanModeV2, ExitWorktree, FileEdit, FileRead, FileWrite, Glob, Grep, LSP,
ListMcpResources, MCP, McpAuth, NotebookEdit, PowerShell, ReadMcpResource,
RemoteTrigger, CronCreate, CronDelete, CronList, SendMessage, Skill,
SyntheticOutput, TaskCreate, TaskGet, TaskList, TaskOutput, TaskStop,
TaskUpdate, TeamCreate, TeamDelete, TodoWrite, ToolSearch, WebFetch,
WebSearch, and TestingPermission. This is a source denominator, not a claim
that every feature-gated tool is active in every direct session.

The adapter currently preserves, without class inference:

- assistant ordinary `tool_use`: exact name plus complete input or streamed
  `partial_json`;
- correlated ordinary user `tool_result`: exact name, result value, and
  optional error flag;
- correlated terminal-tool result: the same exact fields plus the already
  projected terminal kind;
- SDK `tool_progress`: exact tool name plus the already-projected detail;
- exact historical source-null discriminators for unsupported assistant
  result/use blocks, compaction, container upload, and an uncorrelated user
  result;
- historical direct nested progress after the existing projection has already
  classified its children: agent prompt/initializing/hidden-use/message and
  skill hidden-message/message rows, with the skill row's fixed one-line clip.

The following historical behavior remains RED and is not replaced by generic
chrome:

- registry/alias lookup, input-schema validation, `userFacingName`,
  `userFacingNameBackgroundColor`, transparent wrappers, tool-use tags,
  tool-specific use/progress/queued/grouped/result/error/rejection renderers,
  output-schema validation, transcript-mode/verbose differences, and
  per-tool search-text extraction for all 41 definitions/factories;
- cancellation, interruption, rejection, plan-rejection, and classifier
  denial branches whose historical type is encoded only in result text;
- grouped-tool and collapsed read/search composition, which requires the
  historical message lookups, resolved/errored/in-progress sets, progress
  correlation, and renderer-specific grouping state;
- a parent-tool plain string/text result with no typed `ToolPresentation`, a
  non-assistant tool-use source with no retained role discriminator, a
  historical direct special-result block, and advisor invocation dynamic
  input.

Closing those RED rows requires renderer-local ports of the existing registry
presentation descriptors and locally derived transcript correlation. It does
not justify a new backend event, request, response, wire field, transport
route, AppServer dependency, or GUI state.

## Owner-gate adaptation dispositions

The owner gates bind each non-byte-identical prepared target to both its
`SOURCE_MANIFEST.json` local/upstream hashes and the exact disposition below.
Byte-identical prepared targets require no patch disposition and must instead
match their sole upstream bytes.

- `src/scrollback/types.rs` — `SOURCE_SHAPED_RENDER_VALUE_MODEL`
- `src/scrollback/mod.rs` — `COMPLETE_SCROLLBACK_MODULE_TREE_WITH_NARROW_REEXPORTS`
- `src/scrollback/table_geometry.rs` — `DEPENDENCY_CLOSED_TABLE_GEOMETRY`
- `src/scrollback/layout.rs` — `SOURCE_SHAPED_HORIZONTAL_LAYOUT`
- `src/scrollback/sticky.rs` — `DEPENDENCY_CLOSED_STICKY_LAYOUT`
- `src/scrollback/text_selection.rs` — `SOURCE_SHAPED_TEXT_SELECTION_MODEL`
- `src/scrollback/link_map.rs` — `SOURCE_SHAPED_VISIBLE_LINK_MAP`
- `src/scrollback/render.rs` — `DEPENDENCY_CLOSED_SCRATCH_AND_RENDER_OUTPUT_VALUES`
- `src/scrollback/selection.rs` — `DEPENDENCY_CLOSED_POST_RENDER_VALUE_LAYER`
- `src/scrollback/minimal.rs` — `DEPENDENCY_CLOSED_FIXED_MINIMAL_COMMIT_AND_LIVE_LIFECYCLE`
- `src/scrollback/state/layout.rs` — `DEPENDENCY_CLOSED_PAINT_WINDOW_ALGORITHM`
- `src/scrollback/state/mod.rs` — `COMPLETE_SCROLLBACK_STATE_WITH_LOCAL_MEDIA_AND_PROJECTION_MUTATIONS`
- `src/scrollback/state/selection.rs` — `DETERMINISTIC_RENDERER_OWNED_THINKING_VISIBILITY_APPLICATION`
- `src/scrollback/block.rs` — `COMPLETE_BLOCK_CONTENT_CONTRACT_RENDER_BLOCK_ENUM_OPEN`
- `src/scrollback/blocks/mod.rs` — `COMPLETE_BLOCK_MODULE_TREE_WITH_TYPED_CRABCODE_ADAPTER`
- `src/scrollback/blocks/crabcode_projection.rs` — `RENDERER_PRIVATE_TYPED_CRABCODE_PROJECTION_ADAPTER`
- `src/scrollback/blocks/local_command_output.rs` — `RENDERER_PRIVATE_FIXED_HISTORICAL_LOCAL_COMMAND_OUTPUT_ADAPTER`
- `src/scrollback/blocks/system.rs` — `DEPENDENCY_CLOSED_CONCRETE_BLOCK_LEAF`
- `src/scrollback/blocks/workflow.rs` — `DEPENDENCY_CLOSED_CONCRETE_BLOCK_LEAF`
- `src/scrollback/blocks/credit_limit.rs` — `DEPENDENCY_CLOSED_CONCRETE_BLOCK_LEAF`
- `src/scrollback/blocks/tool/lifecycle.rs` — `DEPENDENCY_CLOSED_CONCRETE_TOOL_BLOCK_LEAF`
- `src/scrollback/wrappers/accented.rs` — `DEPENDENCY_CLOSED_RENDER_WRAPPER`
- `src/scrollback/wrappers/padded.rs` — `DEPENDENCY_CLOSED_RENDER_WRAPPER`
- `src/render/osc8.rs` — `SOURCE_SHAPED_CRABCODE_OSC8_ADAPTER`
- `src/audited_appearance/config.rs` — `RUNTIME_VALUE_LAYER_ONLY_CONFIG_AUTHORITY_EXCLUDED`
- `src/inline_media.rs` — `RENDERER_LOCAL_INLINE_MEDIA_CAPABILITY_ADAPTER`
- `src/prompt_images.rs` — `RENDERER_LOCAL_MEDIA_REFERENCE_VALIDATION_AND_PREVIEW_VALUES`
- `src/util.rs` — `DEPENDENCY_CLOSED_DISPLAY_UTILITY`
- `src/scrollback/blocks/mermaid_content.rs` — `DEPENDENCY_CLOSED_DIAGRAM_AFFORDANCE_VALUE`

`SOURCE_MANIFEST.json` is the authoritative full file denominator and digest
map. `scripts/pure-tui-pager-provenance.ts` regenerates it from the pinned
checkout and fails on any unmapped or drifted source file.
