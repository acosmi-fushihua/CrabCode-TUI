// Probes a custom-model endpoint's reachability so the user can self-verify
// `baseUrl` + `protocol` + `modelId` + key before committing the entry. The
// worker owns secrets, and a stored key is resolved from secure storage by its
// handle.
//
// Send one minimal real request with the user's
// `modelId` (`max_tokens: 1`). This genuinely validates the {endpoint, key,
// modelId} triple at the cost of a tiny quota hit, unlike a `GET /models` probe
// which many `/anthropic` endpoints do not implement (false negatives).
//
// The probe sends the user-supplied `modelId`; it never
// hardcodes any model id / family / price.

type MethodContext = unknown;

export type CustomModelTestConnectionParams = {
  baseUrl: string;
  protocol: "anthropic-compatible" | "openai-compatible";
  modelId: string;
  apiKey?: string;
  apiKeyHandle?: string;
};

export type CustomModelTestConnectionResponse = {
  ok: boolean;
  httpStatus?: number;
  latencyMs?: number;
  errorReason?: string;
};

// NOTE: `customModelSecrets` is imported DYNAMICALLY inside the handler, NOT at
// the top level. A static import pulls `secureStorage/*` into `methods.ts`'s
// boot-time module graph, which reorders evaluation so `AgentTool.tsx`'s lazy
// tool schema runs BEFORE `headless-runtime.ts`'s top-level side-effect installs
// `globalThis.MACRO` → `ReferenceError: MACRO is not defined` at worker boot
// (only the `bun src/index.ts` spawn path, where bunfig's `[test]` preload does
// NOT apply). Deferring the import keeps this module side-effect-free at boot.
type ReadCustomModelApiKeySync = (handle: string | undefined) => string | null;
type DebugLogFn = (message: string) => void;

/** Hard wall on a single probe request (DNS + connect + TLS + minimal call). */
const PROBE_TIMEOUT_MS = 15000;
/** Enough room for provider repair hints without echoing an unbounded body. */
const PROBE_RESPONSE_BODY_MAX_CHARS = 8192;
/** Keep probe errors actionable while bounding provider response text. */
const PROBE_USER_ERROR_MAX_CHARS = 1000;
const CUSTOM_MODEL_DEBUG_STREAM_ENV = "CRABCODE_CUSTOM_MODEL_DEBUG_STREAM";

// Test seams (mirrors config-read/config-write `__set*ForTest` pattern): inject
// a deterministic key reader + fetch so unit tests never touch real secure
// storage or the network.
let readKeyOverride: ((handle: string | undefined) => string | null) | null =
  null;
let fetchOverride: typeof fetch | null = null;
let debugLogOverride: DebugLogFn | null = null;

export function __setCustomModelTestFnsForTest(
  fns: {
    readKey?: (handle: string | undefined) => string | null;
    fetch?: typeof fetch;
    debugLog?: DebugLogFn;
  } | null,
): void {
  readKeyOverride = fns?.readKey ?? null;
  fetchOverride = fns?.fetch ?? null;
  debugLogOverride = fns?.debugLog ?? null;
}

interface ProbeDebugTrace {
  noteAttempt(body: Record<string, unknown>): void;
  noteResponse(res: Response, latencyMs: number): void;
  noteHttpDetail(detail: string): void;
  noteRetry(reason: string): void;
  finish(
    outcome: string,
    extra?: Record<string, string | number | boolean | null | undefined>,
  ): Promise<void>;
}

