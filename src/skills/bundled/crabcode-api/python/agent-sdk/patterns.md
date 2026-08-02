# Agent Patterns — Python

## Research Agent

```python
agent = Agent(
    client=client,
    model_id=model_id,
    system="You research topics and write comprehensive reports.",
    tools=[Tool.web_search(), Tool.web_fetch(), Tool.file_write()],
)

result = agent.run("Research the latest trends in AI coding assistants and write a report to report.md")
```

## Code Review Agent

```python
agent = Agent(
    client=client,
    model_id=model_id,
    system="You are an expert code reviewer. Analyze code for bugs, performance issues, and style.",
    tools=[Tool.file_read(), Tool.glob(), Tool.grep()],
)

result = agent.run("Review all Python files in src/ for potential security issues")
```

## Multi-Agent Pipeline

```python
# Agent 1: Research
researcher = Agent(
    client=client, model_id=model_id,
    system="Research and gather information.",
    tools=[Tool.web_search(), Tool.web_fetch()],
)

# Agent 2: Write
writer = Agent(
    client=client, model_id=model_id,
    system="Write content based on provided research.",
    tools=[Tool.file_write()],
)

# Pipeline
research = researcher.run("Find information about Rust async patterns")
writer.run(f"Based on this research, write a tutorial:\n{research.output}")
```

## Error Recovery Pattern

```python
agent = Agent(
    client=client,
    model_id=model_id,
    tools=tools,
    max_iterations=15,
)

try:
    result = agent.run(task)
    if not result.success:
        # Agent hit max iterations without completing
        print(f"Partial result: {result.output}")
        print(f"Iterations used: {result.iterations}")
except Exception as e:
    print(f"Agent error: {e}")
```

## Guardrails

```python
from crabcode_agent_sdk import Tool, Guardrail

agent = Agent(
    client=client,
    model_id=model_id,
    tools=[Tool.bash(), Tool.file_write()],
    guardrails=[
        Guardrail.no_destructive_commands(),  # Block rm -rf, etc.
        Guardrail.file_scope("./src"),         # Limit file access to src/
        Guardrail.confirm_before_execute(),     # Ask user before running commands
    ],
)
```
