package chat_completions

import (
	. "github.com/acosmi/OAuthAPI-LLM/internal/constant"
	"github.com/acosmi/OAuthAPI-LLM/internal/interfaces"
	"github.com/acosmi/OAuthAPI-LLM/internal/translator/translator"
)

func init() {
	translator.Register(
		OpenAI,
		Interactions,
		ConvertOpenAIRequestToInteractions,
		interfaces.TranslateResponse{
			Stream:    ConvertInteractionsResponseToOpenAI,
			NonStream: ConvertInteractionsResponseToOpenAINonStream,
		},
	)
	translator.Register(
		Interactions,
		OpenAI,
		ConvertInteractionsRequestToOpenAI,
		interfaces.TranslateResponse{
			Stream:    ConvertOpenAIResponseToInteractions,
			NonStream: ConvertOpenAIResponseToInteractionsNonStream,
		},
	)
}