function createProbeDebugTrace(args: {
  protocol: CustomModelTestConnectionParams["protocol"];
  endpoint: string;
  modelId: string;
}): ProbeDebugTrace | null {
  if (!isTruthyEnv(process.env[CUSTOM_MODEL_DEBUG_STREAM_ENV])) return null;

  const startedAt = Date.now();
  const statuses: string[] = [];
  const contentTypes = new Set<string>();
  const requestIds = new Set<string>();
  const requestShapes: string[] = [];
  const retryReasons: string[] = [];
  const detailSamples: string[] = [];
  let attempts = 0;
  let finished = false;

  return {
    noteAttempt(body: Record<string, unknown>): void {
      attempts += 1;
      requestShapes.push(summarizeProbeRequestShape(body));
    },
    noteResponse(res: Response, latencyMs: number): void {
      statuses.push(String(res.status));
      const headers = readHeadersLike(res.headers);
      const contentType = headers.get("content-type");
      if (contentType) contentTypes.add(contentType);
      for (const header of ["request-id", "x-request-id", "openai-request-id"]) {
        const value = headers.get(header);
        if (value) requestIds.add(`${header}=${redactProbeText(value)}`);
      }
      requestIds.add(`latencyMs=${latencyMs}`);
    },
    noteHttpDetail(detail: string): void {
      if (detail.trim()) detailSamples.push(summarizeProbeDetail(detail));
    },
    noteRetry(reason: string): void {
      retryReasons.push(reason);
    },
    async finish(outcome, extra = {}): Promise<void> {
      if (finished) return;
      finished = true;
      await emitProbeDebugLog(
        `[custom-model-probe] ${JSON.stringify({
          outcome,
          protocol: args.protocol,
          endpoint: summarizeEndpointForDebug(args.endpoint),
          modelId: args.modelId,
          attempts,
          statuses,
          contentTypes: [...contentTypes],
          requestIds: [...requestIds],
          requestShapes,
          retryReasons,
          detailSamples: detailSamples.slice(0, 3),
          durationMs: Date.now() - startedAt,
          ...extra,
        })}`,
      );
    },
  };
}

function isTruthyEnv(value: string | undefined): boolean {
  if (!value) return false;
  return !["0", "false", "no", "off"].includes(value.toLowerCase().trim());
}

async function emitProbeDebugLog(message: string): Promise<void> {
  if (debugLogOverride) {
    debugLogOverride(message);
    return;
  }
  try {
    const { logForDebugging } = await import("../../utils/debug.js");
    logForDebugging(message);
  } catch {
    console.warn(message);
  }
}

function readHeadersLike(headers: unknown): { get(name: string): string | null } {
  if (headers && typeof (headers as Headers).get === "function") {
    return headers as Headers;
  }
  return { get: () => null };
}

function summarizeEndpointForDebug(endpoint: string): string {
  try {
    const url = new URL(endpoint);
    return `${url.origin}${url.pathname}`;
  } catch {
    return "<invalid-endpoint>";
  }
}

function summarizeProbeRequestShape(body: Record<string, unknown>): string {
  const tokenField =
    "max_completion_tokens" in body
      ? "max_completion_tokens"
      : "max_tokens" in body
        ? "max_tokens"
        : "none";
  const keys = Object.keys(body).sort().join(",");
  return `keys=${keys} tokenField=${tokenField}`;
}

function summarizeProbeDetail(detail: string): string {
  try {
    const parsed = JSON.parse(detail) as unknown;
    if (parsed && typeof parsed === "object") {
      const obj = parsed as Record<string, unknown>;
      const nested = obj.error && typeof obj.error === "object"
        ? (obj.error as Record<string, unknown>)
        : obj;
      const parts: string[] = [];
      if (typeof nested.type === "string") parts.push(`type=${nested.type}`);
      if (typeof nested.code === "string") parts.push(`code=${nested.code}`);
      if (typeof nested.message === "string") {
        parts.push(`message=${truncateProbeSample(redactProbeText(nested.message), 180)}`);
      }
      parts.push(`keys=${Object.keys(obj).slice(0, 8).join(",")}`);
      return `{${parts.join(" ")}}`;
    }
  } catch {
    // Fall through to plain-text summary.
  }
  return truncateProbeSample(redactProbeText(detail), 220);
}

function redactProbeText(text: string): string {
  return text
    .replace(/Bearer\s+[A-Za-z0-9._~+/=-]+/gi, "Bearer <redacted>")
    .replace(/sk-[A-Za-z0-9._-]{8,}/g, "sk-<redacted>")
    .replace(
      /"(api[_-]?key|x-api-key|authorization)"\s*:\s*"[^"]*"/gi,
      '"$1":"<redacted>"',
    );
}

