# Acosmi API Error Codes

## Error Response Format

All API errors follow a consistent JSON envelope:

```json
{
  "code": 40001,
  "message": "Human-readable error description",
  "data": null
}
```

HTTP 200 responses with `code != 0` indicate business-layer errors.

## HTTP-Level Errors

| HTTP Status | Meaning | Action |
|-------------|---------|--------|
| 400 | Bad Request | Fix request parameters |
| 401 | Unauthorized — token expired or invalid | Refresh token or re-authenticate |
| 403 | Forbidden — insufficient scopes | Request additional OAuth scopes |
| 404 | Not Found | Check API endpoint path |
| 429 | Rate Limited | Retry with exponential backoff, honor `Retry-After` header |
| 500 | Internal Server Error | Retry with backoff |
| 502/503 | Service Unavailable | Retry with backoff |

## Business-Layer Errors (HTTP 200, code != 0)

| Code Range | Category | Examples |
|------------|----------|----------|
| 10000-10999 | Authentication | Token expired, invalid refresh token, scope mismatch |
| 20000-20999 | Model | Model not found, model disabled, quota exceeded |
| 30000-30999 | Billing | Insufficient balance, payment failed, order expired |
| 40000-40999 | Request | Invalid parameters, content policy violation |
| 50000-50999 | Skill Store | Skill not found, installation failed |

## SDK Error Types

| Type | Trigger | Retryable |
|------|---------|-----------|
| `RateLimitError` | HTTP 429 | Yes — use `Retry-After` header |
| `BusinessError` | HTTP 200, code != 0 | Depends on code |
| `AuthenticationError` | HTTP 401 or refresh failure | No — re-authenticate |
| `OrderTerminalError` | Order in non-success terminal state | No |
| `NetworkError` | Connection failure | Yes — with backoff |

## Retry Strategy

```
Exponential backoff:
  Attempt 1: wait 1s
  Attempt 2: wait 2s
  Attempt 3: wait 4s
  Attempt 4: wait 8s
  Max attempts: 4

For rate limits (429):
  Use Retry-After header if present, otherwise use backoff above
```

## Streaming Errors

During SSE streaming, errors arrive as events:

```
event: error
data: {"code": 20001, "message": "model quota exceeded"}
```

The stream closes after an error event. Parse, close, and decide whether to retry.
