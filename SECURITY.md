# Security policy

[简体中文](SECURITY.zh-CN.md)

Please report suspected vulnerabilities through GitHub's private security
advisory feature for this repository. Do not open a public issue containing an
exploit, credential, private account data, or an unpatched vulnerability.

Include the affected revision, platform, reproduction steps, impact, and the
minimum evidence needed to validate the report. Use synthetic accounts and
redact tokens, cookies, keys, logs, and personal data.

Only the current `main` branch is supported. This source repository does not
provide an SLA, hosted service, signing key, or entitlement to a third-party
service. Reports about upstream dependencies may also need to be sent to the
upstream maintainer.

## Release trust boundary

Production releases accept only signed annotated `v*` tags whose tagger is
`crabcode-release@acosmi.com`. The SSH Ed25519 public key is supplied through
the protected repository variable `RELEASE_TAG_SIGNER_SSH_ED25519`; the
private key never enters source, packages, or workflow logs. Repository
administration must enforce the `release-tags` ruleset (creation limited to the
release-owner role; deletion and force updates forbidden) and the
`production-release` environment with two required reviewers.

The workflow builds and replays all five platform packages before entering
that environment, creates an immutable draft, and verifies GitHub build
attestations plus server-side SHA-256 for exactly eight assets before making
the release public/latest. `release-manifest.digest.json` is only an internal
file-inventory hash binding; it is deliberately not described as a signature.
