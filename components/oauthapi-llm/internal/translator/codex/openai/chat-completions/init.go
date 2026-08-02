package chat_completions

import (
	. "github.com/acosmi/OAuthAPI-LLM/internal/constant"
	"github.com/acosmi/OAuthAPI-LLM/internal/interfaces"
	"github.com/acosmi/OAuthAPI-LLM/internal/translator/translator"
)

func init() {
	translator.Register(
		OpenAI,
		Codex,
		ConvertOpenAIRequestToCodex,
		interfaces.TranslateResponse{
			Stream:    ConvertCodexResponseToOpenAI,
			NonStream: ConvertCodexResponseToOpenAINonStream,
		},
	)
}
