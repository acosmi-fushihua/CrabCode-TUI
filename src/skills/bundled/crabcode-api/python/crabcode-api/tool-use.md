# Tool Use — Python

## Define Tools

```python
tools = [
    {
        "name": "search_database",
        "description": "Search the product database",
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "limit": {"type": "integer", "description": "Max results", "default": 10},
            },
            "required": ["query"],
        },
    },
    {
        "name": "send_email",
        "description": "Send an email to a recipient",
        "input_schema": {
            "type": "object",
            "properties": {
                "to": {"type": "string"},
                "subject": {"type": "string"},
                "body": {"type": "string"},
            },
            "required": ["to", "subject", "body"],
        },
    },
]
```

## Agentic Tool Loop

```python
def run_agent(client, model_id, user_message, tools):
    messages = [{"role": "user", "content": user_message}]

    while True:
        response = client.chat_complete(
            model_id=model_id,
            messages=messages,
            tools=tools,
        )

        # If no tool use, return final response
        if response.stop_reason != "tool_use":
            return response.content

        # Execute all tool calls
        tool_results = []
        for block in response.content:
            if block.type == "tool_use":
                result = execute_tool(block.name, block.input)
                tool_results.append({
                    "type": "tool_result",
                    "tool_use_id": block.id,
                    "content": str(result),
                })

        # Append assistant response + tool results
        messages.append({"role": "assistant", "content": response.content})
        messages.append({"role": "user", "content": tool_results})


def execute_tool(name, input_args):
    match name:
        case "search_database":
            return search_db(input_args["query"], input_args.get("limit", 10))
        case "send_email":
            return send_email(**input_args)
        case _:
            return f"Unknown tool: {name}"
```

## Streaming with Tools

```python
stream = client.chat_stream(
    model_id=model_id,
    messages=messages,
    tools=tools,
)

tool_calls = []
for event in stream:
    if event.type == "content":
        print(event.text, end="")
    elif event.type == "tool_use":
        tool_calls.append(event)

# Process tool calls after stream completes
for call in tool_calls:
    result = execute_tool(call.name, call.input)
    # Continue conversation...
```

## Error Handling in Tools

```python
def execute_tool(name, input_args):
    try:
        result = tool_dispatch[name](**input_args)
        return {"status": "success", "data": result}
    except Exception as e:
        # Return error as content — model can retry or adjust
        return {"status": "error", "message": str(e)}
```
