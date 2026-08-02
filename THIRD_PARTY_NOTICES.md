# Third-party notices

This file is an attribution index for source retained in the CrabCode TUI
repository. Component-local license and NOTICE files are authoritative. The
root MIT license does not replace them.

## OpenAI Codex

`crates/acosmi-util-absolute-path` is adapted from
[`openai/codex`](https://github.com/openai/codex) at commit
`5a79dfab7c677cbec43fb1ea53e27c91be3091b3` and is identified in source as
Apache-2.0.

## xAI Grok Build and Ratatui derivatives

The following crates contain source adapted from the public
[`xai-org/grok-build`](https://github.com/xai-org/grok-build) tree and, where
noted locally, Ratatui:

- `crates/crabcode-markdown-core`
- `crates/crabcode-markdown`
- `crates/crabcode-mermaid`
- `crates/crabcode-pager-render`
- `crates/crabcode-ratatui-inline`
- `crates/crabcode-ratatui-textarea`

Their adjacent `LICENSE-Apache-2.0.txt`, `NOTICE`, `PROVENANCE.md`,
`PATCHES.md`, and source manifests preserve the applicable Apache-2.0/MIT
terms, upstream revisions, copyright notices, and local modifications.

## Patched terminal libraries

- `third_party/crossterm-0.28.1-patched` — Crossterm 0.28.1, MIT; see
  `LICENSE` and `MODIFICATIONS.md`.
- `third_party/ratatui-0.29.0-patched` — Ratatui 0.29.0, MIT; see `LICENSE`
  and `MODIFICATIONS.md`.
- `third_party/permutation_iterator-0.1.2-patched` — permutation-iterator
  0.1.2, Apache-2.0; see `LICENSE` and `MODIFICATIONS.md`.

## Mermaid rendering stack

`third_party/mermaid-to-svg` is derived from
`warpdotdev/mermaid-to-svg@40cecf2be376e47e15053eadbfb782a531777420`
under MIT and retains its `LICENSE`, `THIRD_PARTY_NOTICES`, provenance, and
modification record. Its vendored graph crates are:

- `third_party/dagre_rust` 0.0.5 — Apache-2.0 (`LICENCE`)
- `third_party/graphlib_rust` 0.0.2 — Apache-2.0 (`LICENCE`)
- `third_party/ordered_hashmap` 0.0.3 — Apache-2.0 (`LICENCE`)

## Memory and search

The `libs/acosmi-memory` workspace declares Apache-2.0. The
`libs/acosmi-se` workspace/components retain their Apache-2.0 license material
in `libs/acosmi-se/LICENSE`.

## Account Bridge

`components/oauthapi-llm` is an MIT-licensed derivative of
`router-for-me/CLIProxyAPI`. Its `LICENSE`, `NOTICE`, `UPSTREAM.lock`, and
component-level third-party notices are retained next to the source.

## JavaScript and registry dependencies

Exact dependency versions are fixed by `bun.lock`, Cargo lockfiles, and
`components/oauthapi-llm/go.sum`. Each dependency remains governed by its
publisher's license. The supplemental source-license bindings under
`third_party/javascript-legal-supplements` are retained for packages whose
published archive did not carry a complete adjacent legal file.

Anyone distributing binaries must collect and ship the complete license and
notice set for the exact platform-specific dependency closure in that binary.
