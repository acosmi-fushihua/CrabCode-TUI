package quota

import (
	"context"
	"encoding/json"
	"net/http"

	coreauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
)

type openAIWindow struct {
	UsedPercent *float64 `json:"used_percent"`
	ResetAt     *int64   `json:"reset_at"`
}

type openAIRateLimit struct {
	LimitReached    bool          `json:"limit_reached"`
	Allowed         *bool         `json:"allowed"`
	PrimaryWindow   *openAIWindow `json:"primary_window"`
	SecondaryWindow *openAIWindow `json:"secondary_window"`
}

type openAISpendControlLimit struct {
	RemainingPercent *float64 `json:"remaining_percent"`
	ResetAt          *int64   `json:"reset_at"`
}

type openAISpendControl struct {
	Reached         bool                     `json:"reached"`
	IndividualLimit *openAISpendControlLimit `json:"individual_limit"`
}

type openAIAdditionalRateLimit struct {
	LimitName      string           `json:"limit_name"`
	MeteredFeature string           `json:"metered_feature"`
	RateLimit      *openAIRateLimit `json:"rate_limit"`
}

type openAIRateLimitReachedType struct {
	Type string `json:"type"`
}

type openAIUsagePayload struct {
	RateLimit            *openAIRateLimit            `json:"rate_limit"`
	SpendControl         *openAISpendControl         `json:"spend_control"`
	AdditionalRateLimits []openAIAdditionalRateLimit `json:"additional_rate_limits"`
	RateLimitReachedType *openAIRateLimitReachedType `json:"rate_limit_reached_type"`
}

type openAIUsageEnvelope struct {
	openAIUsagePayload
	RateLimits *openAIUsagePayload `json:"rate_limits"`
}

func (response *openAIUsageEnvelope) validateProviderJSON(payload []byte) error {
	root, err := decodeJSONObject(payload)
	if err != nil || root == nil {
		return ErrMalformedResponse
	}
	if err = rejectUnknownQuotaShapedFields(root,
		"plan_type", "rate_limit", "credits", "spend_control",
		"additional_rate_limits", "rate_limit_reached_type", "rate_limits",
	); err != nil {
		return err
	}
	selected := root
	if nestedRaw, hasNested := root["rate_limits"]; hasNested && !rawJSONNull(nestedRaw) {
		for _, topLevelField := range []string{"rate_limit", "credits", "spend_control", "additional_rate_limits", "rate_limit_reached_type"} {
			if raw, present := root[topLevelField]; present && !rawJSONNull(raw) {
				return ErrMalformedResponse
			}
		}
		selected, err = decodeJSONObject(nestedRaw)
		if err != nil || selected == nil {
			return ErrMalformedResponse
		}
	}
	return validateOpenAIUsagePayloadObject(selected)
}

func validateOpenAIUsagePayloadObject(object map[string]json.RawMessage) error {
	if err := rejectUnknownQuotaShapedFields(object,
		"plan_type", "rate_limit", "credits", "spend_control",
		"additional_rate_limits", "rate_limit_reached_type", "rate_limits",
	); err != nil {
		return err
	}
	if err := validateOpenAIRateLimitRaw(object["rate_limit"]); err != nil {
		return err
	}
	if raw := object["credits"]; len(raw) > 0 && !rawJSONNull(raw) {
		credits, err := decodeJSONObject(raw)
		if err != nil || credits == nil {
			return ErrMalformedResponse
		}
		if err = rejectUnknownQuotaShapedFields(credits, "has_credits", "unlimited", "balance"); err != nil {
			return err
		}
	}
	if raw := object["spend_control"]; len(raw) > 0 && !rawJSONNull(raw) {
		spend, err := decodeJSONObject(raw)
		if err != nil || spend == nil {
			return ErrMalformedResponse
		}
		if err = rejectUnknownQuotaShapedFields(spend, "reached", "individual_limit"); err != nil {
			return err
		}
		if individualRaw := spend["individual_limit"]; len(individualRaw) > 0 && !rawJSONNull(individualRaw) {
			individual, errIndividual := decodeJSONObject(individualRaw)
			if errIndividual != nil || individual == nil {
				return ErrMalformedResponse
			}
			if errIndividual = rejectUnknownQuotaShapedFields(individual,
				"source", "limit", "used", "remaining", "used_percent",
				"remaining_percent", "reset_after_seconds", "reset_at",
			); errIndividual != nil {
				return errIndividual
			}
		}
	}
	if raw := object["additional_rate_limits"]; len(raw) > 0 && !rawJSONNull(raw) {
		var additional []json.RawMessage
		if err := json.Unmarshal(raw, &additional); err != nil {
			return ErrMalformedResponse
		}
		for _, itemRaw := range additional {
			item, err := decodeJSONObject(itemRaw)
			if err != nil || item == nil {
				return ErrMalformedResponse
			}
			if err = rejectUnknownQuotaShapedFields(item, "limit_name", "metered_feature", "rate_limit"); err != nil {
				return err
			}
			if err = validateOpenAIRateLimitRaw(item["rate_limit"]); err != nil {
				return err
			}
		}
	}
	if raw := object["rate_limit_reached_type"]; len(raw) > 0 && !rawJSONNull(raw) {
		reached, err := decodeJSONObject(raw)
		if err != nil || reached == nil {
			return ErrMalformedResponse
		}
		if err = rejectUnknownQuotaShapedFields(reached, "type"); err != nil {
			return err
		}
	}
	return nil
}

