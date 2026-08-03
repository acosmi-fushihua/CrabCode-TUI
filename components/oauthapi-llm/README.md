# OAuthAPI-LLM

OAuthAPI-LLM is CrabCode's isolated Go Account Bridge sidecar. It converts the
Claude Messages boundary used by CrabCode to supported user-authorized account
backends while keeping OAuth credentials and provider protocol code outside the
Rust and TypeScript processes. It is bundled with CrabCode, starts on demand,
and exposes only a process-authenticated, restricted-loopback facade.

## What the Account Bridge provides

- Browser OAuth and device-code login with cancellable, pollable sessions.
- Encrypted credential storage and provider-specific token refresh.
- Connected-account discovery and removal.
- Per-account model discovery, fixed opaque account/model routes, and
  capability projection for tools, thinking modes, vision, JSON mode, context
  windows, and maximum output tokens.
- Provider protocol translation for CrabCode chat turns, including streaming
  and tool-use paths supported by the selected route.
- Route revalidation before use, plus cooldown and provider quota projections
  where the provider exposes them.
- Multiple connected accounts without leaking provider credentials into the
  Rust renderer or TypeScript structured-I/O protocol.

## Supported CrabCode account connectors

| Connector | Authorized account | Flow | Internal provider |
| --- | --- | --- | --- |
| OpenAI | OpenAI / ChatGPT account eligible for Codex | Browser OAuth | `codex` |
| Anthropic | Claude account | Browser OAuth | `claude` |
| Google | Google account usable by Gemini CLI; fixed verified plugin required | Browser OAuth | `gemini-cli` |
| xAI | Grok / xAI account | Device code | `xai` |
| Qwen Code | Qwen Code account | Device code | `qwen` |
| Kimi Code | Kimi Code account | Device code | `kimi` |
| Z Code | Z.AI Coding Plan account | Device code | `zai` |

The connector list is a strict allowlist. A connector is enabled only when the
signed directory, regional grant, terms status, real-account conformance, and
fixed release artifact all authorize it. Models and quota are discovered from
the bundled adapter and connected provider account rather than promised by this
repository; the TUI shows the effective result.

## Use it from CrabCode

End users do not launch this binary directly:

1. Start `crabcode` and run `/model manage`.
2. Open **Local account connections**, start the account runtime if needed,
   and choose **Connect account**.
3. Select a connector. Complete browser authorization, or enter the device
   code shown by the TUI on the provider's verification page.
4. Return to the TUI. It polls the session and, after success, lists selectable
   account/model routes together with account labels and available usage.
5. The same page refreshes accounts/routes/usage and removes local credentials.

Direct upstream-style credential flags such as `--codex-login` are deliberately
rejected in the CrabCode distribution. The private facade is not a supported
public API and the listener must not be exposed to a LAN or the internet. Use
CrabCode's **Custom models** flow for API keys or custom compatible endpoints.

## Upstream relationship

This component is a Go derivative of the MIT-licensed
[`router-for-me/CLIProxyAPI`](https://github.com/router-for-me/CLIProxyAPI),
pinned to upstream
[`v7.2.71`](https://github.com/router-for-me/CLIProxyAPI/releases/tag/v7.2.71)
/ commit
[`5b7f2361ee27d195f6514dde08656f6e4773a9a4`](https://github.com/router-for-me/CLIProxyAPI/commit/5b7f2361ee27d195f6514dde08656f6e4773a9a4).
We gratefully acknowledge the upstream maintainers and contributors who built
the OAuth and provider-protocol foundation. This derivative is not an official
Router-For.ME distribution and does not imply endorsement by any model service
provider.

The pinned upstream project is a general OpenAI/Gemini/Claude/Codex/Grok-
compatible CLI proxy with OAuth and multi-account features. CrabCode does not
expose that entire product surface: generic API-key providers, remote
management, arbitrary plugins, and direct login commands are intentionally
outside this sidecar's supported boundary. See the upstream
[`v7.2.71 README`](https://github.com/router-for-me/CLIProxyAPI/blob/v7.2.71/README.md)
for upstream usage; do not treat this derivative binary as a drop-in upstream
distribution.

## Build and maintenance boundary

This directory is an independent Go module. It must not import CrabCode private
packages outside this directory, must not use cgo or FFI, and must remain
buildable with:

```bash
go test ./...
CGO_ENABLED=0 go build -trimpath -buildvcs=false ./cmd/oauthapi-llm
```

The component is bundled and verified by the same CrabCode release. Runtime
downloads, PATH fallback, remote management, and an unrestricted plugin store
are not supported by the CrabCode distribution.

CrabCode's foundation is Rust and its current business runtime is TypeScript;
this isolated account layer remains Go today. The long-term maintenance target
is an all-Rust product runtime. Migration will happen only after a Rust
replacement matches the current credential isolation, provider compatibility,
signed-artifact verification, state transitions, failure recovery, and test
coverage. Until then this Go module is supported production code, not an
archive.

The public source tree contains no OAuth client secret. If the optional
Antigravity provider is enabled, inject
`CRABCODE_ANTIGRAVITY_OAUTH_CLIENT_SECRET` into the sidecar process at runtime.
Missing configuration fails before a token request is sent. Do not commit or
log the value. Antigravity is not one of the seven shipped CrabCode Account
Bridge login connectors listed above.

See `UPSTREAM.lock`, `DEPENDENCY_LICENSES.json`, `NOTICE`, and
`THIRD_PARTY_NOTICES.md` for provenance and license information.
