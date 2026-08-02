# Sanitized provider quota fixtures

These fixtures preserve the provider response shape but contain no real account,
token, project, billing, or subscription data.

- `openai-usage.json` follows the generated `RateLimitStatusPayload`,
  `RateLimitStatusDetails`, `AdditionalRateLimitDetails`, and spend-control
  models plus the `/wham/usage` path in the official `openai/codex` source.
  The official backend client maps the top-level limit and spend control to
  the default `codex` bucket, while additional limits remain separate buckets
  keyed by `metered_feature`; this fixture exercises that boundary. Evidence
  is pinned to `openai/codex` commit
  `5bed6447998c754d154dbd796517310b8f04d4ce` (`codex-rs/codex-backend-openapi-models/src/models/{rate_limit_status_payload,rate_limit_status_details,additional_rate_limit_details,spend_control_status_details,spend_control_limit_details}.rs`,
  `codex-rs/backend-client/src/{client.rs,client/rate_limit_resets.rs}`).
- `anthropic-usage.json` follows the `/api/oauth/usage` window names and
  `utilization` / `resets_at` fields in the official
  `@anthropic-ai/claude-code-darwin-arm64` 2.1.209 artifact.
- `google-user-quota.json` follows `RetrieveUserQuotaResponse` / `BucketInfo`
  in the official `google-gemini/gemini-cli` source
  (`packages/core/src/code_assist/types.ts`). In particular,
  `remainingAmount` is not treated as a percentage or limit.
- `xai-billing.json` follows the read-only billing response fields used by the
  official `@xai-official/grok-darwin-arm64` 0.2.101 artifact's
  `extensions/billing.rs` implementation.
