# Batch Processing — TypeScript

## Overview

Batch processing sends multiple requests at once for non-latency-sensitive workloads at reduced cost.

## Usage

```typescript
// Create batch
const batch = await client.createBatch({
  modelId,
  requests: [
    {
      customId: 'req-1',
      messages: [{ role: 'user', content: 'Summarize article 1...' }],
    },
    {
      customId: 'req-2',
      messages: [{ role: 'user', content: 'Summarize article 2...' }],
    },
  ],
});

console.log(`Batch ${batch.id}: ${batch.status}`);

// Poll for completion
while (batch.status !== 'completed') {
  await sleep(30_000);
  batch = await client.getBatch(batch.id);
  console.log(`Status: ${batch.status}, ${batch.completed}/${batch.total}`);
}

// Get results
const results = await client.getBatchResults(batch.id);
for (const result of results) {
  console.log(`${result.customId}: ${result.content}`);
}
```

## Best Practices

1. **Check capabilities** — Verify `capabilities.supportsBatch`
2. **Use customId** — Set unique IDs for matching results to requests
3. **Handle partial failures** — Check individual result status
4. **Reasonable sizes** — Keep under 1000 requests per batch