func validateOpenAIRateLimitRaw(raw json.RawMessage) error {
	if len(raw) == 0 || rawJSONNull(raw) {
		return nil
	}
	rateLimit, err := decodeJSONObject(raw)
	if err != nil || rateLimit == nil {
		return ErrMalformedResponse
	}
	if err = rejectUnknownQuotaShapedFields(rateLimit, "allowed", "limit_reached", "primary_window", "secondary_window"); err != nil {
		return err
	}
	for _, field := range []string{"primary_window", "secondary_window"} {
		windowRaw := rateLimit[field]
		if len(windowRaw) == 0 || rawJSONNull(windowRaw) {
			continue
		}
		window, errWindow := decodeJSONObject(windowRaw)
		if errWindow != nil || window == nil {
			return ErrMalformedResponse
		}
		if errWindow = rejectUnknownQuotaShapedFields(window, "used_percent", "limit_window_seconds", "reset_after_seconds", "reset_at"); errWindow != nil {
			return errWindow
		}
	}
	return nil
}

func fetchOpenAI(ctx context.Context, client *http.Client, endpoint string, credential *coreauth.Auth) (Report, error) {
	token := credentialValue(credential, "access_token")
	if token == "" {
		return Report{}, ErrMissingCredential
	}
	headers := bearerHeaders(token)
	headers.Set("User-Agent", "codex-cli")
	if accountID := credentialValue(credential, "account_id"); accountID != "" {
		headers.Set("ChatGPT-Account-Id", accountID)
	}
	var response openAIUsageEnvelope
	if err := requestJSON(ctx, client, http.MethodGet, endpoint, nil, headers, &response); err != nil {
		return Report{}, err
	}
	payload := &response.openAIUsagePayload
	if response.RateLimits != nil {
		payload = response.RateLimits
	}

	windows, err := openAIRateLimitWindows(payload.RateLimit)
	if err != nil {
		return Report{}, err
	}
	if payload.SpendControl != nil && payload.SpendControl.IndividualLimit != nil {
		individualLimit := payload.SpendControl.IndividualLimit
		remainingPercent, errRemaining := strictProviderRemainingPercentValue(individualLimit.RemainingPercent)
		if errRemaining != nil {
			return Report{}, errRemaining
		}
		resetAt, errReset := strictProviderUnixReset(individualLimit.ResetAt)
		if errReset != nil {
			return Report{}, errReset
		}
		windows = append(windows, Window{
			Label:            "spend_control",
			RemainingPercent: remainingPercent,
			ResetsAt:         resetAt,
		})
	}
	explicitExhausted := openAIRateLimitExhausted(payload.RateLimit) ||
		(payload.SpendControl != nil && payload.SpendControl.Reached) ||
		openAIGlobalReachedType(payload.RateLimitReachedType)
	snapshot := normalizeSnapshot(windows, explicitExhausted)
	report := Report{Account: &snapshot}
	for _, additional := range payload.AdditionalRateLimits {
		additionalWindows, errAdditional := openAIRateLimitWindows(additional.RateLimit)
		if errAdditional != nil {
			return Report{}, errAdditional
		}
		additionalSnapshot := normalizeSnapshot(
			additionalWindows,
			openAIRateLimitExhausted(additional.RateLimit),
		)
		report.UnboundLimits = append(report.UnboundLimits, UnboundLimitSnapshot{
			LimitID:   additional.MeteredFeature,
			LimitName: additional.LimitName,
			Snapshot:  additionalSnapshot,
		})
	}
	return report, nil
}

func openAIRateLimitWindows(rateLimit *openAIRateLimit) ([]Window, error) {
	if rateLimit == nil {
		return nil, nil
	}
	windows := make([]Window, 0, 2)
	appendWindow := func(label string, providerWindow *openAIWindow) error {
		if providerWindow == nil {
			return nil
		}
		resetAt, errReset := strictProviderUnixReset(providerWindow.ResetAt)
		if errReset != nil {
			return errReset
		}
		window := Window{Label: label, ResetsAt: resetAt}
		if providerWindow.UsedPercent != nil {
			remaining, errRemaining := strictProviderRemainingFromUsedPercent(*providerWindow.UsedPercent)
			if errRemaining != nil {
				return errRemaining
			}
			window.RemainingPercent = remaining
		}
		windows = append(windows, window)
		return nil
	}
	if err := appendWindow("primary", rateLimit.PrimaryWindow); err != nil {
		return nil, err
	}
	if err := appendWindow("secondary", rateLimit.SecondaryWindow); err != nil {
		return nil, err
	}
	return windows, nil
}

func openAIRateLimitExhausted(rateLimit *openAIRateLimit) bool {
	return rateLimit != nil && (rateLimit.LimitReached || (rateLimit.Allowed != nil && !*rateLimit.Allowed))
}

func openAIGlobalReachedType(reachedType *openAIRateLimitReachedType) bool {
	if reachedType == nil {
		return false
	}
	switch reachedType.Type {
	case "rate_limit_reached",
		"workspace_owner_credits_depleted",
		"workspace_member_credits_depleted",
		"workspace_owner_usage_limit_reached",
		"workspace_member_usage_limit_reached":
		return true
	default:
		return false
	}
}

func strictProviderRemainingPercentValue(value *float64) (*float64, error) {
	if value == nil {
		return nil, nil
	}
	return strictProviderRemainingPercent(*value)
}
