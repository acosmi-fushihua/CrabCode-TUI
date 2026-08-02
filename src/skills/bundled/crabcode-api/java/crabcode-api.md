# Acosmi SDK for Java

## Installation

### Maven
```xml
<dependency>
    <groupId>ai.acosmi</groupId>
    <artifactId>acosmi-sdk</artifactId>
    <version>LATEST</version>
</dependency>
```

### Gradle
```groovy
implementation 'ai.acosmi:acosmi-sdk:LATEST'
```

## Quick Start

```java
import ai.acosmi.AcosmiClient;
import ai.acosmi.model.ManagedModel;

AcosmiClient client = AcosmiClient.builder()
    .serverUrl("https://acosmi.com")
    .build();

// Authenticate
if (!client.isAuthorized()) {
    client.login("MyApp", Scopes.all());
}

// List models (NEVER hardcode model IDs)
List<ManagedModel> models = client.listModels();
ManagedModel defaultModel = models.stream()
    .filter(m -> m.isDefault() && m.isEnabled())
    .findFirst()
    .orElseThrow();

// Chat completion
ChatResponse response = client.chatComplete(ChatRequest.builder()
    .modelId(defaultModel.getId())
    .addMessage("user", "Hello!")
    .build());

System.out.println(response.getContent());
```

## Streaming

```java
Stream<ChatEvent> stream = client.chatStream(ChatRequest.builder()
    .modelId(modelId)
    .addMessage("user", "Write a story")
    .build());

stream.forEach(event -> {
    switch (event.getType()) {
        case CONTENT -> System.out.print(event.getText());
        case USAGE -> System.out.printf(
            "%nTokens: %d in, %d out%n",
            event.getInputTokens(), event.getOutputTokens());
        case ERROR -> System.err.println("Error: " + event.getMessage());
    }
});
```

## Tool Use

```java
List<Tool> tools = List.of(
    Tool.builder()
        .name("get_weather")
        .description("Get weather for a location")
        .inputSchema("""
            {"type":"object","properties":{"location":{"type":"string"}},"required":["location"]}
            """)
        .build()
);

ChatResponse response = client.chatComplete(ChatRequest.builder()
    .modelId(modelId)
    .addMessage("user", "Weather in Beijing?")
    .tools(tools)
    .build());

if (response.getStopReason() == StopReason.TOOL_USE) {
    for (ContentBlock block : response.getContent()) {
        if (block.getType() == BlockType.TOOL_USE) {
            String result = executeTool(block.getName(), block.getInput());
            // Send tool_result back...
        }
    }
}
```

## Error Handling

```java
try {
    ChatResponse response = client.chatComplete(request);
} catch (RateLimitException e) {
    Thread.sleep(e.getRetryAfterMs());
    // retry
} catch (BusinessException e) {
    System.err.printf("Error %d: %s%n", e.getCode(), e.getMessage());
} catch (AuthenticationException e) {
    client.login("MyApp", Scopes.all());
}
```

## Key Points

- Models are dynamic — always use `listModels()`
- Don't set HTTP read timeout for streaming connections
- Token refresh is automatic
- All API calls require valid OAuth tokens with appropriate scopes
