# CrabCode modifications

Relative to the source identified in `PROVENANCE.md`:

1. The crate, Rust imports, examples, test imports, temporary-directory prefix,
   and product-facing theme examples use neutral CrabCode names. Upstream names
   remain only in attribution and provenance.
2. The dependency on the upstream product-specific TTY utility crate was
   removed. The renderer now uses a local, renderer-scoped process-group
   detach/reap implementation and the existing `nix` dependency on Unix.
3. The `mmdc` path supplies an explicit non-interactive pager/editor
   environment instead of calling the upstream product helper.
4. Unix timeout teardown uses the safe `nix::sys::signal::killpg` wrapper
   instead of a direct `libc::killpg` unsafe block.
5. The raster cap test additionally asserts each output axis is within the
   configured per-dimension limit.
6. `Roboto-LICENSE.txt` changes only the product-identification sentence from
   “Grok CLI” to “CrabCode CLI”; its copyright and license text are unchanged.

The diagram validation, render limits, Mermaid engine interface, pure SVG
engine selection, rasterization algorithm, font bytes, timeout semantics, and
public result/error shapes otherwise retain the pinned implementation.
