# Tool Use — TypeScript

## Define Tools

```typescript
const tools = [
  {
    name: 'search_database',
    description: 'Search the product database',
    inputSchema: {
      type: 'object',
      properties: {
        query: { type: 'string', description: 'Search query' },
        limit: { type: 'integer', description: 'Max results', default: 10 },
      },
      required: ['query'],
    },
  },
  {
    name: 'send_email',
    description: 'Send an email',
    inputSchema: {
      type: 'object',
      properties: {
        to: { type: 'string' },
        subject: { type: 'string' },
        body: { type: 'string' },
      },
      required: ['to', 'subject', 'body'],
    },
  },
];
```

## Agentic Tool Loop

```typescript
async function runAgent(
  client: AcosmiClient,
  modelId: string,
  userMessage: string,
  tools: Tool[]
): Promise<string> {
  const messages: Message[] = [{ role: 'user', content: userMessage }];

  while (true) {
    const response = await client.chatComplete({ modelId, messages, tools });

    if (response.stopReason !== 'tool_use') {
      return response.content;
    }

    const toolResults = [];
    for (const block of response.content) {
      if (block.type === 'tool_use') {
        const result = await executeTool(block.name, block.input);
        toolResults.push({
          type: 'tool_result',
          toolUseId: block.id,
          content: String(result),
        });
      }
    }

    messages.push({ role: 'assistant', content: response.content });
    messages.push({ role: 'user', content: toolResults });
  }
}

async function executeTool(name: string, input: Record<string, unknown>) {
  switch (name) {
    case 'search_database':
      return searchDb(input.query as string, (input.limit as number) ?? 10);
    case 'send_email':
      return sendEmail(input as EmailParams);
    default:
      return `Unknown tool: ${name}`;
  }
}
```

## Streaming with Tools

```typescript
const stream = client.chatStream({ modelId, messages, tools });

const toolCalls: ToolUseEvent[] = [];
for await (const event of stream) {
  if (event.type === 'content') {
    process.stdout.write(event.text);
  } else if (event.type === 'tool_use') {
    toolCalls.push(event);
  }
}

for (const call of toolCalls) {
  const result = await executeTool(call.name, call.input);
  // Continue conversation...
}
```

## Error Handling in Tools

```typescript
async function executeTool(name: string, input: Record<string, unknown>) {
  try {
    const result = await toolDispatch[name](input);
    return { status: 'success', data: result };
  } catch (error) {
    return { status: 'error', message: String(error) };
  }
}
```
