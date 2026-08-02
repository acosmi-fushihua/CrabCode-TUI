# Acosmi SDK for Ruby

## Installation

```ruby
gem 'acosmi-sdk'
```

## Quick Start

```ruby
require 'acosmi'

client = Acosmi::Client.new(server_url: 'https://acosmi.com')

# Authenticate
client.login('MyApp', scopes: %w[ai skills account]) unless client.authorized?

# List models (NEVER hardcode model IDs)
models = client.list_models
default_model = models.find { |m| m.default? && m.enabled? }

# Chat completion
response = client.chat_complete(
  model_id: default_model.id,
  messages: [{ role: 'user', content: 'Hello!' }]
)
puts response.content
```

## Streaming

```ruby
client.chat_stream(
  model_id: model_id,
  messages: [{ role: 'user', content: 'Write a poem' }]
) do |event|
  case event.type
  when :content
    print event.text
  when :usage
    puts "\nTokens: #{event.input_tokens} in, #{event.output_tokens} out"
  when :error
    warn "Error: #{event.message}"
  end
end
```

## Tool Use

```ruby
tools = [
  {
    name: 'get_weather',
    description: 'Get weather for a location',
    input_schema: {
      type: 'object',
      properties: { location: { type: 'string' } },
      required: ['location']
    }
  }
]

response = client.chat_complete(
  model_id: model_id,
  messages: [{ role: 'user', content: 'Weather in Beijing?' }],
  tools: tools
)

if response.stop_reason == 'tool_use'
  response.content.each do |block|
    if block.type == 'tool_use'
      result = execute_tool(block.name, block.input)
      # Send tool_result back...
    end
  end
end
```

## Error Handling

```ruby
begin
  response = client.chat_complete(model_id: model_id, messages: messages)
rescue Acosmi::RateLimitError => e
  sleep(e.retry_after)
  retry
rescue Acosmi::BusinessError => e
  puts "Error #{e.code}: #{e.message}"
rescue Acosmi::AuthenticationError
  client.login('MyApp', scopes: %w[ai])
end
```

## Key Points

- Models are dynamic — always use `list_models`
- Token refresh is automatic
- Don't set HTTP read timeout for streaming
