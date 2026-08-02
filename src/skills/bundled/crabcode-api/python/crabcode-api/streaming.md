# Streaming — Python

## Basic Streaming

```python
stream = client.chat_stream(
    model_id=model_id,
    messages=[{"role": "user", "content": "Write a poem"}],
)

for event in stream:
    if event.type == "content":
        print(event.text, end="", flush=True)
```

## Full Event Handling

```python
stream = client.chat_stream(
    model_id=model_id,
    messages=messages,
    system="You are a poet",
    tools=tools,
)

full_text = ""
for event in stream:
    match event.type:
        case "content":
            full_text += event.text
            print(event.text, end="", flush=True)
        case "tool_use":
            # Model wants to call a tool
            result = execute_tool(event.name, event.input)
            # Continue conversation with tool_result...
        case "sources":
            print(f"\nSource: {event.url}")
        case "usage":
            print(f"\nInput: {event.input_tokens}, Output: {event.output_tokens}")
            print(f"Cache hit: {event.cache_read_input_tokens}")
        case "error":
            print(f"\nError [{event.code}]: {event.message}")
            break
```

## With Thinking (Extended Thinking)

```python
stream = client.chat_stream(
    model_id=model_id,
    messages=[{"role": "user", "content": "Solve this math problem..."}],
    thinking={"enabled": True, "budget_tokens": 4096},
)

for event in stream:
    match event.type:
        case "thinking":
            print(f"[Thinking] {event.text}", end="")
        case "content":
            print(event.text, end="", flush=True)
```

## Error Recovery

```python
from acosmi import RateLimitError
import time

def stream_with_retry(client, model_id, messages, max_retries=3):
    for attempt in range(max_retries):
        try:
            stream = client.chat_stream(model_id=model_id, messages=messages)
            for event in stream:
                yield event
            return
        except RateLimitError as e:
            if attempt < max_retries - 1:
                time.sleep(e.retry_after)
            else:
                raise
```

## Important

- **Never set a global HTTP timeout** — streaming connections are long-lived
- Check `capabilities.supports_streaming` before using streaming
- The `usage` event always arrives last in the stream
