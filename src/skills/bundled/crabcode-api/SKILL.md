# Acosmi API Skill

You are an expert at building applications with the **Acosmi API** and **Acosmi SDK**. The Acosmi platform provides AI model access, authentication, billing, and skill management through a unified REST API.

## Core Principles

1. **Models are 100% dynamic** — All model IDs come from `ListModels()` / `GET /models`. NEVER hardcode any model ID.
2. **Base URL** — Production: `https://acosmi.com`. The SDK automatically appends `/api/v4`.
3. **Authentication** — OAuth 2.1 PKCE flow. Access tokens expire in 15 minutes, refresh tokens in 7 days. The SDK handles automatic refresh.
4. **Streaming** — Use Server-Sent Events (SSE) for real-time chat responses. Never set a global HTTP timeout on streaming connections.

## Current Models

Models are managed by the Acosmi platform. Use the List Models API to discover available models:

```
GET /api/v4/models
Authorization: Bearer <access_token>
```

Each model returns: `id`, `name`, `provider`, `model_id`, `max_tokens`, `context_window`, `is_enabled`, `is_default`, `price_per_m_tok`, and `capabilities` (17 capability flags).

Do NOT use `{{MAX_EFFORT_ID}}`, `{{DEFAULT_ID}}`, or `{{FAST_MODE_ID}}` as literal strings — these are template variables resolved at runtime from the SDK model cache.

## API Endpoints Overview

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/models` | GET | List all available models |
| `/models/{id}/capabilities` | GET | Get model capabilities (cached 5min) |
| `/chat/completions` | POST | Non-streaming chat completion |
| `/chat/stream` | POST | Streaming chat (SSE) |
| `/balance` | GET | Get aggregated account balance |
| `/balance/detail` | GET | Get balance with line-item detail |
| `/balance/claim-monthly` | POST | Claim monthly free credits (idempotent) |
| `/store/packages` | GET | List available token packages |
| `/store/buy` | POST | Purchase a token package |
| `/store/orders/{id}` | GET | Check order payment status |
| `/skills/browse` | GET | Browse the skill store |
| `/skills/install` | POST | Install a skill |
| `/skills/upload` | POST | Upload a custom skill |
| `/skills/generate` | POST | AI-generate a skill |
| `/ws` | WebSocket | Real-time event push |

## Authentication Flow

```
1. Client generates PKCE code_verifier + code_challenge
2. Client opens browser → Acosmi OAuth authorize endpoint
3. User logs in and grants scopes (ai, skills, account)
4. Redirect with authorization_code
5. Client exchanges code → access_token + refresh_token
6. SDK stores tokens (default: ~/.acosmi/tokens.json)
7. SDK auto-refreshes access_token 30s before expiry
```

## Reading Guide

Refer to the language-specific documentation below for SDK usage in your language. All SDKs wrap the same REST API.

## When to Use WebFetch

Use `WebFetch` to check the latest Acosmi API documentation when:
- The user asks about a feature you're not sure is covered in these docs
- You need to verify current model availability or pricing
- The user references a recently added API endpoint

## Common Pitfalls

1. **Hardcoding model IDs** — Models change. Always use ListModels.
2. **Setting HTTP timeout on streaming** — SSE connections are long-lived. A global timeout will truncate responses.
3. **Ignoring refresh token expiry** — Refresh tokens last 7 days. Long-idle applications must handle re-authentication.
4. **Not checking `is_enabled`** — Some models may be listed but disabled. Filter by `is_enabled: true`.
5. **Manual base URL construction** — SDKs auto-append `/api/v4`. Don't double it.
