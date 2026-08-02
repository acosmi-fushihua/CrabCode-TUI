# Local modifications

Relative to `warpdotdev/mermaid-to-svg` commit
`40cecf2be376e47e15053eadbfb782a531777420`:

1. The standalone renderer binary, snapshot fixtures, reference SVGs, and
   dev-only dependencies were omitted; the library remains.
2. The experimental environment-selected flowchart port is disabled to keep
   rendering deterministic and to retain correct cyclic back-edge routing.
3. Unused `petgraph` and `regex` dependencies were removed; `thiserror` follows
   the workspace major version.
4. Sequence-diagram grammar support was expanded, keyword matching and arrow
   styling were corrected, and corresponding in-source tests were added.
5. Display-width calculations account for East Asian wide characters.
6. Open edge-label parsing and quoted/bracketed label handling were corrected.
7. Unbreakable-token wrapping and bounded identifier-break behavior were
   corrected.
8. Sources were formatted under the workspace formatter.

The exact per-file implementation notes and re-vendor checklist remain in the
header of the local `Cargo.toml`; an upgrade must re-audit every listed change.
