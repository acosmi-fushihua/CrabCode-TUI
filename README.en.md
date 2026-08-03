<p align="center">
  <img src=".github/assets/crab-code-logo.png" width="320" alt="CrabCode Logo">
</p>

<h1 align="center">CrabCode TUI</h1>

<p align="center">An open-source terminal coding agent with a native Rust UI, TypeScript agent runtime, and isolated Go OAuth bridge.</p>

<p align="center"><a href="README.md">简体中文</a> · <a href="https://github.com/acosmi/CrabCode-TUI/releases/latest">TUI releases</a> · <a href="https://acosmi.com/zh/downloads">GUI download</a></p>

CrabCode TUI is CrabCode's terminal-only open-source edition. Rust owns the terminal, rendering, and local process lifecycle; it directly launches the TypeScript business runtime. An isolated Go Account Bridge starts only when account OAuth is needed. This repository contains no desktop/web GUI, React/Ink UI, AppServer, shared-application communication layer, archived implementation, or internal project plans.

## Install

The current stable release is `v1.0.24`. Ordinary installation is one command and does not require GitHub CLI:

macOS / Linux:

```bash
curl -fsSL https://github.com/acosmi/CrabCode-TUI/releases/latest/download/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://github.com/acosmi/CrabCode-TUI/releases/latest/download/install.ps1 | iex
```