function truncateProbeSample(text: string, maxChars: number): string {
  if (text.length <= maxChars) return text;
  return `${text.slice(0, maxChars)}...<truncated>`;
}

function normalizeProbeWhitespace(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

function probeScalar(value: unknown): string | null {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return null;
}

/**
 * Convert common provider error envelopes into one status-free diagnostic.
 * `httpStatus` already has its own protocol field; repeating it here produced
 * duplicated output such as `403 403 Forbidden`.
 */
function formatProbeHttpError(res: Response, body: string): string {
  const parts: string[] = [];
  const statusText = normalizeProbeWhitespace(res.statusText ?? "");
  if (statusText) parts.push(statusText);

  let bodyRequestId: string | null = null;
  if (body.trim()) {
    try {
      const parsed = JSON.parse(body) as unknown;
      if (parsed && typeof parsed === "object") {
        const outer = parsed as Record<string, unknown>;
        const nested = outer.error && typeof outer.error === "object"
          ? (outer.error as Record<string, unknown>)
          : outer;
        const detailParts: string[] = [];
        const type = probeScalar(nested.type);
        const code = probeScalar(nested.code);
        const param = probeScalar(nested.param);
        const nestedMessage = probeScalar(nested.message);
        const outerMessage = probeScalar(outer.message);
        const stringError = probeScalar(outer.error);
        const message = nestedMessage ?? outerMessage ?? stringError;
        if (type) detailParts.push(`type=${type}`);
        if (code) detailParts.push(`code=${code}`);
        if (param) detailParts.push(`param=${param}`);
        if (message) detailParts.push(`message=${message}`);
        if (detailParts.length > 0) parts.push(detailParts.join(" · "));
        bodyRequestId = probeScalar(outer.request_id) ?? probeScalar(nested.request_id);
      }
    } catch {
      // Plain text / HTML-ish provider responses remain useful after whitespace
      // normalization and secret redaction below.
    }
    if (parts.length <= (statusText ? 1 : 0)) {
      parts.push(normalizeProbeWhitespace(body));
    }
  }

  const headerValues = readHeadersLike(res.headers);
  const headerRequestId = ["request-id", "x-request-id", "openai-request-id"]
    .map(name => headerValues.get(name))
    .find((value): value is string => Boolean(value));
  const requestId = bodyRequestId ?? headerRequestId ?? null;
  if (requestId) parts.push(`request_id=${requestId}`);

  const combined = parts.join(" · ") || "request rejected";
  return truncateProbeSample(
    redactProbeText(normalizeProbeWhitespace(combined)),
    PROBE_USER_ERROR_MAX_CHARS,
  );
}

export async function customModelTestConnectionHandler(
  rawParams: unknown,
  _ctx: MethodContext,
): Promise<CustomModelTestConnectionResponse> {
  const params = rawParams as CustomModelTestConnectionParams;

  if (!params || typeof params.baseUrl !== "string" || params.baseUrl.length === 0) {
    return { ok: false, errorReason: "baseUrl is required" };
  }
  if (typeof params.modelId !== "string" || params.modelId.length === 0) {
    return { ok: false, errorReason: "modelId is required" };
  }

  // Resolve the key: an edit reuses the stored handle; a new entry passes the
  // plaintext key for this probe only (never persisted here). The real reader is
  // imported dynamically (see the top-of-file note on boot-order / MACRO).
  let readKey: ReadCustomModelApiKeySync;
  if (readKeyOverride) {
    readKey = readKeyOverride;
  } else {
    const mod = await import(
      "../../utils/model/customModelSecrets.js"
    );
    readKey = mod.readCustomModelApiKeySync;
  }
  let apiKey: string | null = null;
  if (params.apiKeyHandle) {
    apiKey = readKey(params.apiKeyHandle);
    if (!apiKey) {
      return { ok: false, errorReason: "stored API key not found for this entry" };
    }
  } else if (typeof params.apiKey === "string" && params.apiKey.length > 0) {
    apiKey = params.apiKey;
  } else {
    return { ok: false, errorReason: "no API key provided" };
  }

  const isAnthropic = params.protocol === "anthropic-compatible";
  // Single endpoint truth shared with the runtime chat adapter
  // (`customModelChatStream.ts` imports the same function) — the probe hits
  // the EXACT URL the real custom-model chat dials, by construction. Imported
  // dynamically for the same boot-order/MACRO reason as `customModelSecrets`.
  const { resolveCustomModelEndpoint } = await import(
    "./customModelEndpoint.js"
  );
  const url = resolveCustomModelEndpoint(
    isAnthropic ? "anthropic-compatible" : "openai-compatible",
    params.baseUrl,
  );
  const debugTrace = createProbeDebugTrace({
    protocol: params.protocol,
    endpoint: url,
    modelId: params.modelId,
  });
  const headers: Record<string, string> = {
    "content-type": "application/json",
  };
  if (isAnthropic) {
    headers["x-api-key"] = apiKey;
    headers["anthropic-version"] = "2023-06-01";
  } else {
    headers["authorization"] = `Bearer ${apiKey}`;
  }
  // §1: `model` is the user-supplied id, never hardcoded. `max_tokens: 1`
  // keeps the probe quota cost minimal. Some official/newer OpenAI-compatible
  // models require `max_completion_tokens`; on a 400 that explicitly says so,
  // the shared shape repair below retries once with the newer spelling.
  let body: Record<string, unknown> = {
    model: params.modelId,
    max_tokens: 1,
    messages: [{ role: "user", content: "ping" }],
  };
  const { createOpenAIChatCompletionRepairState, maybeRepairOpenAIChatCompletionRequest } =
    await import(
      "./openAIChatCompletionRepair.js"
    );
  const openAIRepairState = createOpenAIChatCompletionRepairState();

  const doFetch = fetchOverride ?? fetch;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), PROBE_TIMEOUT_MS);
  const startedAt = Date.now();
  try {
    for (;;) {
      debugTrace?.noteAttempt(body);
      const res = await doFetch(url, {
        method: "POST",
        headers,
        body: JSON.stringify(body),
        signal: controller.signal,
      });
      const latencyMs = Date.now() - startedAt;
      debugTrace?.noteResponse(res, latencyMs);
      if (res.ok) {
        // 2xx — the endpoint accepted the request with this key + model.
        await debugTrace?.finish("ok", { httpStatus: res.status });
        return { ok: true, httpStatus: res.status, latencyMs };
      }
      // Non-2xx: the server was reached but rejected the request (e.g. 401 bad
      // key, 404 wrong model/endpoint). Keep a bounded body large enough for
      // request-shape repair, then project a redacted structured explanation.
      // The numeric status stays solely in `httpStatus` to avoid duplicate UI.
      let responseBody = "";
      try {
        responseBody = (await res.text()).slice(0, PROBE_RESPONSE_BODY_MAX_CHARS);
      } catch {
        /* body unreadable — status alone is enough */
      }
      debugTrace?.noteHttpDetail(responseBody);
      if (res.status === 400 && !isAnthropic) {
        const repair = maybeRepairOpenAIChatCompletionRequest(
          body,
          responseBody,
          openAIRepairState,
        );
        if (repair) {
          body = repair.body;
          debugTrace?.noteRetry(repair.reason);
          continue;
        }
      }
      const reason = formatProbeHttpError(res, responseBody);
      await debugTrace?.finish("http_error", {
        httpStatus: res.status,
        errorReason: reason,
      });
      return { ok: false, httpStatus: res.status, latencyMs, errorReason: reason };
    }
  } catch (err) {
    const latencyMs = Date.now() - startedAt;
    const name = (err as { name?: string }).name;
    const reason =
      name === "AbortError"
        ? `connect timeout (${PROBE_TIMEOUT_MS}ms)`
        : (err as Error).message || "connection failed";
    await debugTrace?.finish(name === "AbortError" ? "timeout" : "fetch_error", {
      errorReason: reason,
    });
    return { ok: false, latencyMs, errorReason: reason };
  } finally {
    clearTimeout(timer);
  }
}
