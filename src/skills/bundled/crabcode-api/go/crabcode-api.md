# Acosmi SDK for Go

## Installation

```bash
go get github.com/Acosmi/acosmi-sdk-go@latest
```

Current version: **v0.3.1**

## Quick Start

```go
package main

import (
    "context"
    "fmt"
    "github.com/Acosmi/acosmi-sdk-go/acosmi"
)

func main() {
    ctx := context.Background()

    // Create client (production)
    client, err := acosmi.NewClient(acosmi.Config{
        ServerURL: "https://acosmi.com",  // SDK auto-appends /api/v4
    })
    if err != nil {
        panic(err)
    }

    // Authenticate via OAuth 2.1 PKCE
    if !client.IsAuthorized() {
        err = client.Login(ctx, "MyApp", acosmi.AllScopes())
        if err != nil {
            panic(err)
        }
    }

    // List available models (NEVER hardcode model IDs)
    models, err := client.ListModels(ctx)
    if err != nil {
        panic(err)
    }

    // Find default model
    var modelID string
    for _, m := range models {
        if m.IsDefault && m.IsEnabled {
            modelID = m.ID
            break
        }
    }

    // Streaming chat
    req := acosmi.ChatRequest{
        RawMessages: []acosmi.ChatMessage{
            {Role: "user", Content: "Hello!"},
        },
    }
    eventsCh, sourcesCh, settleCh, errCh := client.ChatStreamWithUsage(ctx, modelID, req)

    for event := range eventsCh {
        fmt.Print(event.Data)
    }
    // Drain remaining channels
    for range sourcesCh {}
    settle := <-settleCh
    if err := <-errCh; err != nil {
        panic(err)
    }
    fmt.Printf("\nTokens used: %d input, %d output\n", settle.InputTokens, settle.OutputTokens)
}
```

## Authentication

### OAuth 2.1 PKCE

```go
// Simple login (opens browser)
client.Login(ctx, "AppName", acosmi.AllScopes())

// Login with event handler (for custom UI)
client.LoginWithHandler(ctx, "CrabCode", scopes, func(event acosmi.LoginEvent) {
    switch event.Type {
    case "url":
        fmt.Println("Open:", event.URL)
    case "code":
        fmt.Println("Enter code:", event.Code)
    case "success":
        fmt.Println("Logged in!")
    }
}, opts...)

// Check status / logout
client.IsAuthorized()  // bool
client.Logout(ctx)
```

**Scopes:** `ai` | `skills` | `account`
**Token storage:** `~/.acosmi/tokens.json` (default)
**Token lifecycle:** Access 15min, Refresh 7d, auto-refresh 30s before expiry

### Custom Token Store

```go
client, _ := acosmi.NewClient(acosmi.Config{
    ServerURL: "https://acosmi.com",
    Store:     myCustomStore, // implements acosmi.TokenStore interface
})
```

## Models

```go
// List all models
models, _ := client.ListModels(ctx)  // []ManagedModel

// Get specific model capabilities (cached 5min)
caps, _ := client.GetModelCapabilities(ctx, modelID)  // ModelCapabilities
```

### ManagedModel struct

```go
type ManagedModel struct {
    ID, Name, Provider, ModelID string
    MaxTokens, ContextWindow    int
    IsEnabled, IsDefault        bool
    PricePerMTok                float64
    Capabilities                ModelCapabilities  // 17 capability flags
}
```

## Chat

> **Note:** The SDK only provides streaming APIs. There is no non-streaming `ChatComplete` method. Use `ChatStream` (2-channel) or `ChatStreamWithUsage` (4-channel, recommended).

### ChatStreamWithUsage (recommended, v0.3.0+)

Returns 4 channels: `StreamEvent`, `SourcesEvent`, `StreamSettlement`, `error`.

