# Acosmi SDK for PHP

## Installation

```bash
composer require acosmi/sdk
```

## Quick Start

```php
use Acosmi\AcosmiClient;

$client = new AcosmiClient(['server_url' => 'https://acosmi.com']);

// Authenticate (requires prior OAuth flow)
// Token stored at ~/.acosmi/tokens.json

// List models (NEVER hardcode model IDs)
$models = $client->listModels();
$defaultModel = array_values(array_filter($models, fn($m) => $m->isDefault && $m->isEnabled))[0];

// Chat completion
$response = $client->chatComplete([
    'model_id' => $defaultModel->id,
    'messages' => [
        ['role' => 'user', 'content' => 'Hello!'],
    ],
]);

echo $response->content;
```

## Streaming

```php
$stream = $client->chatStream([
    'model_id' => $modelId,
    'messages' => [['role' => 'user', 'content' => 'Write a poem']],
]);

foreach ($stream as $event) {
    match ($event->type) {
        'content' => print($event->text),
        'usage' => printf("\nTokens: %d in, %d out\n", $event->inputTokens, $event->outputTokens),
        'error' => fprintf(STDERR, "Error: %s\n", $event->message),
    };
}
```

## Tool Use

```php
$tools = [
    [
        'name' => 'get_weather',
        'description' => 'Get weather for a location',
        'input_schema' => [
            'type' => 'object',
            'properties' => [
                'location' => ['type' => 'string', 'description' => 'City name'],
            ],
            'required' => ['location'],
        ],
    ],
];

$response = $client->chatComplete([
    'model_id' => $modelId,
    'messages' => [['role' => 'user', 'content' => 'Weather in Beijing?']],
    'tools' => $tools,
]);

if ($response->stopReason === 'tool_use') {
    foreach ($response->content as $block) {
        if ($block->type === 'tool_use') {
            $result = executeTool($block->name, $block->input);
            // Send tool_result back...
        }
    }
}
```

## Error Handling

```php
use Acosmi\Exceptions\{RateLimitException, BusinessException, AuthenticationException};

try {
    $response = $client->chatComplete($request);
} catch (RateLimitException $e) {
    sleep($e->getRetryAfter());
    // retry
} catch (BusinessException $e) {
    echo "Error {$e->getCode()}: {$e->getMessage()}\n";
} catch (AuthenticationException $e) {
    // Re-authenticate
}
```

## Key Points

- Models are dynamic — always use `listModels()`
- Token refresh is automatic
- Don't set `CURLOPT_TIMEOUT` for streaming connections
