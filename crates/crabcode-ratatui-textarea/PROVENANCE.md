# Source provenance

## Fixed upstream identity

- Repository: `https://github.com/xai-org/grok-build.git`
- Public source commit: `a5727c5960452e7527a154b25cb5bf00cda0545e`
- Monorepo `SOURCE_REV`: `30192d2eef5d91a8fff0e53957de5bd05b43398c`
- Upstream package: `crates/codegen/xai-ratatui-textarea`
- Upstream repository license Git blob:
  `90b1793cf8eb2d6444863591e8405ecc707dc62d`
- Upstream repository license SHA-256:
  `116f7778b9802e569b7fa3a532b17bd80eb13c67837def01eed093d4ea472f28`
- Upstream repository license size: `11388` bytes

`LICENSE-Apache-2.0.txt` is byte-identical to the license at that fixed
revision.

## Complete import denominator

All 13 upstream Rust files were imported:

- seven non-test library files under `src/`;
- five editor test files under `src/editor_tests/`; and
- `examples/textarea_demo.rs`.

The upstream `.gitignore`, `Cargo.toml`, and `NOTICE` were imported as well.
No upstream source, test, or example file is omitted.

Ten of the 12 Rust files under `src/` are byte-identical to the fixed source.
`src/textarea.rs` retains the complete fixed source plus one reviewed
renderer-local production insertion and its test-only evidence insertion, as
described below. `src/lib.rs` differs only by crate-level Clippy allowances
added next to the allowance already present upstream: CrabCode denies
`clippy::unwrap_used` workspace-wide, while the fixed source contains three
production unwraps guarded by explicit position bounds and additional unwraps
in its tests; current Clippy also diagnoses the upstream test-only
single-range fixture syntax. No runtime statement or algorithm was changed.
The example differs only by the Rust package/crate identifier rename and a
crate-level allowance for its one upstream `unwrap()`.

The byte-identical `src/` SHA-256 values are:

- `src/editor.rs`:
  `7d225bb2992d9cf79305e073a2d4a03298dba4302fb191ead638c644c1ce8d35`
- `src/editor_keys.rs`:
  `b4f80746cc7848590d463519cb1bbf7a4dc16c5236394d2eecb951867fbc5c0b`
- `src/editor_tests/editing.rs`:
  `84a5c2ecfec31a9e1d2eb76002b3b5d665819dcd8ebc5698fb141004b521ecfa`
- `src/editor_tests/keys.rs`:
  `8daf00e6bf481dc8d54de621291b99cf56f3065a2d9c54a1d3807ba8bbbd30a1`
- `src/editor_tests/mod.rs`:
  `d36f5f756576e71f134b80c1ac134c7a1410b50f70c94d526a685fbadd664775`
- `src/editor_tests/planning.rs`:
  `f767f36a79a33b089c3f9fd69cf69b69bee9948adce9a04d3039645624f0f136`
- `src/editor_tests/viewport.rs`:
  `4e15c696a974f5c92c19974853bc0889bec9167c57ccee2f87da2f7417b2dba9`
- `src/render/line_utils.rs`:
  `b4fbe4f6596e329f4733d1612cb9bfb7f7fdc48597fc56e2726022a9b606655e`
- `src/render/mod.rs`:
  `af08ce0a2e9921421ce1893e67f00ca6981af70ea261321139daa2c9141bfe22`
- `src/wrapping.rs`:
  `5c5ffa630062a895da0b2ec12b2357e1ea50aeb84c82961d913cfc68d360a0bd`

The textarea's fixed-upstream and reviewed local SHA-256 pair is:

- `src/textarea.rs`:
  `2a8915174fd076d5e0599e1b7f1dc8dab5014d1262dc6105ddeea0cd4f037bbe`
  → `5f865059e40ba1a3fa9943c6f1da1974a4a06cb022df3f6aa24472770e76a548`

The local file preserves the fixed `set_text` bytes exactly. Its only
production addition is the 883-byte
`cancel_transient_mouse_interaction` method (SHA-256
`2173d5279f557420e44be5ed9e6251c3d7ead2b847e907ce5db7435f22d93c10`),
which lets the renderer retire press, drag, hover and click-cadence state when
focus or terminal ownership changes without clearing document or viewport
state. The accompanying 1969-byte test-only insertion has SHA-256
`eeec31a1c22e03874d1d5d5d46d7e4b6ff4cd75e4bc53595237ba7b441e8fb24`.
The lifecycle lineage gate occurrence-checks and hashes both insertions,
deletes them from the target in memory, and requires the complete normalized
file to equal the fixed-upstream bytes. Any replacement or additional byte
outside those two insertions fails closed.

The library root's upstream and minimally adapted local SHA-256 pair is:

- `src/lib.rs`:
  `369508a3f05668a6e89da4359d8e0c40fd9a5e5f7dde5c6a85f366e762661024`
  → `dc3d5bb9a867a3a8dd6713c759db24cf83b58b432473f2e8c65de90b4618d908`

The example's upstream and renamed local SHA-256 pair is:

- `examples/textarea_demo.rs`:
  `c1749c324acc7ef7b2fe21c20448087debf3837cde39d13e70c511f8acf7eb6c`
  → `7e5b9e292f671ec5b087fcbe0939e39d16e72e2c6a894d3593ec59f1f09e4a68`

Package-facing metadata differs only to:

- rename the package to `crabcode-ratatui-textarea`;
- identify the CrabCode package in its description and NOTICE;
- mark the package as non-publishable and inherit the workspace Rust version;
- declare the same dependency versions directly where the CrabCode workspace
  does not expose those dependencies; and
- retain upstream authorship and brand names only in legal/provenance text.

The fixed editor, wrapping, viewport, input, clipboard, element, rendering and
scrollbar algorithms are otherwise unchanged. The renderer-local transient
mouse retirement method changes only terminal-generation and focus-lifecycle
cleanup; it adds no backend field, wire protocol, AppServer route or GUI
surface. Product-facing code must use only the
`crabcode-ratatui-textarea` package and `crabcode_ratatui_textarea` Rust crate
identifiers.
