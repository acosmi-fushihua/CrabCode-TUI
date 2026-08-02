# Batch Processing — Python

## Overview

Batch processing allows sending multiple requests at once for non-latency-sensitive workloads at a reduced cost.

## Usage

```python
# Create a batch of requests
batch = client.create_batch(
    model_id=model_id,
    requests=[
        {
            "custom_id": "req-1",
            "messages": [{"role": "user", "content": "Summarize article 1..."}],
        },
        {
            "custom_id": "req-2",
            "messages": [{"role": "user", "content": "Summarize article 2..."}],
        },
    ],
)

print(f"Batch ID: {batch.id}, Status: {batch.status}")

# Poll for completion
import time
while batch.status != "completed":
    time.sleep(30)
    batch = client.get_batch(batch.id)
    print(f"Status: {batch.status}, Progress: {batch.completed}/{batch.total}")

# Get results
results = client.get_batch_results(batch.id)
for result in results:
    print(f"{result.custom_id}: {result.content}")
```

## Best Practices

1. **Check capabilities** — Verify `capabilities.supports_batch` before using
2. **Use custom_id** — Always set unique IDs for matching results to requests
3. **Handle partial failures** — Some requests may fail; check individual result status
4. **Reasonable batch sizes** — Keep batches under 1000 requests for manageable processing
