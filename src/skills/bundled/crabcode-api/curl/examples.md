# Acosmi API — curl Examples

All endpoints use base URL `https://acosmi.com/api/v4`.

## Authentication

After completing OAuth 2.1 PKCE flow, use the access token:

```bash
export ACOSMI_TOKEN="your_access_token_here"
```

## List Models

```bash
curl -s https://acosmi.com/api/v4/models \
  -H "Authorization: Bearer $ACOSMI_TOKEN" | jq '.data'
```

## Chat Completion (Non-streaming)

```bash
curl -s https://acosmi.com/api/v4/chat/completions \
  -H "Authorization: Bearer $ACOSMI_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model_id": "MODEL_ID_FROM_LIST_MODELS",
    "messages": [
      {"role": "user", "content": "Hello!"}
    ]
  }' | jq '.data.content'
```

## Chat Streaming (SSE)

```bash
curl -sN https://acosmi.com/api/v4/chat/stream \
  -H "Authorization: Bearer $ACOSMI_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model_id": "MODEL_ID_FROM_LIST_MODELS",
    "messages": [
      {"role": "user", "content": "Write a short poem"}
    ]
  }'
```

Note: Use `-N` (no buffer) for real-time SSE output.

## Chat with System Prompt

```bash
curl -s https://acosmi.com/api/v4/chat/completions \
  -H "Authorization: Bearer $ACOSMI_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model_id": "MODEL_ID",
    "system": "You are a helpful coding assistant",
    "messages": [
      {"role": "user", "content": "Explain recursion"}
    ],
    "temperature": 0.7,
    "max_tokens": 1024
  }'
```

## Chat with Tool Use

```bash
curl -s https://acosmi.com/api/v4/chat/completions \
  -H "Authorization: Bearer $ACOSMI_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "model_id": "MODEL_ID",
    "messages": [
      {"role": "user", "content": "What is the weather in Beijing?"}
    ],
    "tools": [
      {
        "name": "get_weather",
        "description": "Get current weather",
        "input_schema": {
          "type": "object",
          "properties": {
            "location": {"type": "string"}
          },
          "required": ["location"]
        }
      }
    ]
  }'
```

## Get Account Balance

```bash
curl -s https://acosmi.com/api/v4/balance \
  -H "Authorization: Bearer $ACOSMI_TOKEN" | jq '.data'
```

## Claim Monthly Free Credits

```bash
curl -s -X POST https://acosmi.com/api/v4/balance/claim-monthly \
  -H "Authorization: Bearer $ACOSMI_TOKEN" | jq
```

## List Token Packages

```bash
curl -s https://acosmi.com/api/v4/store/packages \
  -H "Authorization: Bearer $ACOSMI_TOKEN" | jq '.data'
```

## Get Model Capabilities

```bash
curl -s https://acosmi.com/api/v4/models/MODEL_ID/capabilities \
  -H "Authorization: Bearer $ACOSMI_TOKEN" | jq '.data.capabilities'
```

## Important

- Replace `MODEL_ID` / `MODEL_ID_FROM_LIST_MODELS` with actual IDs from the List Models response
- **Never hardcode model IDs** — always query `/models` first
- For streaming, don't set `--max-time` — SSE connections are long-lived
- Access tokens expire in 15 minutes; refresh using your OAuth refresh token
