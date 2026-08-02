package quota

import (
	"context"
	"net/http"

	coreauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
)

type xAICurrentPeriod struct {
	End                string   `json:"end"`
	CreditUsagePercent *float64 `json:"creditUsagePercent"`
}

type xAIQuotaResponse struct {
	CurrentPeriod      *xAICurrentPeriod `json:"currentPeriod"`
	End                string            `json:"end"`
	CreditUsagePercent *float64          `json:"creditUsagePercent"`
}

func (response *xAIQuotaResponse) validateProviderJSON(payload []byte) error {
	root, err := decodeJSONObject(payload)
	if err != nil || root == nil {
		return ErrMalformedResponse
	}
	if err = rejectUnknownQuotaShapedFields(root,
		"currentPeriod", "end", "creditUsagePercent", "monthlyLimit",
		"onDemandCap", "onDemandUsed", "prepaidBalance", "isUnifiedBillingUser",
	); err != nil {
		return err
	}
	currentRaw := root["currentPeriod"]
	if len(currentRaw) == 0 || rawJSONNull(currentRaw) {
		return nil
	}
	current, err := decodeJSONObject(currentRaw)
	if err != nil || current == nil {
		return ErrMalformedResponse
	}
	if err = rejectUnknownQuotaShapedFields(current, "month", "end", "creditUsagePercent"); err != nil {
		return err
	}
	// The official response currently uses currentPeriod. A response carrying
	// both legacy top-level period fields and currentPeriod is ambiguous even
	// when the values happen to match, so reject rather than compose shapes.
	for _, legacyField := range []string{"end", "creditUsagePercent"} {
		if raw := root[legacyField]; len(raw) > 0 && !rawJSONNull(raw) {
			return ErrMalformedResponse
		}
	}
	return nil
}

func fetchXAI(ctx context.Context, client *http.Client, endpoint string, credential *coreauth.Auth) (Report, error) {
	token := credentialValue(credential, "access_token")
	if token == "" {
		return Report{}, ErrMissingCredential
	}
	headers := bearerHeaders(token)
	// These are the identity headers used by the audited official Grok CLI
	// 0.2.101 billing extension. They carry no account identity or credential.
	headers.Set("x-grok-client-version", "0.2.101")
	headers.Set("x-grok-client-mode", "billing")
	var response xAIQuotaResponse
	if err := requestJSON(ctx, client, http.MethodGet, endpoint, nil, headers, &response); err != nil {
		return Report{}, err
	}
	usedPercent := response.CreditUsagePercent
	resetAt, errReset := strictProviderReset(response.End)
	if errReset != nil {
		return Report{}, errReset
	}
	if response.CurrentPeriod != nil {
		// `currentPeriod` is one provider window. Once present, never fill one
		// of its missing fields from the legacy top-level representation: doing
		// so would synthesize a percentage/reset pair from two different shapes.
		usedPercent = response.CurrentPeriod.CreditUsagePercent
		resetAt, errReset = strictProviderReset(response.CurrentPeriod.End)
		if errReset != nil {
			return Report{}, errReset
		}
	}
	windows := []Window{}
	if usedPercent != nil || resetAt != nil {
		window := Window{Label: "billing-cycle", ResetsAt: resetAt}
		if usedPercent != nil {
			remaining, errRemaining := strictProviderRemainingFromUsedPercent(*usedPercent)
			if errRemaining != nil {
				return Report{}, errRemaining
			}
			window.RemainingPercent = remaining
		}
		windows = append(windows, window)
	}
	snapshot := normalizeSnapshot(windows, false)
	return Report{Account: &snapshot}, nil
}
