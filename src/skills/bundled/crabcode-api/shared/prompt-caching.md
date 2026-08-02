# Prompt Caching

## Overview

Prompt caching reduces latency and cost by reusing previously processed prompt prefixes. Cache hits skip re-processing and charge at a reduced rate.

## How It Works

1. The API caches prompt prefixes reused across requests
2. Cache keys are based on exact prefix content (system prompt + early messages)
3. Cache entries expire after ~5 minutes of inactivity
4. Cache metrics are reported in usage responses

## Maximizing Cache Hits

### Keep static content first

```json
{
  "system": "You are a helpful assistant...",        // Static → cacheable
  "messages": [
    {"role": "user", "content": "Reference docs..."}, // Static → cacheable
    {"role": "user", "content": "User query..."}      // Dynamic → not cached
  ]
}
```

### Patterns that reduce cache hits

| Pattern | Problem | Fix |
|---------|---------|-----|
| Timestamps in system prompt | Changes every request | Move to last message |
| Random examples in prefix | Different each call | Use fixed examples |
| User-specific data in system | Per-user variation | Move after static content |
| Reordering tools array | Changes cache key | Sort tools consistently |

## Cache Metrics in Streaming

The final usage event includes cache information:

```json
{
  "type": "usage",
  "input_tokens": 1500,
  "output_tokens": 200,
  "cache_creation_input_tokens": 1200,
  "cache_read_input_tokens": 300
}
```

| Field | Meaning |
|-------|---------|
| `cache_creation_input_tokens` | Tokens newly cached (first time) |
| `cache_read_input_tokens` | Tokens served from cache (hit) |

## Best Practices

1. **Tool definitions early** — They rarely change and cache well
2. **System prompt first** — Most stable prefix
3. **Append, don't rewrite** — Add messages to end; don't restructure prefix
4. **Monitor metrics** — Track `cache_read_input_tokens` to verify
5. **Check capabilities** — Verify `capabilities.supports_prompt_caching`
