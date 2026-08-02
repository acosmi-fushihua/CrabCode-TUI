package gemini

import (
	. "github.com/acosmi/OAuthAPI-LLM/internal/constant"
	"github.com/acosmi/OAuthAPI-LLM/internal/interfaces"
	"github.com/acosmi/OAuthAPI-LLM/internal/translator/translator"
)

func init() {
	translator.Register(
		Gemini,
		OpenAI,
		ConvertGeminiRequestToOpenAI,
		interfaces.TranslateResponse{
			Stream:     ConvertOpenAIResponseToGemini,
			NonStream:  ConvertOpenAIResponseToGeminiNonStream,
			TokenCount: GeminiTokenCount,
		},
	)
}
