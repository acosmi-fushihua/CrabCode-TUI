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
[`router-for-me/CLIProxyAPI`](https://github.com/router-for-me/CLIProxyAPI),
pinned to release
[`v7.2.71`](https://github.com/router-for-me/CLIProxyAPI/releases/tag/v7.2.71)
and commit
[`5b7f2361ee27d195f6514dde08656f6e4773a9a4`](https://github.com/router-for-me/CLIProxyAPI/commit/5b7f2361ee27d195f6514dde08656f6e4773a9a4).
We thank the upstream maintainers and contributors for the OAuth login and
provider-protocol foundation. Its `LICENSE`, `NOTICE`, `UPSTREAM.lock`, and
component-level third-party notices are retained next to the source. The
CrabCode derivative is not an official Router-For.ME distribution and is not
endorsed by any model service provider.

## JavaScript and registry dependencies

Exact dependency versions are fixed by `bun.lock`, Cargo lockfiles, and
`components/oauthapi-llm/go.sum`. Each dependency remains governed by its
publisher's license. The supplemental source-license bindings under
`third_party/javascript-legal-supplements` are retained for packages whose
published archive did not carry a complete adjacent legal file.

## Prebuilt release runtime

Official platform archives also bundle pinned native runtime artifacts that
are not stored as binaries in this source repository: Bun 1.3.11, ripgrep
14.1.1, crabcode-browser 0.28.0, and the platform-specific Sharp/libvips
payload fixed by `third_party/sharp-native/file-hashes.json`. Every archive
contains the corresponding upstream licenses/notices, the exact artifact URLs
and SHA-256 values, a JavaScript/Rust dependency inventory, and the signed
Account Bridge provenance materials.

Anyone distributing binaries must collect and ship the complete license and
notice set for the exact platform-specific dependency closure in that binary.
