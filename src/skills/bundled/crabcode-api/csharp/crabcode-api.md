# Acosmi SDK for C#

## Installation

```bash
dotnet add package Acosmi.SDK
```

## Quick Start

```csharp
using Acosmi;

var client = new AcosmiClient(new AcosmiConfig
{
    ServerUrl = "https://acosmi.com"
});

// Authenticate
if (!client.IsAuthorized)
    await client.LoginAsync("MyApp", Scopes.All);

// List models (NEVER hardcode model IDs)
var models = await client.ListModelsAsync();
var defaultModel = models.First(m => m.IsDefault && m.IsEnabled);

// Chat completion
var response = await client.ChatCompleteAsync(new ChatRequest
{
    ModelId = defaultModel.Id,
    Messages = new[] { new Message("user", "Hello!") }
});

Console.WriteLine(response.Content);
```

## Streaming

```csharp
await foreach (var evt in client.ChatStreamAsync(new ChatRequest
{
    ModelId = modelId,
    Messages = new[] { new Message("user", "Write a poem") }
}))
{
    switch (evt.Type)
    {
        case EventType.Content:
            Console.Write(evt.Text);
            break;
        case EventType.Usage:
            Console.WriteLine($"\nTokens: {evt.InputTokens} in, {evt.OutputTokens} out");
            break;
        case EventType.Error:
            Console.Error.WriteLine($"Error: {evt.Message}");
            break;
    }
}
```

## Tool Use

```csharp
var tools = new[]
{
    new Tool
    {
        Name = "get_weather",
        Description = "Get weather for a location",
        InputSchema = JsonDocument.Parse("""
            {"type":"object","properties":{"location":{"type":"string"}},"required":["location"]}
        """)
    }
};

var response = await client.ChatCompleteAsync(new ChatRequest
{
    ModelId = modelId,
    Messages = new[] { new Message("user", "Weather in Beijing?") },
    Tools = tools
});

if (response.StopReason == "tool_use")
{
    foreach (var block in response.Content.Where(b => b.Type == "tool_use"))
    {
        var result = await ExecuteToolAsync(block.Name, block.Input);
        // Send tool_result back...
    }
}
```

## Error Handling

```csharp
try
{
    var response = await client.ChatCompleteAsync(request);
}
catch (RateLimitException ex)
{
    await Task.Delay(ex.RetryAfter);
    // retry
}
catch (BusinessException ex)
{
    Console.Error.WriteLine($"Error {ex.Code}: {ex.Message}");
}
catch (AuthenticationException)
{
    await client.LoginAsync("MyApp", Scopes.All);
}
```

## Key Points

- Models are dynamic — always use `ListModelsAsync()`
- Token refresh is automatic
- Use `IAsyncEnumerable` for streaming (no HTTP timeout needed)