The installer verifies the release SHA-256 and package per-file manifest. Users who require additional GitHub build-provenance verification can download the matching assets from [GitHub Releases](https://github.com/acosmi/CrabCode-TUI/releases/latest) and run `gh attestation verify`; that audit flow is no longer presented as the ordinary installer. Release archives support macOS/Linux arm64 and x64 plus Windows x64, and bundle `crabcode`, the native TUI, Bun, memory/cron sidecars, ripgrep, the browser backend, native image libraries, and the Account Bridge.

The open-source TUI and GUI have separate programs, installation roots, and release chains. For state isolation during same-machine testing or multi-product use, set `CRABCODE_CONFIG_DIR` to an absolute dedicated directory; the same authority covers Rust, TypeScript, memory, cron, and renderer diagnostics. Upgrades retain the historical default state location so existing TUI sessions and settings are not silently abandoned.

### GUI (separate product, not open-sourced here)

Download the desktop GUI from the [official CrabCode GUI page](https://acosmi.com/zh/downloads). Its source, project files, application communication implementation, and installers are outside this repository and are not hidden in archives or history branches.

## Account, GO membership, and models

The account entry supports OAuth login. See the [Go OAuth Account Bridge](#go-oauth-account-bridge-capabilities-supported-accounts-and-usage) for the supported third-party model accounts and exact workflow. Registration includes a complimentary **six-month GO subscription membership**, and inviting friends can earn **reset counts**. Current GO membership access includes **DeepSeek-V4, Mino, and Qwen 3.7 Fast**.

**GO is the product membership name, not the Go programming language.** Eligibility, regions, model availability, quotas, reset rules, and service terms follow the live service shown after sign-in. The MIT source license does not grant subscriptions, model quota, third-party APIs, hosted services, or trademark rights.

## Architecture today

| Layer | Current implementation | Responsibility |
| --- | --- | --- |
| Core/foundation | Rust | Terminal ownership, input/rendering, launcher, process supervision, sandbox foundation, memory, search, cron, and local lifecycle |
| Business layer | TypeScript | Agent orchestration, sessions, tools, permission decisions, model and account business logic, compiled into one TUI runtime bundle |
| Account access | Go | Isolated loopback-only Account Bridge for OAuth credentials and provider protocols; no FFI embedding into Rust/TS |

```text
Terminal
  └─ Rust crabcode launcher / native TUI
       ├─ Rust memory, search and cron sidecars
       └─ private structured stdio
            └─ TypeScript agent runtime (bundled Bun)
                 └─ Go Account Bridge (only for OAuth account flows)
```

Rust and TypeScript communicate through a process-private structured stdio protocol, not a GUI/AppServer or shared-app transport. Before login, the package verifies the Account Bridge version, platform, signed provenance, fixed plugins, SBOM, and third-party license materials.

## Go OAuth Account Bridge: capabilities, supported accounts, and usage

`components/oauthapi-llm` is not a remote Go service that users deploy separately, nor is it a general proxy exposing the upstream management API. It is a bundled, on-demand, restricted-loopback private sidecar that:

- starts browser OAuth or device-code authorization and lets the TUI poll the result;
- encrypts OAuth credentials outside the Rust and TypeScript processes, refreshes them, and isolates provider tokens;
- discovers connected accounts and models, then creates a fixed opaque route for each account/model pair;
- translates CrabCode's Claude Messages boundary to provider protocols and publishes per-model metadata for tools, thinking, vision, JSON mode, context windows, and related capabilities;
- revalidates connector, account, route, model capability, cooldown, and quota state before each turn, and shows usage windows/reset times when a provider exposes them; and
- supports multiple connected accounts, route selection, refresh, and local authorization removal.

These are **OAuth/device-code account connections**, not API-key integrations. Available models and quota depend on the bundled adapter, provider state, and connected account plan; the TUI shows the effective result.

| TUI entry | Account being authorized | Flow | Internal Go provider |
| --- | --- | --- | --- |
| OpenAI | An OpenAI / ChatGPT account eligible for Codex | Browser OAuth | `codex` |
| Anthropic | A Claude account | Browser OAuth | `claude` |
| Google | A Google account usable by Gemini CLI, through the fixed verified plugin | Browser OAuth | `gemini-cli` |
| xAI | A Grok / xAI account | Device code: open the verification page and enter the code shown by the TUI | `xai` |
| Qwen Code | A Qwen Code account | Device code | `qwen` |
| Kimi Code | A Kimi Code account | Device code | `kimi` |
| Z Code | A Z.AI Coding Plan account | Device code | `zai` |

To connect an account:

1. Start `crabcode` and run `/model manage` in the chat composer.
2. Choose **Local account connections** → **Start account runtime** if it is not ready → **Connect account**. First use may ask for consent to the regional eligibility check.
3. Pick a connector. Browser flows open the authorization page; device flows show the verification URL, user code, and expiry.
4. Complete authorization and return to the TUI. It polls automatically; after success, select the model route labeled with its connector, account, and available usage.
5. Use the same page to refresh accounts/usage/routes or remove an account and its locally encrypted credentials.

Whether a connector is selectable also depends on the signed connector directory, current region, terms status, real-account conformance, and fixed release artifact. Unavailable connectors remain visible with a disabled reason. API keys and custom compatible endpoints belong under **Custom models**, not Account Bridge.

The CrabCode distribution intentionally disables upstream direct-credential flags and public management surfaces. Do not run upstream-style commands such as `oauthapi-llm --codex-login`, call process-private endpoints directly, or expose the loopback listener. Use upstream CLIProxyAPI itself when you need a standalone general-purpose proxy.

## OAuth upstream source and differences

The `components/oauthapi-llm` sidecar is an MIT-licensed derivative of [`router-for-me/CLIProxyAPI`](https://github.com/router-for-me/CLIProxyAPI), pinned to [`v7.2.71`](https://github.com/router-for-me/CLIProxyAPI/releases/tag/v7.2.71) / commit [`5b7f2361ee27d195f6514dde08656f6e4773a9a4`](https://github.com/router-for-me/CLIProxyAPI/commit/5b7f2361ee27d195f6514dde08656f6e4773a9a4). We thank its maintainers and contributors for the OAuth login and provider-protocol foundation.

The pinned upstream version describes itself as an OpenAI/Gemini/Claude/Codex/Grok-compatible CLI proxy with OAuth, multi-account access, and protocol translation; see its [`v7.2.71 README`](https://github.com/router-for-me/CLIProxyAPI/blob/v7.2.71/README.md). CrabCode retains and hardens only the fixed Account Bridge subset described above. Generic API-key providers, a public compatibility proxy, remote management, arbitrary plugins, and direct CLI login from upstream are not part of CrabCode's public runtime surface.

Local changes include branding, a restricted loopback surface, fixed-account routing, regional/connector policy verification, credential hardening, fixed plugins, and release verification. This derivative is not an official Router-For.ME distribution and implies no model-provider endorsement. See the component [`NOTICE`](components/oauthapi-llm/NOTICE), [`UPSTREAM.lock`](components/oauthapi-llm/UPSTREAM.lock), and [`LICENSE`](components/oauthapi-llm/LICENSE).

## All-Rust destination

The maintainers' long-term destination is an **all-Rust product runtime**. That is a roadmap, not a claim about today: the foundation is Rust, the business layer remains TypeScript, and the OAuth Account Bridge remains Go.

- Prefer Rust for new foundational and cross-layer capabilities.
- Freeze behavior, protocols, state machines, and security boundaries before replacing TS/Go pieces.
- Move OAuth only after credential isolation, provider compatibility, provenance checks, and recovery meet or exceed current behavior.
- Remove Bun or Go only after parity, regression coverage, and rollback paths are complete.

The TypeScript and Go code are therefore supported production transition implementations, not archived code.

Migration is delivered by frozen behavioral boundary rather than mechanical file-by-file rewriting. The current TypeScript-to-Rust implementation register is:

| Behavioral boundary | TypeScript authority | Rust parity owner / migration destination |
| --- | --- | --- |
| StructuredIO, process lifecycle, backpressure, correlation, shutdown | `src/entrypoints/sdk`, `src/cli/structuredIO.ts` | `crates/crabcode-tui/src/sdk_runtime.rs`, `runtime_host.rs` |
| Message, stream, progress, attachment, and result projection | `src/types`, `src/Tool.ts` | `sdk_projection.rs`, `scrollback_projection.rs`, `agent_view.rs` |
| Startup, workspace trust, onboarding, and session picking | `src/cli/crabcodeTuiBridgeProtocol.ts` | `tui_app.rs`, `session_picker.rs`, `terminal.rs` |
| Model, account, usage, plugin, and retained commands | `src/cli/directTui*Actions.ts` | `sdk_runtime.rs`, `model_management.rs`, `usage_plugin_management.rs`, `retained_command_surface.rs` |
| Terminal input, drawing, suspend/resume, and exit | TS supplies business events only | `app_event_loop.rs`, `terminal.rs`, `tui_app.rs`, `tui_ui.rs` |

`bun run check:capabilities` expands the current public and process-private TypeScript unions with the TypeScript compiler and generates `crates/crabcode-tui/src/generated_renderer_contract.rs`. Every protocol family records an explicit `rustOwner`; adding a TS branch without a reviewed Rust owner fails the check or native tests instead of creating silent migration drift.

## Included source

| Area | Source |
| --- | --- |
| Native terminal UI and pure launcher | `crates/crabcode-tui`, `crates/crabcode-cli` |
| Direct agent/tool runtime | `src`, with the sole entry at `src/entrypoints/tuiRuntime.ts` |
| Cron sidecar | `crates/crabcode-cron` and its Rust dependency closure |
| Memory and search | `libs/acosmi-memory`, `libs/acosmi-se` |
| OAuth Account Bridge | `components/oauthapi-llm` |
| Patched/pinned terminal, diagram, and platform dependencies | `third_party` and rendering crates |
| Build, verification, and releases | `scripts`, `.github/workflows` |

`bun run check:boundary` fail-closes on GUI/AppServer/Ink paths, extra crates/scripts/workflows, unreachable TypeScript source, archives and binaries, internal plans, and project instruction files such as `AGENTS.md` or `CLAUDE.md`.

## Build from source

Prebuilt-package users do not need these tools. Source development requires Bun 1.3.11+, Rust 1.92+, the Go version declared in `components/oauthapi-llm/go.mod`, and Git:

```bash
git clone https://github.com/acosmi/CrabCode-TUI.git
cd CrabCode-TUI
bun install --frozen-lockfile
bun run build
```

Individual targets:

```bash
bun run build:ts
bun run build:rust
bun run build:memory
bun run build:account-bridge
```

Development launch:

```bash
bun run build:ts
CRABCODE_TUI_RUNTIME_SCRIPT="$PWD/dist/tui-runtime/index.js" \
CRABCODE_TUI_BUN="$(command -v bun)" \
cargo run --manifest-path crates/Cargo.toml \
  -p crabcode-tui --features terminal-lifecycle-tests
```

The production launcher accepts only a closed runtime layout within one immutable version directory; the environment seam above exists only under the test/development feature.

## Verification

```bash
bun run check
bun run test
bun run test:rust
bun run test:memory
bun run test:search
bun run test:account-bridge
bun run smoke:tui
```

`bun run ci` runs the full local validation. Release CI additionally builds on five native platforms, verifies Account Bridge signatures, writes a per-file manifest, collects dependency licenses, validates the install layout, and produces SHA-256 files plus GitHub build provenance for release assets.

## Open source and licenses

Original CrabCode TUI code is under the [MIT License](LICENSE). Derivative and vendored components retain their Apache-2.0, MIT, or other component-specific terms; release packages include exact dependency licenses and a materials inventory. See the [open-source scope](OPEN_SOURCE.md), [third-party notices](THIRD_PARTY_NOTICES.md), [contribution guide](CONTRIBUTING.md), and [security policy](SECURITY.md).
