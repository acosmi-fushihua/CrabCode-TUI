# Acosmi Agent SDK — TypeScript

## Installation

```bash
npm install crabcode-agent-sdk
```

## Quick Start

```typescript
import { Agent, Tool } from 'crabcode-agent-sdk';

const agent = new Agent({
  client,  // AcosmiClient instance
  modelId,
  system: 'You are a helpful coding assistant',
  tools: [
    Tool.fileRead(),
    Tool.fileWrite(),
    Tool.webSearch(),
    Tool.bash(),
  ],
});

const result = await agent.run('Find all TypeScript files and count lines');
console.log(result.output);
```

## Built-in Tools

| Tool | Description |
|------|-------------|
| `Tool.fileRead()` | Read files |
| `Tool.fileWrite()` | Write/create files |
| `Tool.webSearch()` | Search the web |
| `Tool.webFetch()` | Fetch a URL |
| `Tool.bash()` | Execute shell commands |
| `Tool.glob()` | Find files by pattern |
| `Tool.grep()` | Search file contents |

## Custom Tools

```typescript
import { Agent, Tool } from 'crabcode-agent-sdk';

const getStockPrice = Tool.fromFunction({
  name: 'get_stock_price',
  description: 'Get current stock price',
  parameters: {
    type: 'object',
    properties: {
      symbol: { type: 'string', description: 'Ticker symbol' },
    },
    required: ['symbol'],
  },
  execute: async ({ symbol }) => {
    const price = await fetchPrice(symbol);
    return `${symbol}: $${price}`;
  },
});

const agent = new Agent({
  client, modelId,
  tools: [getStockPrice, Tool.bash()],
});
```

## Multi-turn Conversation

```typescript
const agent = new Agent({ client, modelId, tools });

// Agent maintains conversation history
const r1 = await agent.run('Read package.json');
const r2 = await agent.run('What dependencies does it have?');
const r3 = await agent.run('Are any outdated?');
```

## Configuration

```typescript
const agent = new Agent({
  client,
  modelId,
  system: 'System prompt',
  tools,
  maxIterations: 10,
  temperature: 0.7,
  thinking: true,
});
```
