# Source provenance

## Fixed upstream identity

- Repository: `https://github.com/xai-org/grok-build.git`
- Public source commit: `a5727c5960452e7527a154b25cb5bf00cda0545e`
- Monorepo `SOURCE_REV`: `30192d2eef5d91a8fff0e53957de5bd05b43398c`
- Primary upstream package:
  `crates/codegen/xai-grok-pager-render`
- Additional upstream rendering primitives:
  `crates/codegen/xai-ratatui-inline`
- Additional upstream pager presentation primitives:
  `crates/codegen/xai-grok-pager`
- Upstream repository license Git blob:
  `90b1793cf8eb2d6444863591e8405ecc707dc62d`
- Upstream repository license SHA-256:
  `116f7778b9802e569b7fa3a532b17bd80eb13c67837def01eed093d4ea472f28`
- Upstream repository license size: `11388` bytes

`LICENSE-Apache-2.0.txt` is byte-identical to that fixed repository license.
The same commit and source revision are pinned in
`scripts/rust-tui-parity/upstream-source-pin.json`.

Upstream product names in this file, `NOTICE`, and `SOURCE_MANIFEST.json` are
legal/source attribution only. They are not executable CrabCode product
identifiers.

## Complete machine-checked denominator

`SOURCE_MANIFEST.json` covers every source and checked-in test-fixture file
under `src/`, every runtime asset under `assets/`, plus this crate's
`Cargo.toml`: 143 files in total. For every file it records:

- the local path, byte length, and SHA-256;
- whether the file is byte-identical, adapted, a local adapter, or a local
  composite;
- every fixed upstream path used as a source anchor; and
- each upstream path's Git blob, byte length, and SHA-256.

The manifest was generated and byte-checked against the pinned checkout with:

```text
bun scripts/pure-tui-pager-provenance.ts \
  --upstream-root /absolute/path/to/pinned-checkout
bun scripts/pure-tui-pager-provenance.ts \
  --check --upstream-root /absolute/path/to/pinned-checkout
```

The generator rejects a missing or extra local source file, a symlink, a
commit or `SOURCE_REV` mismatch, an invalid byte-identical classification, or
any local byte drift.

## Source classification

The 27 byte-identical files are:

- `src/audited_appearance/render_mermaid.rs`
- `src/audited_appearance/scroll_mode.rs`
- `src/audited_appearance/text_selection.rs`
- `src/audited_render/line_utils.rs`
- `src/audited_render/renderable.rs`
- `src/audited_render/safe_buf.rs`
- `src/render/highlight.rs`
- `src/render/image_overlay.rs`
- `src/render/image_overlay/content.rs`
- `src/render/image_overlay/geometry.rs`
- `src/scrollback/blocks/btw.rs`
- `src/scrollback/blocks/tool/list_dir.rs`
- `src/scrollback/blocks/tool/snapshots/crabcode_pager_render__scrollback__blocks__tool__edit__tests__diff_basic.snap`
- `src/scrollback/blocks/tool/snapshots/crabcode_pager_render__scrollback__blocks__tool__edit__tests__diff_basic_dual.snap`
- `src/scrollback/blocks/tool/snapshots/crabcode_pager_render__scrollback__blocks__tool__edit__tests__diff_merged_hunks_gap_markers.snap`
- `src/scrollback/blocks/tool/snapshots/crabcode_pager_render__scrollback__blocks__tool__edit__tests__diff_multiple_hunks.snap`
- `src/scrollback/blocks/tool/snapshots/crabcode_pager_render__scrollback__blocks__tool__edit__tests__diff_multiple_hunks_dual.snap`
- `src/scrollback/blocks/tool/snapshots/crabcode_pager_render__scrollback__blocks__tool__edit__tests__diff_reflow.snap`
- `src/scrollback/blocks/tool/snapshots/crabcode_pager_render__scrollback__blocks__tool__edit__tests__diff_reflow_dual.snap`
- `src/scrollback/blocks/tool/snapshots/crabcode_pager_render__scrollback__blocks__tool__edit__tests__diff_three_digit_lines.snap`
- `src/scrollback/blocks/tool/snapshots/crabcode_pager_render__scrollback__blocks__tool__edit__tests__diff_three_digit_lines_dual.snap`
- `src/scrollback/blocks/tool/web_fetch.rs`
- `src/scrollback/blocks/tool/web_search.rs`
- `src/scrollback/wrappers/block_renderer.rs`
- `src/search/mod.rs`
- `src/terminal/overlay.rs`
- `src/timeline.rs`

The 100 adapted files/assets are:

