# Acosmi SDK for Python

## Installation

```bash
pip install acosmi
```

## Quick Start

```python
from acosmi import AcosmiClient

client = AcosmiClient(server_url="https://acosmi.com")

# Authenticate
if not client.is_authorized():
    client.login("MyApp", scopes=["ai", "skills", "account"])

# List models (NEVER hardcode model IDs)
models = client.list_models()
default_model = next(m for m in models if m.is_default and m.is_enabled)

# Chat completion
response = client.chat_complete(
    model_id=default_model.id,
    messages=[{"role": "user", "content": "Hello!"}],
)
print(response.content)
```

## Authentication

```python
# Simple login (opens browser for OAuth 2.1 PKCE)
client.login("AppName", scopes=["ai", "skills", "account"])

# Check authorization
client.is_authorized()  # bool

# Logout
client.logout()
```

Token storage: `~/.acosmi/tokens.json`
Access token: 15min, Refresh token: 7d, auto-refresh 30s before expiry.

## Models

```python
# List all available models
models = client.list_models()

for model in models:
    print(f"{model.name}: {model.id} (enabled={model.is_enabled})")

# Get capabilities for a specific model
caps = client.get_model_capabilities(model_id)
if caps.supports_tool_use:
    print("This model supports tool use")
```

## Chat

### Non-streaming

```python
response = client.chat_complete(
    model_id=model_id,
    messages=[
        {"role": "user", "content": "Explain quantum computing"}
    ],
    system="You are a physics professor",
    temperature=0.7,
    max_tokens=1024,
)
print(response.content)
```

### Streaming

```python
stream = client.chat_stream(
    model_id=model_id,
    messages=[{"role": "user", "content": "Write a story"}],
)

for event in stream:
    if event.type == "content":
        print(event.text, end="", flush=True)
    elif event.type == "sources":
        print(f"\nSource: {event.url}")
    elif event.type == "usage":
        print(f"\nTokens: {event.input_tokens} in, {event.output_tokens} out")
    elif event.type == "error":
        print(f"\nError: {event.message}")
```

### With Tools

```python
tools = [
    {
        "name": "get_weather",
        "description": "Get weather for a location",
        "input_schema": {
            "type": "object",
            "properties": {
                "location": {"type": "string", "description": "City name"},
            },
            "required": ["location"],
        },
    }
]

response = client.chat_complete(
    model_id=model_id,
    messages=[{"role": "user", "content": "Weather in Beijing?"}],
    tools=tools,
)

# Handle tool_use response
if response.stop_reason == "tool_use":
    for block in response.content:
        if block.type == "tool_use":
            result = execute_tool(block.name, block.input)
            # Send tool_result back...
```

### Extended Options

```python
response = client.chat_complete(
    model_id=model_id,
    messages=messages,
    system="System prompt",
    tools=tools,
    temperature=0.7,
    max_tokens=2048,
    thinking={"enabled": True, "budget_tokens": 4096},
    server_tools=["web_search"],
    speed="fast",
    effort="high",
    metadata={"session_id": "abc123"},
)
```

## Balance & Billing

```python
balance = client.get_balance()
print(f"Available: {balance.available}")

detail = client.get_balance_detail()
client.claim_monthly_free()  # Idempotent
```

## Error Handling

```python
from acosmi import RateLimitError, BusinessError, AuthenticationError

try:
    response = client.chat_complete(model_id=model_id, messages=messages)
except RateLimitError as e:
    time.sleep(e.retry_after)
    # retry
except BusinessError as e:
    print(f"Business error {e.code}: {e.message}")
except AuthenticationError:
    client.login("MyApp", scopes=["ai"])
```

## Important Notes

- **Never hardcode model IDs** — use `list_models()` to discover them
- **Don't set HTTP timeout** for streaming — SSE connections are long-lived
- **Token auto-refresh** — SDK handles this automatically
