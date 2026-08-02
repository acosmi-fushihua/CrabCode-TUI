# Agent Patterns — TypeScript

## Research Agent

```typescript
const agent = new Agent({
  client, modelId,
  system: 'Research topics and write comprehensive reports.',
  tools: [Tool.webSearch(), Tool.webFetch(), Tool.fileWrite()],
});

const result = await agent.run(
  'Research AI coding assistant trends and write a report to report.md'
);
```

## Code Review Agent

```typescript
const agent = new Agent({
  client, modelId,
  system: 'Expert code reviewer. Analyze for bugs, performance, and style.',
  tools: [Tool.fileRead(), Tool.glob(), Tool.grep()],
});

const result = await agent.run('Review all TypeScript files in src/ for security issues');
```

## Multi-Agent Pipeline

```typescript
// Agent 1: Research
const researcher = new Agent({
  client, modelId,
  system: 'Research and gather information.',
  tools: [Tool.webSearch(), Tool.webFetch()],
});

// Agent 2: Write
const writer = new Agent({
  client, modelId,
  system: 'Write content based on provided research.',
  tools: [Tool.fileWrite()],
});

// Pipeline
const research = await researcher.run('Find info about Rust async patterns');
await writer.run(`Based on this research, write a tutorial:\n${research.output}`);
```

## Error Recovery

```typescript
const agent = new Agent({
  client, modelId,
  tools,
  maxIterations: 15,
});

try {
  const result = await agent.run(task);
  if (!result.success) {
    console.log(`Partial: ${result.output}`);
    console.log(`Iterations: ${result.iterations}`);
  }
} catch (error) {
  console.error('Agent error:', error);
}
```

## Guardrails

```typescript
import { Tool, Guardrail } from 'crabcode-agent-sdk';

const agent = new Agent({
  client, modelId,
  tools: [Tool.bash(), Tool.fileWrite()],
  guardrails: [
    Guardrail.noDestructiveCommands(),
    Guardrail.fileScope('./src'),
    Guardrail.confirmBeforeExecute(),
  ],
});
```
