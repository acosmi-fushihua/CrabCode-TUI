# Acosmi SDK for TypeScript

## Installation

```bash
npm install @acosmi-ai/sdk
```

## Quick Start

```typescript
import { AcosmiClient } from '@acosmi-ai/sdk';

const client = new AcosmiClient({ serverUrl: 'https://acosmi.com' });

// Authenticate
if (!client.isAuthorized()) {
  await client.login('MyApp', { scopes: ['ai', 'skills', 'account'] });
}

// List models (NEVER hardcode model IDs)
const models = await client.listModels();
const defaultModel = models.find(m => m.isDefault && m.isEnabled);

// Chat completion
const response = await client.chatComplete({
  modelId: defaultModel.id,
  messages: [{ role: 'user', content: 'Hello!' }],
});
console.log(response.content);
```

## Authentication

```typescript
// OAuth 2.1 PKCE login (opens browser)
await client.login('AppName', { scopes: ['ai', 'skills', 'account'] });

// Check status
client.isAuthorized(); // boolean

// Logout
await client.logout();
```

Token storage: `~/.acosmi/tokens.json`
Access token: 15min, Refresh token: 7d, auto-refresh 30s before expiry.

## Models

```typescript
const models = await client.listModels();

for (const model of models) {
  console.log(`${model.name}: ${model.id} (enabled=${model.isEnabled})`);
}

// Get capabilities
const caps = await client.getModelCapabilities(modelId);
if (caps.supportsToolUse) {
  console.log('Tool use supported');
}
```

## Chat

### Non-streaming

```typescript
const response = await client.chatComplete({
  modelId,
  messages: [{ role: 'user', content: 'Explain quantum computing' }],
  system: 'You are a physics professor',
  temperature: 0.7,
  maxTokens: 1024,
});
console.log(response.content);
```

### Streaming

```typescript
const stream = client.chatStream({
  modelId,
  messages: [{ role: 'user', content: 'Write a story' }],
});

for await (const event of stream) {
  switch (event.type) {
    case 'content':
      process.stdout.write(event.text);
      break;
    case 'sources':
      console.log('Source:', event.url);
      break;
    case 'usage':
      console.log(`Tokens: ${event.inputTokens} in, ${event.outputTokens} out`);
      break;
    case 'error':
      console.error('Error:', event.message);
      break;
  }
}
```

### With Tools

```typescript
const tools = [
  {
    name: 'get_weather',
    description: 'Get weather for a location',
    inputSchema: {
      type: 'object',
      properties: {
        location: { type: 'string', description: 'City name' },
      },
      required: ['location'],
    },
  },
];

const response = await client.chatComplete({
  modelId,
  messages: [{ role: 'user', content: 'Weather in Beijing?' }],
  tools,
});

if (response.stopReason === 'tool_use') {
  for (const block of response.content) {
    if (block.type === 'tool_use') {
      const result = await executeTool(block.name, block.input);
      // Send tool_result back...
    }
  }
}
```

### Extended Options

```typescript
const response = await client.chatComplete({
  modelId,
  messages,
  system: 'System prompt',
  tools,
  temperature: 0.7,
  maxTokens: 2048,
  thinking: { enabled: true, budgetTokens: 4096 },
  serverTools: ['web_search'],
  speed: 'fast',
  effort: 'high',
  metadata: { sessionId: 'abc123' },
});
```

## Balance & Billing

```typescript
const balance = await client.getBalance();
console.log(`Available: ${balance.available}`);

const detail = await client.getBalanceDetail();
await client.claimMonthlyFree(); // Idempotent
```

## Error Handling

```typescript
import { RateLimitError, BusinessError, AuthenticationError } from '@acosmi-ai/sdk';

try {
  const response = await client.chatComplete({ modelId, messages });
} catch (error) {
  if (error instanceof RateLimitError) {
    await sleep(error.retryAfter);
    // retry
  } else if (error instanceof BusinessError) {
    console.error(`Business error ${error.code}: ${error.message}`);
  } else if (error instanceof AuthenticationError) {
    await client.login('MyApp', { scopes: ['ai'] });
  }
}
```

## Important Notes

- **Never hardcode model IDs** — use `listModels()` to discover them
- **Don't set HTTP timeout** for streaming — SSE connections are long-lived
- **Token auto-refresh** — SDK handles this automatically
