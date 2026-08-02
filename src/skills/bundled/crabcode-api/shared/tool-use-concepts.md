# Tool Use Concepts

## Overview

The Acosmi API supports tool use (function calling), allowing models to invoke client-defined functions to interact with external systems.

## Flow

```
1. Client sends messages + tool definitions → API
2. Model responds with tool_use block (name + arguments)
3. Client executes the function locally
4. Client sends tool_result back → API
5. Model generates final response incorporating tool results
```

## Tool Definition

```json
{
  "name": "get_weather",
  "description": "Get current weather for a location",
  "input_schema": {
    "type": "object",
    "properties": {
      "location": { "type": "string", "description": "City name" },
      "unit": { "type": "string", "enum": ["celsius", "fahrenheit"] }
    },
    "required": ["location"]
  }
}
```

## Message Flow Example

### 1. Request with tools

```json
{
  "model_id": "<from ListModels>",
  "messages": [{"role": "user", "content": "What's the weather in Beijing?"}],
  "tools": [{"name": "get_weather", "description": "...", "input_schema": {...}}]
}
```

### 2. Model returns tool_use

```json
{
  "role": "assistant",
  "content": [
    {"type": "tool_use", "id": "toolu_abc123", "name": "get_weather", "input": {"location": "Beijing"}}
  ],
  "stop_reason": "tool_use"
}
```

### 3. Client sends tool_result

```json
{
  "messages": [
    {"role": "user", "content": "What's the weather in Beijing?"},
    {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_abc123", ...}]},
    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_abc123", "content": "22°C, sunny"}]}
  ]
}
```

## Best Practices

1. **Clear descriptions** — Directly affects how well the model uses tools
2. **Strict schemas** — Use `required`, `enum`, `description` to constrain inputs
3. **Handle errors** — Return errors in tool_result so the model can adjust
4. **Limit tool count** — Provide only relevant tools per context
5. **Check capabilities** — Verify `capabilities.supports_tool_use` before sending tools
6. **Parallel calls** — Model may return multiple tool_use blocks; execute concurrently