```go
eventsCh, sourcesCh, settleCh, errCh := client.ChatStreamWithUsage(ctx, modelID, req)

for event := range eventsCh {
    fmt.Print(event.Data)  // event.Event = type, event.Data = content text
}
for source := range sourcesCh {
    for _, s := range source.Sources {
        fmt.Println("Source:", s.URL)
    }
}
settle := <-settleCh  // StreamSettlement with token counts
err := <-errCh        // Final error (nil on success)
```

### ChatStream (legacy, 2-channel)

```go
eventsCh, errCh := client.ChatStream(ctx, modelID, req)
for event := range eventsCh {
    fmt.Print(event.Data)
}
if err := <-errCh; err != nil {
    panic(err)
}
```

### ChatRequest Extended Fields

All extension fields use `json:"-"` and are serialized internally:

```go
req := acosmi.ChatRequest{
    RawMessages: messages,
    System:      "You are a helpful assistant",
    Tools:       tools,
    Temperature: floatPtr(0.7),
    MaxTokens:   1024,  // int, not pointer
    Thinking:    &acosmi.ThinkingConfig{Type: "enabled", BudgetTokens: 4096},
    ServerTools: []acosmi.ServerTool{{Type: "web_search", Name: "web_search"}},
    Speed:       "fast",
    Betas:       []string{},  // Auto-assembled from model capabilities
    ExtraBody:   map[string]interface{}{},
    Metadata:    map[string]string{},
}
```

### Tool Use

```go
tools := []acosmi.Tool{
    {
        Name:        "get_weather",
        Description: "Get weather for a location",
        InputSchema: json.RawMessage(`{
            "type": "object",
            "properties": {
                "location": {"type": "string"}
            },
            "required": ["location"]
        }`),
    },
}

req := acosmi.ChatRequest{
    RawMessages: messages,
    Tools:       tools,
}
```

## Balance & Billing

```go
balance, _ := client.GetBalance(ctx)          // Aggregated balance
detail, _ := client.GetBalanceDetail(ctx)     // Line-item detail
_ = client.ClaimMonthlyFree(ctx)              // Claim free credits (idempotent)
```

## Token Package Store

```go
packages, _ := client.ListTokenPackages(ctx)
order, _ := client.BuyTokenPackage(ctx, packageID, paymentMethod)
result, _ := client.WaitForPayment(ctx, order.ID, 3*time.Second)
```

## Skill Store

```go
skills, _ := client.BrowseSkillStore(ctx, "web scraper")
_ = client.InstallSkill(ctx, skillID)
_ = client.UploadSkill(ctx, zipData, "public", "published")
generated, _ := client.GenerateSkill(ctx, acosmi.GenerateSkillRequest{
    Description: "A skill that summarizes web pages",
})
```

## WebSocket Real-time Push

```go
client.Connect(ctx, acosmi.WSConfig{
    Topics: []string{"balance", "skills", "system"},
    OnEvent: func(event acosmi.WSEvent) {
        switch event.Topic {
        case "balance":
            fmt.Println("Balance updated:", event.Data)
        case "system":
            fmt.Println("System notice:", event.Data)
        }
    },
})
// Auto-reconnect with exponential backoff (2s-60s)
// 13 system notification types
```

## Error Handling

```go
eventsCh, _, _, errCh := client.ChatStreamWithUsage(ctx, modelID, req)
for range eventsCh {}
if err := <-errCh; err != nil {
    switch e := err.(type) {
    case *acosmi.RateLimitError:
        time.Sleep(e.RetryAfter)
        // retry
    case *acosmi.BusinessError:
        fmt.Println("Business error:", e.Code, e.Message)
    case *acosmi.AuthenticationError:
        client.Login(ctx, "MyApp", acosmi.AllScopes())
    default:
        fmt.Println("Unknown error:", err)
    }
}
```

## Important Notes

- **Never set global HTTP Timeout** on the client — it truncates SSE streams
- **Models are dynamic** — Always use `ListModels()`, never hardcode IDs
- **Beta headers** — Automatically assembled from model capabilities; no manual management needed
- **Token auto-refresh** — SDK refreshes access tokens 30s before expiry
