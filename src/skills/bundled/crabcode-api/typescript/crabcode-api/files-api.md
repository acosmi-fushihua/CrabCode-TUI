# Files API — TypeScript

## Overview

Upload files to reference across multiple chat requests without re-uploading.

## Upload

```typescript
const file = await client.uploadFile({
  filePath: 'document.pdf',
  purpose: 'chat',
});
console.log(`File ID: ${file.id}`);
```

## Use in Chat

```typescript
const response = await client.chatComplete({
  modelId,
  messages: [
    {
      role: 'user',
      content: [
        { type: 'file', fileId: file.id },
        { type: 'text', text: 'Summarize this document' },
      ],
    },
  ],
});
```

## List and Delete

```typescript
const files = await client.listFiles();
for (const f of files) {
  console.log(`${f.id}: ${f.filename} (${f.size} bytes)`);
}

await client.deleteFile(file.id);
```

## Best Practices

1. **Reuse file IDs** — Upload once, reference in multiple requests
2. **Set purpose** — `chat` for messages, `batch` for batch processing
3. **Clean up** — Delete files when no longer needed
4. **Check support** — Not all models support file inputs
