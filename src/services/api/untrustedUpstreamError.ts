/**
 * Untrusted-upstream error wrapper.
 *
 * Custom-model and local-model chat-stream adapters dial a third-party /
 * self-hosted HTTP endpoint. When that endpoint returns a non-2xx status the
 * adapter reads the response BODY for local debugging — but that body is
 * UNTRUSTED text controlled by the remote provider and must never leak into an
 * outbound telemetry field (Datadog `tengu_api_error`, OTel `api_error`, beta
 * tracing spans).
 *
 * This wrapper carries TWO representations:
 *   - `error.message` — the FULL message including the body, for LOCAL-only
 *     debug sinks (in-memory error log, `--debug` files).
 *   - `safeMessage` — a redacted form with ONLY the status code and the adapter
 *     label, NO body. Outbound telemetry sinks must use this.
 *
 * Detection uses a BRAND PROPERTY, not `instanceof`: the worker bundle is
 * source-free / minified, so a single module identity is not guaranteed across
 * bundle copies and `instanceof` across copies is fragile. A plain own-property
 * brand survives bundling and structured boundaries.
 */

export const UNTRUSTED_UPSTREAM_BRAND = 'crabcode.untrustedUpstreamBody'

export class UntrustedUpstreamError extends Error {
  /** HTTP status code returned by the untrusted upstream endpoint. */
  public readonly statusCode: number
  /** Redacted message safe for outbound sinks — status only, no body. */
  public readonly safeMessage: string

  constructor({
    adapterLabel,
    status,
    detail,
  }: {
    adapterLabel: string
    status: number
    detail: string
  }) {
    // W-CUSTOM-MODEL-PLUS-GATE PR-4（裁决② 配套）：400 + context/length/token
    // 指纹 → 附一句可行动指引（纯文案，不自动改配）。裁决②把默认声明提到 1M，
    // 声明高于端点真实窗口的失败形态就是这里——给用户一条明确的出路。
    // The hint is OUR constant copy (not body-derived), so appending it to the
    // local-only full message keeps `safeMessage` untouched for telemetry.
    const contextOverflowHint =
      status === 400 && /context|length|token/i.test(detail)
        ? '（若为上下文超限：请在 /model manage 或桌面版设置中调低该模型的上下文窗口声明）'
        : ''
    // FULL message (with detail) — only ever read by LOCAL-only debug sinks.
    super(
      `${adapterLabel}: provider returned HTTP ${status}` +
        (detail ? `: ${detail}` : '') +
        contextOverflowHint,
    )
    this.name = 'UntrustedUpstreamError'
    this.statusCode = status
    // SAFE message — status code only, NO untrusted body.
    this.safeMessage = `${adapterLabel}: provider returned HTTP ${status}`
    // Brand property (not instanceof) — survives bundling / minification.
    ;(this as unknown as Record<string, unknown>)[UNTRUSTED_UPSTREAM_BRAND] =
      true
  }
}

/**
 * Type guard that detects an untrusted-upstream error via its brand property.
 * Works across bundle copies where `instanceof` would fail.
 */
export function isUntrustedUpstreamError(
  e: unknown,
): e is { statusCode: number; safeMessage: string } {
  return (
    typeof e === 'object' &&
    e !== null &&
    (e as Record<string, unknown>)[UNTRUSTED_UPSTREAM_BRAND] === true &&
    typeof (e as { statusCode?: unknown }).statusCode === 'number' &&
    typeof (e as { safeMessage?: unknown }).safeMessage === 'string'
  )
}