- `assets/crabcode-dark.tmTheme`
- `assets/crabcode-light.tmTheme`
- `src/appearance.rs`
- `src/audited_appearance/config.rs`
- `src/audited_glyphs.rs`
- `src/audited_host/display_refresh.rs`
- `src/audited_host/mod.rs`
- `src/audited_modal_window_state.rs`
- `src/audited_render/color.rs`
- `src/audited_render/preview_overlay.rs`
- `src/audited_render/tool_paths.rs`
- `src/audited_render/video_overlay.rs`
- `src/audited_render/wrapping.rs`
- `src/audited_terminal/embedded_editor.rs`
- `src/audited_terminal/hyperlinks.rs`
- `src/audited_terminal/keyboard.rs`
- `src/audited_terminal/mod.rs`
- `src/audited_terminal/probe.rs`
- `src/audited_terminal/test.rs`
- `src/audited_terminal/tmux_probe.rs`
- `src/audited_terminal/xtversion.rs`
- `src/audited_theme/cache.rs`
- `src/audited_theme/color_support.rs`
- `src/audited_theme/osc11.rs`
- `src/audited_theme/system_appearance.rs`
- `src/diff.rs`
- `src/inline_media.rs`
- `src/input/line_editor.rs`
- `src/input/mod.rs`
- `src/input/mouse.rs`
- `src/input/mouse/tests.rs`
- `src/input/scroll_log.rs`
- `src/link_opener.rs`
- `src/mcp_display.rs`
- `src/modal_window.rs`
- `src/picker.rs`
- `src/picker_line_editor.rs`
- `src/picker_scrollbar.rs`
- `src/picker_shortcuts.rs`
- `src/prompt_images.rs`
- `src/render/image_overlay/tests.rs`
- `src/render/osc8.rs`
- `src/scrollback/block.rs`
- `src/scrollback/blocks/agent.rs`
- `src/scrollback/blocks/bg_task.rs`
- `src/scrollback/blocks/credit_limit.rs`
- `src/scrollback/blocks/context_info.rs`
- `src/scrollback/blocks/markdown_content.rs`
- `src/scrollback/blocks/mermaid_content.rs`
- `src/scrollback/blocks/mod.rs`
- `src/scrollback/blocks/quote_bar.rs`
- `src/scrollback/blocks/session_event.rs`
- `src/scrollback/blocks/subagent.rs`
- `src/scrollback/blocks/system.rs`
- `src/scrollback/blocks/thinking.rs`
- `src/scrollback/blocks/tool/execute.rs`
- `src/scrollback/blocks/tool/edit.rs`
- `src/scrollback/blocks/tool/hook.rs`
- `src/scrollback/blocks/tool/lifecycle.rs`
- `src/scrollback/blocks/tool/memory_search.rs`
- `src/scrollback/blocks/tool/mod.rs`
- `src/scrollback/blocks/tool/other.rs`
- `src/scrollback/blocks/tool/read.rs`
- `src/scrollback/blocks/tool/search.rs`
- `src/scrollback/blocks/tool/search_tool.rs`
- `src/scrollback/blocks/tool/use_tool.rs`
- `src/scrollback/blocks/user.rs`
- `src/scrollback/blocks/workflow.rs`
- `src/scrollback/entry.rs`
- `src/scrollback/export.rs`
- `src/scrollback/layout.rs`
- `src/scrollback/link_map.rs`
- `src/scrollback/mod.rs`
- `src/scrollback/render.rs`
- `src/scrollback/scrollback_pane.rs`
- `src/scrollback/search.rs`
- `src/scrollback/selection.rs`
- `src/scrollback/state/layout.rs`
- `src/scrollback/state/mod.rs`
- `src/scrollback/state/groups.rs`
- `src/scrollback/state/nav.rs`
- `src/scrollback/state/selection.rs`
- `src/scrollback/state/timeline.rs`
- `src/scrollback/state/types.rs`
- `src/scrollback/state/verb_group.rs`
- `src/scrollback/sticky.rs`
- `src/scrollback/table_geometry.rs`
- `src/scrollback/text_selection.rs`
- `src/scrollback/types.rs`
- `src/scrollback/wrappers/accented.rs`
- `src/scrollback/wrappers/entry_renderer.rs`
- `src/scrollback/wrappers/mod.rs`
- `src/scrollback/wrappers/padded.rs`
- `src/search/matcher.rs`
- `src/side_question_panel.rs`
- `src/syntax.rs`
- `src/terminal/image.rs`
- `src/terminal_output.rs`
- `src/theme/md_style.rs`
- `src/util.rs`

