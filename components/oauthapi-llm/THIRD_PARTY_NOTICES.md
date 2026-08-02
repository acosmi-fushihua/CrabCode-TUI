# Third-party notices

The authoritative Go build graph is recorded in `go.mod` and `go.sum`; license
conclusions are produced by the fixed scanner described below rather than
inferred from those files. Every release includes this file, `LICENSE`,
`NOTICE`, the generated CycloneDX SBOM, and a
`third-party-licenses.manifest.json`-bound `third-party-licenses/` tree beside
the `oauthapi-llm` binary.

The source baseline derives from
[`router-for-me/CLIProxyAPI`](https://github.com/router-for-me/CLIProxyAPI) at
commit `5b7f2361ee27d195f6514dde08656f6e4773a9a4`; its MIT license is preserved in
`LICENSE`. Transitive dependencies retain their respective licenses.
The build-source `DEPENDENCY_LICENSES.json` pins the five-platform reachable
module graph, approved SPDX IDs, scanner version, a digest-pinned machine report
template and the one reviewed README-based MIT evidence override. Release CI
runs `google/go-licenses@v1.6.0` `check` and `report --template` with
`GOPROXY=off`/`GOWORK=off`, maps every reported reachable import package back
to its Go module, and requires each module's actual scanner `LicenseName` set to
exactly equal the lock and generated SBOM. Incomplete, unknown, duplicate,
unmapped, extra, graph-drifted or disallowed discovery fails closed.
The same pinned scanner's `save` operation collects the license and NOTICE
bytes required for redistribution. The reviewed `go-localereader` README
evidence is added by exact digest because that upstream version has no
standalone license file. Before signing, CI regenerates the complete material
tree from the committed source and module cache and requires its canonical
path/size/SHA-256 manifest to match byte-for-byte; portable verification then
checks every distributed material against the signed manifest.

The fixed in-process connector `gemini-cli` is distributed from
[`router-for-me/cpa-plugin-gemini-cli`](https://github.com/router-for-me/cpa-plugin-gemini-cli)
release `v1.0.5`, commit
`19d9868ffa24e94a2919ea1d1a761afa634de669`. Its MIT license is preserved at
`licenses/gemini-cli-LICENSE`. `UPSTREAM.lock` pins the release archive and
extracted binary SHA-256 for every supported CrabCode platform. Release builds
must verify both digests before staging the plugin, add the staged plugin to the
SBOM and signed provenance, and apply CrabCode's platform signature. The
runtime does not download, update, discover from `PATH`, or accept any other
plugin.

The Qwen connector's OAuth 2.0 device-flow protocol constants (endpoints,
client identifier, scope, PKCE and polling semantics, and the `resource_url`
endpoint derivation) were confirmed against
[`QwenLM/qwen-code`](https://github.com/QwenLM/qwen-code) (Apache-2.0) as a
protocol reference; no code was imported from it.
The Z.AI connector implements the publicly documented ZCode CLI OAuth flow
(init/poll endpoints, coding-plan API-key provisioning, and the
Anthropic-compatible inference endpoint) as an original implementation from
protocol documentation; no third-party code was imported for it.

No provider name or connector entry in this source tree implies endorsement by
that provider.
