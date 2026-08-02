# Source provenance

## Fixed upstream identity

- Repository: `https://github.com/xai-org/grok-build.git`
- Public source commit: `a5727c5960452e7527a154b25cb5bf00cda0545e`
- Monorepo `SOURCE_REV`: `30192d2eef5d91a8fff0e53957de5bd05b43398c`
- Upstream package: `crates/codegen/xai-ratatui-inline`
- Upstream repository license Git blob:
  `90b1793cf8eb2d6444863591e8405ecc707dc62d`
- Upstream repository license SHA-256:
  `116f7778b9802e569b7fa3a532b17bd80eb13c67837def01eed093d4ea472f28`
- Upstream repository license size: `11388` bytes

`LICENSE-Apache-2.0.txt` is byte-identical to the license at that fixed
revision.

## Complete import denominator

All 10 upstream Rust files were imported:

- seven files under `src/`;
- `tests/segment_differential.rs`;
- `examples/inline.rs`; and
- `benches/bench.rs`.

The upstream `Cargo.toml`, `README.md`, and `NOTICE` were imported as well.
No upstream source, test, example, or benchmark file is omitted.

The seven `src/` files are byte-identical to the fixed source:

- `src/common.rs`:
  `614b4f316fb79cc4290b3ba2e4488e129de30d418c3d31154b7776ddf249915f`
- `src/lib.rs`:
  `123c880ad7d3dead00d83ce2a23305bb3c22d09c574b41f95d83e34438c5f0fa`
- `src/resize.rs`:
  `5d00a208955cbf04a57c8417315f2fc564ef1b094a8fceedcad19e49580881d7`
- `src/scrollback.rs`:
  `aaabba91adfb88da1401c3a9fea1f08c3f7b6c0fe9b312220a1cb34f2cb79f18`
- `src/segment.rs`:
  `a9e3cc612de63c111f0d084da0baaad70e19e5e63f041e272d84336f7d125f2c`
- `src/terminal.rs`:
  `b9819c4a8acd9770f9fb0d4f92786145a5939b037f5a66001426d73bc3b92f71`
- `src/tests.rs`:
  `9ee6c2b294fe0c61f41699c81710ae80739a996213445374b75cb591ac111817`

The integration test, example, and benchmark differ only by the Rust crate
identifier rename from `xai_ratatui_inline` to
`crabcode_ratatui_inline`.

Their upstream and local SHA-256 pairs are:

- `tests/segment_differential.rs`:
  `17d6ff5ba85621a0e26886803c941c318d613794bc08138ec42bbd910751ccad`
  → `f9c8e7558bd7a12d21de7b6ff09cdbe5647e899a205d51af7090c2dc1a6adfa7`
- `examples/inline.rs`:
  `9448f7cc8239c8a8bb6896b01723bc6f06b37d8a9a445c52abaeb66d0dbc49f2`
  → `35a47f76a3ad9e6aaf2bc467f378d19ac902b0fe54a645f6564361e2d3173696`
- `benches/bench.rs`:
  `0dfb0cff2a8cc8b31d28c8752e4d318b5d774d814bf326640896d91793ae11ec`
  → `3d2ac004754bde87bf6bf962dafcbf66d8556938dd94c33554f34e56cf5e0b26`

Package-facing metadata differs only to:

- rename `xai-ratatui-inline` to `crabcode-ratatui-inline`;
- identify the CrabCode package in its README and NOTICE;
- keep upstream authorship in NOTICE/provenance instead of the renamed
  package's `authors` field;
- mark the package as non-publishable;
- mirror the workspace lint policy while allowing only the three audited
  upstream `unsafe` operations at the package boundary, so strict
  `-D warnings` builds do not require modifying the byte-identical Rust
  sources; and
- add a package description.

The terminal, viewport, resize, native-scrollback, ANSI segmentation, and
rendering algorithms are otherwise unchanged. Product-facing code must use
only the `crabcode-ratatui-inline` package and `crabcode_ratatui_inline` Rust
crate identifiers. The upstream name remains only in legal and provenance
text.
