# Source provenance

- Repository: `https://github.com/xai-org/grok-build.git`
- Public source commit: `a5727c5960452e7527a154b25cb5bf00cda0545e`
- Monorepo `SOURCE_REV`: `30192d2eef5d91a8fff0e53957de5bd05b43398c`
- Upstream package: `crates/codegen/xai-grok-mermaid`
- Imported source: the six Rust files under `src/`, `tests/pure_engine.rs`,
  `assets/Roboto-Regular.ttf`, and its adjacent `Roboto-LICENSE.txt`
- Upstream repository license Git blob:
  `90b1793cf8eb2d6444863591e8405ecc707dc62d`

The bundled font is byte-identical to the pinned source:

- upstream Git blob:
  `ddf4bfacb396e97546364ccfeeb9c31dfaea4c25`
- SHA-256:
  `4e147ab64b9fdf6d89d01f6b8c3ca0b3cddc59d608a8e2218f9a2504b5c98e14`
- size: `168260` bytes

The adjacent upstream font notice has Git blob
`2c3ec0f1493329191d5ffc827655055de56308c5`. The local notice changes only
the sentence identifying the distributed product from “Grok CLI” to
“CrabCode CLI”; the Google copyright and Apache-2.0 terms are unchanged.

The in-process SVG engine is separately vendored from
`https://github.com/warpdotdev/mermaid-to-svg` at commit
`40cecf2be376e47e15053eadbfb782a531777420`; its own license, upstream
third-party notices, and local modification record are distributed separately.
