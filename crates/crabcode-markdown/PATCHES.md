# CrabCode modifications

Relative to the source identified in `PROVENANCE.md`:

1. The package, crate import, documentation example, and source-path test
   fixture use the neutral `crabcode-markdown` /
   `crabcode-markdown-core` names.
2. Product-facing documentation references use the CrabCode name. Upstream
   attribution remains in `NOTICE`, `PROVENANCE.md`, and the Apache license.
3. The upstream `cfg(fuzzing)` test-helper gate is expressed as the declared
   Cargo feature `fuzzing`, preserving the same opt-in surface while satisfying
   Cargo check-cfg.
4. The upstream pointer-range implementation of `find_substring` is unchanged;
   a function-scoped `allow(unsafe_code)` records the exact exception required
   by the CrabCode workspace's warning-level unsafe-code lint.
5. The Cargo manifest was recreated with a neutral package name and with the
   fixed upstream dependency versions used by this renderer.
6. The `cell_word_separator` lookup spells the `split_whitespace` source
   invariant with `expect` instead of `unwrap`. This preserves the upstream
   success and invariant-violation behavior while satisfying the product
   crate's no-unwrap lint.

No parser, renderer, streaming, LaTeX, hyperlink, color, output, source-map,
syntax-highlighting, URL-scanning, buffer, checkpoint, or Mermaid-rendering
result algorithm was changed.
