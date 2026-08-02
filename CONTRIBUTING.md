# Contributing

[简体中文](CONTRIBUTING.zh-CN.md)

Contributions to the terminal product are welcome. Keep every change inside
the repository's pure-TUI boundary.

1. Create a focused branch and avoid unrelated generated files.
2. Install dependencies with `bun install --frozen-lockfile`.
3. Make the smallest coherent change and add tests at the owning layer.
4. Run `bun run check` and the relevant TypeScript, Rust, Go, memory, and search
   tests. Run `bun run ci` before a broad change.
5. Explain behavior, compatibility, security impact, and license/provenance for
   newly vendored code in the pull request.

Do not add GUI/AppServer code, archived implementations, internal plans,
agent-instruction files, secrets, production endpoints, or binary artifacts.
Do not weaken `scripts/repository-boundary.mjs` merely to admit a new product
surface. Propose a separate repository when the work is not part of the TUI.

By contributing, you agree that your contribution is provided under the
license applicable to the files you modify. Preserve all upstream notices.
