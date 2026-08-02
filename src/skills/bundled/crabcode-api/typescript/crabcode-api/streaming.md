# Streaming — TypeScript

## Basic Streaming

```typescript
const stream = client.chatStream({
  modelId,
  messages: [{ role: 'user', content: 'Write a poem' }],
});

for await (const event of stream) {
  if (event.type === 'content') {
    process.stdout.write(event.text);
  }
}
```

## Full Event Handling

```typescript
const stream = client.chatStream({
  modelId,
  messages,
  system: 'You are a poet',
  tools,
});

let fullText = '';
for await (const event of stream) {
  switch (event.type) {
    case 'content':
      fullText += event.text;
      process.stdout.write(event.text);
      break;
    case 'tool_use':
      const result = await executeTool(event.name, event.input);
      // Continue conversation with tool_result...
      break;
    case 'sources':
      console.log(`\nSource: ${event.url}`);
      break;
    case 'usage':
      console.log(`\nInput: ${event.inputTokens}, Output: ${event.outputTokens}`);
      console.log(`Cache hit: ${event.cacheReadInputTokens}`);
      break;
    case 'error':
      console.error(`\nError [${event.code}]: ${event.message}`);
      break;
  }
}
```

## With Extended Thinking

```typescript
const stream = client.chatStream({
  modelId,
  messages: [{ role: 'user', content: 'Solve this math problem...' }],
  thinking: { enabled: true, budgetTokens: 4096 },
});

for await (const event of stream) {
  switch (event.type) {
    case 'thinking':
      process.stdout.write(`[Thinking] ${event.text}`);
      break;
    case 'content':
      process.stdout.write(event.text);
      break;
  }
}
```

## Error Recovery

```typescript
async function* streamWithRetry(
  client: AcosmiClient,
  modelId: string,
  messages: Message[],
  maxRetries = 3
) {
  for (let attempt = 0; attempt < maxRetries; attempt++) {
    try {
      const stream = client.chatStream({ modelId, messages });
      for await (const event of stream) {
        yield event;
      }
      return;
    } catch (error) {
      if (error instanceof RateLimitError && attempt < maxRetries - 1) {
        await sleep(error.retryAfter);
      } else {
        throw error;
      }
    }
  }
}
```

## Important

- **Never set a global HTTP timeout** — streaming connections are long-lived
- Check `capabilities.supportsStreaming` before using
- The `usage` event always arrives last
