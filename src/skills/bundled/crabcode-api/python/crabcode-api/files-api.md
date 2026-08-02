# Files API — Python

## Overview

The Files API allows uploading files to be referenced across multiple chat requests, avoiding re-uploading the same content.

## Upload a File

```python
file = client.upload_file(
    file_path="document.pdf",
    purpose="chat",  # or "batch"
)
print(f"File ID: {file.id}")
```

## Use in Chat

```python
response = client.chat_complete(
    model_id=model_id,
    messages=[
        {
            "role": "user",
            "content": [
                {"type": "file", "file_id": file.id},
                {"type": "text", "text": "Summarize this document"},
            ],
        }
    ],
)
```

## List and Delete Files

```python
# List uploaded files
files = client.list_files()
for f in files:
    print(f"{f.id}: {f.filename} ({f.size} bytes)")

# Delete a file
client.delete_file(file.id)
```

## Best Practices

1. **Reuse file IDs** — Upload once, reference in multiple requests
2. **Set purpose correctly** — `chat` for chat messages, `batch` for batch processing
3. **Clean up** — Delete files when no longer needed
4. **Check model support** — Not all models support file inputs; check capabilities
