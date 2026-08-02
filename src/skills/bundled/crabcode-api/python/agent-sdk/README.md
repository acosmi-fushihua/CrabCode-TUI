# Acosmi Agent SDK — Python

## Overview

The Agent SDK provides a high-level framework for building AI agents with built-in tools (file access, web search, terminal execution).

## Installation

```bash
pip install crabcode-agent-sdk
```

## Quick Start

```python
from crabcode_agent_sdk import Agent, Tool

agent = Agent(
    client=client,  # AcosmiClient instance
    model_id=model_id,
    system="You are a helpful coding assistant",
    tools=[
        Tool.file_read(),
        Tool.file_write(),
        Tool.web_search(),
        Tool.bash(),
    ],
)

result = agent.run("Find all Python files and count lines of code")
print(result.output)
```

## Built-in Tools

| Tool | Description |
|------|-------------|
| `Tool.file_read()` | Read files from the filesystem |
| `Tool.file_write()` | Write/create files |
| `Tool.web_search()` | Search the web |
| `Tool.web_fetch()` | Fetch a URL |
| `Tool.bash()` | Execute shell commands |
| `Tool.glob()` | Find files by pattern |
| `Tool.grep()` | Search file contents |

## Custom Tools

```python
from crabcode_agent_sdk import Tool

def get_stock_price(symbol: str) -> str:
    """Get current stock price for a ticker symbol."""
    price = fetch_price(symbol)
    return f"{symbol}: ${price}"

agent = Agent(
    client=client,
    model_id=model_id,
    tools=[
        Tool.from_function(get_stock_price),
        Tool.bash(),
    ],
)
```

## Conversation Loop

```python
# Multi-turn agent with memory
agent = Agent(client=client, model_id=model_id, tools=tools)

while True:
    user_input = input("> ")
    if user_input == "exit":
        break
    result = agent.run(user_input)
    print(result.output)
    # Agent maintains conversation history automatically
```

## Configuration

```python
agent = Agent(
    client=client,
    model_id=model_id,
    system="System prompt",
    tools=tools,
    max_iterations=10,    # Max tool-use loops
    temperature=0.7,
    thinking=True,        # Enable extended thinking
)
```
