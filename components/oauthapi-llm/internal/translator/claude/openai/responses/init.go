package responses

import (
	. "github.com/acosmi/OAuthAPI-LLM/internal/constant"
	"github.com/acosmi/OAuthAPI-LLM/internal/interfaces"
	"github.com/acosmi/OAuthAPI-LLM/internal/translator/translator"
)

func init() {
	translator.Register(
		OpenaiResponse,
		Claude,
		ConvertOpenAIResponsesRequestToClaude,
		interfaces.TranslateResponse{
			Stream:    ConvertClaudeResponseToOpenAIResponses,
			NonStream: ConvertClaudeResponseToOpenAIResponsesNonStream,
		},
	)
}
