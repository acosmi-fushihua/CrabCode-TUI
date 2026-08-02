/**
 * PR-2 (2026-07-01 custom/local model lifecycle audit, finding 5): a
 * `custom:<entry.id>` reference whose registry entry no longer resolves
 * (deleted / disabled / membership downgraded) used to fall through to the
 * Acosmi gateway, which replied with a baffling model_not_found/402 that
 * buried the real cause. The prefix already declares "this is not a gateway
 * model", so an unresolvable reference is a terminal, user-fixable state —
 * fail loud with recovery guidance instead.
 *
 * Brand-property (not instanceof) check, mirroring `UntrustedUpstreamError`:
 * the error may cross module-duplication boundaries under bun's test loader.
 */

const BRAND = 'DanglingModelReferenceError' as const

export class DanglingModelReferenceError extends Error {
  readonly brand = BRAND
  readonly reference: string

  constructor(reference: string) {
    super(
      `自定义模型条目不可用（引用 ${reference}）：该条目可能已被删除、已停用，或当前账号无自定义模型权益。` +
        `请在 设置 → 模型 中重新选择一个模型。此请求不会发送到 Acosmi 网关。`,
    )
    this.name = BRAND
    this.reference = reference
  }
}

export function isDanglingModelReferenceError(
  err: unknown,
): err is DanglingModelReferenceError {
  return (
    typeof err === 'object' &&
    err !== null &&
    (err as { brand?: unknown }).brand === BRAND
  )
}
