# OAuthAPI-LLM

OAuthAPI-LLM is CrabCode's isolated Account Bridge sidecar. It converts the
Claude Messages boundary used by CrabCode to supported user-authorized account
backends while keeping OAuth credentials and provider protocol code outside the
Rust and TypeScript processes.

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
log the value.

See `UPSTREAM.lock`, `DEPENDENCY_LICENSES.json`, `NOTICE`, and
`THIRD_PARTY_NOTICES.md` for provenance and license information.
