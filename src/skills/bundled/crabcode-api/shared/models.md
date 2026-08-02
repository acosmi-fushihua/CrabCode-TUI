# Acosmi Models

## Dynamic Model System

All models are managed by the Acosmi platform and retrieved dynamically via the API. **No model IDs should ever be hardcoded.**

### List Models

```
GET /api/v4/models
Authorization: Bearer <access_token>
```

Response:
```json
{
  "code": 0,
  "data": [
    {
      "id": "model-uuid-here",
      "name": "Display Name",
      "provider": "provider-name",
      "model_id": "upstream-model-id",
      "max_tokens": 8192,
      "context_window": 200000,
      "is_enabled": true,
      "is_default": true,
      "price_per_m_tok": 3.0,
      "capabilities": {
        "supports_streaming": true,
        "supports_tool_use": true,
        "supports_vision": true,
        "supports_thinking": true,
        "supports_prompt_caching": true,
        "supports_batch": true,
        "supports_auto_mode": true,
        "supports_citations": false,
        "supports_system_prompt": true,
        "supports_temperature": true,
        "supports_top_p": true,
        "supports_top_k": true,
        "supports_stop_sequences": true,
        "supports_max_tokens": true,
        "supports_extended_thinking": true,
        "supports_computer_use": false
      }
    }
  ]
}
```

> The `capabilities` block above is **illustrative, not exhaustive, and not a
> contract**. The authoritative field set is whatever `@acosmi/sdk-ts` exposes as
> `ModelCapabilities` — read it from the SDK types, never assume a field exists
> because it looks like it should. In particular there is **no `supports_pdf`
> field** anywhere in the platform (checked 2026-07-25 across gateway, SDK and
> this client): PDF handling cannot be gated per-model today, which is why
> `src/utils/pdfUtils.ts::isPDFSupported` only checks "does a capability record
> exist at all". A sample that invented this field already cost one investigation
> a false lead.

### Get Model Capabilities

```
GET /api/v4/models/{model_id}/capabilities
Authorization: Bearer <access_token>
```

Returns the same capabilities object. Cached server-side for 5 minutes.

### ManagedModel Fields

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Unique model identifier (use this in API calls) |
| `name` | string | Human-readable display name |
| `provider` | string | Upstream provider name |
| `model_id` | string | Upstream provider's model ID |
| `max_tokens` | int | Maximum output tokens |
| `context_window` | int | Maximum context window size |
| `is_enabled` | bool | Whether the model is currently available |
| `is_default` | bool | Whether this is the default model |
| `price_per_m_tok` | float | Price per million tokens |
| `capabilities` | object | 17 capability flags (see above) |

### Best Practices

1. **Cache model list** — Call ListModels once at startup, refresh periodically (e.g., every 5 minutes)
2. **Filter by capabilities** — Use capability flags to determine what features a model supports before calling
3. **Check `is_enabled`** — Only use models with `is_enabled: true`
4. **Use `is_default`** — When the user doesn't specify a model, use the default model
5. **Dynamic UI** — Build model selectors dynamically from the API response, not from hardcoded lists
