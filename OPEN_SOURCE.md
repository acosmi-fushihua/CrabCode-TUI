# Open-source scope and license guide

[简体中文](OPEN_SOURCE.zh-CN.md)

## Source scope

This repository is the source distribution of the CrabCode terminal product.
It includes only code needed by the native TUI, its direct TypeScript backend,
and required local sidecars. The following are deliberately outside scope and
must not be committed:

- desktop, browser-window, mobile, or web GUI source;
- React/Ink application renderers;
- AppServer and shared application communication implementations;
- archived, superseded, migration-only, or research code;
- internal audits, roadmaps, implementation plans, prompts, and agent project
  instruction files;
- credentials, production configuration, signing material, and build output.

This boundary is enforced by `scripts/repository-boundary.mjs` and CI.

## License map

The root [MIT License](LICENSE) applies to original CrabCode TUI code unless a
file or component carries a different notice. In particular:

- `crates/acosmi-util-absolute-path` is adapted from OpenAI Codex and remains
  available under Apache-2.0 as identified in its source headers.
- `crates/crabcode-markdown*`, `crates/crabcode-mermaid`,
  `crates/crabcode-pager-render`, `crates/crabcode-ratatui-inline`, and
  `crates/crabcode-ratatui-textarea` contain derivatives of public xAI and
  Ratatui sources. Their Apache-2.0/MIT notices, provenance, and modification
  records remain adjacent to the code.
- `libs/acosmi-memory` and `libs/acosmi-se` declare Apache-2.0 in their Cargo
  workspaces/components.
- `components/oauthapi-llm` is an MIT-licensed derivative and carries its own
  `LICENSE` and `NOTICE`.
- `third_party` components keep their own license and modification files.
- Registry and npm dependencies remain governed by the licenses declared by
  their publishers and recorded by the relevant lockfiles.

When licenses differ, the component-specific license controls that component.
The MIT license at the repository root does not erase upstream copyright,
NOTICE, patent, attribution, or source-distribution obligations.

## What the license does not provide

The source license does not provide API access, paid-service entitlement,
third-party account permission, signing keys, hosted infrastructure, support,
or a trademark license. Users are responsible for complying with the terms of
every service and model they connect.

## Redistribution

Source redistributions must preserve the root license, this guide, and all
applicable component licenses/notices. Binary distributors must also collect
the licenses for the exact dependency versions and platform artifacts they
ship. [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) is an attribution index,
not a substitute for reviewing the complete dependency closure.
