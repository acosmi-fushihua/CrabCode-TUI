# CrabCode TUI

[简体中文](README.zh-CN.md)

CrabCode TUI is an open-source, terminal-only coding agent. A native Rust UI
owns the terminal while a directly spawned TypeScript runtime owns the agent
and tool lifecycle. This repository intentionally contains no desktop/web GUI,
React/Ink renderer, AppServer, shared application communication layer, archived
implementation, or internal planning material.

## What is included

| Area | Source |
| --- | --- |
| Native terminal UI and launcher | `crates/crabcode-tui`, `crates/crabcode-cli` |
| Direct agent/tool runtime | `src`, built from `src/entrypoints/tuiRuntime.ts` |
| Cron sidecar | `crates/crabcode-cron` and its Rust dependency closure |
| Memory and search sidecars | `libs/acosmi-memory`, `libs/acosmi-se` |
| Account Bridge | `components/oauthapi-llm` |
| Patched terminal and diagram dependencies | `third_party` and the rendering crates |

The boundary is executable: `bun run check:boundary` rejects forbidden product
surfaces, extra crates/scripts/workflows, unreachable TypeScript source, GUI
dependencies, archives, binaries, and internal agent-project files.

## Requirements

- Bun 1.3.11 or newer
- Rust 1.88 or newer
- Go version declared by `components/oauthapi-llm/go.mod`
- Git and a supported terminal

No service credentials, OAuth tokens, signing keys, or hosted-service access
are included. Configure only accounts and endpoints you are authorized to use.
The optional Antigravity OAuth provider requires
`CRABCODE_ANTIGRAVITY_OAUTH_CLIENT_SECRET` at runtime; it is not required to
build, test, or use other providers. Never commit that value.

## Build from source

```bash
git clone https://github.com/acosmi/CrabCode-TUI.git
cd CrabCode-TUI
bun install --frozen-lockfile
bun run build
```

Individual build targets are also available:

```bash
bun run build:ts
bun run build:rust
bun run build:memory
bun run build:account-bridge
```

For a development run, build the TypeScript runtime and start the Rust TUI with
its explicit test/development runtime seam:

```bash
bun run build:ts
CRABCODE_TUI_RUNTIME_SCRIPT="$PWD/dist/tui-runtime/index.js" \
CRABCODE_TUI_BUN="$(command -v bun)" \
cargo run --manifest-path crates/Cargo.toml \
  -p crabcode-tui --features terminal-lifecycle-tests
```

The production launcher uses a closed sibling layout and performs stricter
generation validation; the command above is for source development.

## Test

```bash
bun run check
bun run test
bun run test:rust
bun run test:memory
bun run test:search
bun run test:account-bridge
bun run smoke:tui
```

`bun run ci` runs the complete local validation set. Some platform-specific
tests require the matching operating system or external sandbox facilities.

## Repository policy

- Product code must be reachable from the native TUI or a required sidecar.
- GUI, AppServer, unified-app communication, archived source, migration
  evidence, internal plans, and agent instruction files are not accepted.
- Generated build output and local audit material must remain untracked.
- New product surfaces require a separate repository, not a hidden branch in
  this source tree.

See [OPEN_SOURCE.md](OPEN_SOURCE.md) for the licensing boundary,
[CONTRIBUTING.md](CONTRIBUTING.md) for changes, and [SECURITY.md](SECURITY.md)
for vulnerability reporting.

## License

Original CrabCode TUI code is available under the MIT License. Some retained
derivative and vendored components remain under Apache-2.0, MIT, or another
license identified next to that component. See [LICENSE](LICENSE),
[OPEN_SOURCE.md](OPEN_SOURCE.md), and
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