The twelve local adapters are `Cargo.toml`, `src/lib.rs`,
`src/context_visualization.rs`, `src/text_safety.rs`, the three audited module
roots `src/audited_appearance/mod.rs`, `src/audited_render/mod.rs`, and
`src/audited_theme/mod.rs`, plus `src/render/mod.rs`,
`src/scrollback/blocks/crabcode_projection.rs`,
`src/scrollback/blocks/local_command_output.rs`, `src/terminal/mod.rs`, and
`src/theme.rs`. The local-command block is a renderer-only adapter to the
fixed historical CrabCode `UserLocalCommandOutputMessage`; it consumes only
the existing tagged display payload and introduces no command, backend, or
protocol authority.

The four local composites are:

- `src/audited_theme/engine.rs`, whose fixed anchors are the upstream theme
  model plus the complete neutral day/night mother palettes. The only product
  overrides are the exact historical CrabCode direct-TUI semantic matches
  classified by `PRODUCT_ROLE_AUDIT`; every unproven role retains its mother
  value.
- `src/scrollback/minimal.rs`, whose fixed anchors are the pinned minimal
  commit and live-tail modules. It keeps the commit frontier, retry, display,
  capped-paint, tail-height, and draw lifecycle in one dependency-closed
  renderer module.
- `src/tui_render.rs`, whose fixed anchors are the inline `common.rs` and
  `segment.rs` primitives plus pager-render `appearance/scroll_mode.rs`,
  `render/osc8.rs`, `render/safe_buf.rs`, `render/scrollbar.rs`,
  `render/terminal_output.rs`, and `render/wrapping.rs`.
- `src/shell_command.rs`, whose fixed anchors are the pager permission-command
  display, task-pane shell highlighter, and workspace tree-sitter Bash
  splitting implementation. It owns display parsing only and contains no
  permission, policy, execution, or backend authority.

Their complete fixed-upstream blob and digest identities are in
`SOURCE_MANIFEST.json`.

The adapted `src/side_question_panel.rs` is anchored to the fixed
`xai-grok-pager/src/views/btw_overlay.rs`. It is not a newly designed panel:
it retains that source's `Loading`/`Done`/`Error` renderer state, bounded
twelve-row Markdown body, focus/scroll hint, always-visible close geometry,
visible and full-response selection models, and Markdown/plain-link overlay.
The local adapter returns geometry, selection, and links as renderer-owned
values instead of importing the fixed app's mutable `HitArea`; the existing
CrabCode `AgentView` owns focus, dismissal, correlation, and request dispatch.
The exact local reductions and safety substitutions are listed in
`PATCHES.md`.

The adapted `src/prompt_images.rs` deliberately has two different media
boundaries. Prompt preview bytes remain caller-supplied renderer values.
Scrollback reference extraction, however, performs local validation of paths
already present in transcript text: extension filtering, regular-file
existence, image-byte reading/decoding for dimensions, and video-file
existence. It performs no filesystem discovery, remote fetch, request
dispatch, backend mutation, or protocol expansion. `PATCHES.md` records this
distinction explicitly.

The scrollback source graph contains the fixed presentation owners
`RenderBlock`, `ToolCallBlock`, `ScrollbackEntry`, `ScrollbackState`,
`ScrollbackPane`, the scroll render pass, search, timeline, entry wrappers,
edit rendering and their dependency-closed presentation leaves. Fixed files
that required no source change, including timeline rendering and the nine edit
snapshots, are classified byte-identical even when the local snapshot filename
uses the CrabCode crate prefix. `ScrollbackPane`, entry, export, navigation,
group scanning, state constants, thinking summaries, and search matching are
classified adapted because their current bytes contain explicit cross-crate
visibility, test-only reachability, invariant diagnostics, or
captured-format-spelling changes. Adapted files, local composites, and every
local transformation are enumerated in `PATCHES.md`; the manifest binds both
sides of each mapping to their fixed SHA-256 values.

The adapted `ScrollbackState` owner has exactly three additional renderer-only
integration methods: atomic full-entry reordering by stable `EntryId`,
in-place replacement of an already proven `RenderBlock` while retaining
identity and view state, and deterministic application of the existing
thinking-block presentation state. Replacement delegates running-to-final
completion to the fixed lifecycle; the deterministic setter delegates to the
same current/future thinking-block state transition as the fixed toggle.
The first two methods' four named unit tests and all three methods' exact local
behavior are listed in `PATCHES.md`; none defines a request, transport field,
backend operation, or protocol authority.

This source graph imports no session, tool execution, permission,
configuration, telemetry, AppServer, GUI, or backend authority. Whether and
where a production TUI route consumes these owners is a separate integration
claim and is not inferred from their presence in this crate.

This provenance establishes source identity and classification. It does not
by itself assert behavioral parity; behavioral evidence is maintained by the
rendering parity catalogs and tests.
