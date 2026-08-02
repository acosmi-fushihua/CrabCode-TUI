# OAuthAPI-LLM

OAuthAPI-LLM is CrabCode's isolated Account Bridge sidecar. It converts the
Claude Messages boundary used by CrabCode to supported user-authorized account
backends while keeping OAuth credentials and provider protocol code outside the
Rust and TypeScript processes.

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

The public source tree contains no OAuth client secret. If the optional
Antigravity provider is enabled, inject
`CRABCODE_ANTIGRAVITY_OAUTH_CLIENT_SECRET` into the sidecar process at runtime.
Missing configuration fails before a token request is sent. Do not commit or
log the value.

See `UPSTREAM.lock`, `DEPENDENCY_LICENSES.json`, `NOTICE`, and
`THIRD_PARTY_NOTICES.md` for provenance and license information.
