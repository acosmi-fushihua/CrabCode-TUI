# Source provenance

- Repository: `https://github.com/xai-org/grok-build.git`
- Public source commit: `a5727c5960452e7527a154b25cb5bf00cda0545e`
- Monorepo `SOURCE_REV`: `30192d2eef5d91a8fff0e53957de5bd05b43398c`
- Upstream package: `crates/codegen/xai-grok-markdown-core`
- Imported source: `src/lib.rs`
- Imported executable unit-test denominator: 45 `#[test]` cases
- Upstream license: Apache-2.0, copied verbatim as
  `LICENSE-Apache-2.0.txt`

The repository pin is also machine-recorded in
`scripts/rust-tui-parity/upstream-source-pin.json`. The verification script
`scripts/rust-tui-parity/verify-markdown-source-parity.sh` rejects any other
commit or `SOURCE_REV`, compares the source after the documented neutral-name
transformation, and checks the test denominator.
