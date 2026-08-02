package quota

import (
	"context"
	"encoding/json"
	"math"
	"net/http"
	"strings"

	coreauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
)

type anthropicWindow struct {
	Utilization    *float64 `json:"utilization"`
	UsedPercentage *float64 `json:"used_percentage"`
	ResetsAt       string   `json:"resets_at"`
}

type anthropicExtraUsage struct {
	IsEnabled    *bool    `json:"is_enabled"`
	MonthlyLimit *float64 `json:"monthly_limit"`
	UsedCredits  *float64 `json:"used_credits"`
	ResetsAt     string   `json:"resets_at"`
}

type anthropicUsageResponse map[string]json.RawMessage

var anthropicWindowLabels = []string{
	"five_hour",
	"seven_day",
	"seven_day_opus",
	"seven_day_sonnet",
	"seven_day_overage_included",
	"seven_day_oauth_apps",
}

func (response *anthropicUsageResponse) validateProviderJSON(payload []byte) error {
	root, err := decodeJSONObject(payload)
	if err != nil || root == nil {
		return ErrMalformedResponse
	}
	allowed := append(append([]string{}, anthropicWindowLabels...), "extra_usage")
	if err = rejectUnknownQuotaShapedFields(root, allowed...); err != nil {
		return err
	}
	for _, label := range anthropicWindowLabels {
		raw := root[label]
		if len(raw) == 0 || rawJSONNull(raw) {
			continue
		}
		window, errWindow := decodeJSONObject(raw)
		if errWindow != nil || window == nil {
			return ErrMalformedResponse
		}
		if errWindow = rejectUnknownQuotaShapedFields(window, "utilization", "used_percentage", "resets_at"); errWindow != nil {
			return errWindow
		}
	}
	if raw := root["extra_usage"]; len(raw) > 0 && !rawJSONNull(raw) {
		extra, errExtra := decodeJSONObject(raw)
		if errExtra != nil || extra == nil {
			return ErrMalformedResponse
		}
		if errExtra = rejectUnknownQuotaShapedFields(extra, "is_enabled", "monthly_limit", "used_credits", "resets_at"); errExtra != nil {
			return errExtra
		}
	}
	return nil
}

func fetchAnthropic(ctx context.Context, client *http.Client, endpoint string, credential *coreauth.Auth) (Report, error) {
	token := credentialValue(credential, "access_token")
	if token == "" {
		return Report{}, ErrMissingCredential
	}
	headers := bearerHeaders(token)
	headers.Set("anthropic-beta", "oauth-2025-04-20")
	var response anthropicUsageResponse
	if err := requestJSON(ctx, client, http.MethodGet, endpoint, nil, headers, &response); err != nil {
		return Report{}, err
	}
	knownWindowLabels := make(map[string]struct{}, len(anthropicWindowLabels))
	for _, label := range anthropicWindowLabels {
		knownWindowLabels[label] = struct{}{}
	}
	for label, raw := range response {
		if _, known := knownWindowLabels[label]; known || label == "extra_usage" {
			continue
		}
		// New provider metadata is forward-compatible, but a new object with
		// quota-window fields cannot be ignored safely: it may be the limiting
		// window for every route. Refuse the aggregate until its applicability
		// is explicitly audited.
		if anthropicQuotaWindowShape(raw) {
			return Report{}, ErrMalformedResponse
		}
	}
	windows := make([]Window, 0, len(anthropicWindowLabels))
	unbound := make([]UnboundLimitSnapshot, 0, 2)
	malformedKnownWindow := false
	for _, label := range anthropicWindowLabels {
		raw, ok := response[label]
		if !ok || string(raw) == "null" {
			continue
		}
		var providerWindow anthropicWindow
		if err := json.Unmarshal(raw, &providerWindow); err != nil {
			malformedKnownWindow = true
			continue
		}
		if providerWindow.Utilization != nil && providerWindow.UsedPercentage != nil && *providerWindow.Utilization != *providerWindow.UsedPercentage {
			malformedKnownWindow = true
			continue
		}
		resetAt, errReset := strictProviderReset(providerWindow.ResetsAt)
		if errReset != nil {
			malformedKnownWindow = true
			continue
		}
		window := Window{Label: label, ResetsAt: resetAt}
		used := providerWindow.Utilization
		if used == nil {
			used = providerWindow.UsedPercentage
		}
		if used != nil {
			remaining, errRemaining := strictProviderRemainingFromUsedPercent(*used)
			if errRemaining != nil {
				malformedKnownWindow = true
				continue
			}
			window.RemainingPercent = remaining
		}
		switch label {
		case "seven_day_opus":
			// The provider response names a subscription bucket, but supplies no
			// exact model IDs to which it applies. Binding it by a token in the
			// model name would guess a model family and can overstate quota.
			unbound = append(unbound, UnboundLimitSnapshot{
				LimitID: label, LimitName: label,
				Snapshot: normalizeSnapshot([]Window{window}, false),
			})
		case "seven_day_sonnet":
			unbound = append(unbound, UnboundLimitSnapshot{
				LimitID: label, LimitName: label,
				Snapshot: normalizeSnapshot([]Window{window}, false),
			})
		default:
			windows = append(windows, window)
		}
	}
	if malformedKnownWindow {
		return Report{}, ErrMalformedResponse
	}
	if rawExtra, ok := response["extra_usage"]; ok && !rawJSONNull(rawExtra) {
		var extra anthropicExtraUsage
		if err := json.Unmarshal(rawExtra, &extra); err != nil {
			return Report{}, ErrMalformedResponse
		}
		if extra.IsEnabled == nil {
			unbound = append(unbound, UnboundLimitSnapshot{
				LimitID: "extra_usage", LimitName: "extra_usage",
				Snapshot: normalizeSnapshot([]Window{{Label: "extra_usage"}}, false),
			})
		} else if *extra.IsEnabled {
			resetAt, errReset := strictProviderReset(extra.ResetsAt)
			if errReset != nil {
				return Report{}, errReset
			}
			window := Window{Label: "extra_usage", ResetsAt: resetAt}
			if extra.MonthlyLimit == nil || extra.UsedCredits == nil || math.IsNaN(*extra.MonthlyLimit) || math.IsInf(*extra.MonthlyLimit, 0) || math.IsNaN(*extra.UsedCredits) || math.IsInf(*extra.UsedCredits, 0) || *extra.MonthlyLimit <= 0 || *extra.UsedCredits < 0 {
				return Report{}, ErrMalformedResponse
			}
			window.Limit = cloneFloat(extra.MonthlyLimit)
			window.Used = cloneFloat(extra.UsedCredits)
			unbound = append(unbound, UnboundLimitSnapshot{
				LimitID: "extra_usage", LimitName: "extra_usage",
				Snapshot: normalizeSnapshot([]Window{window}, false),
			})
		}
	}
	snapshot := normalizeSnapshot(windows, false)
	report := Report{Account: &snapshot, UnboundLimits: unbound}
	return report, nil
}

func anthropicQuotaWindowShape(raw json.RawMessage) bool {
	if len(raw) == 0 || string(raw) == "null" {
		return false
	}
	var object map[string]json.RawMessage
	if err := json.Unmarshal(raw, &object); err != nil || object == nil {
		return false
	}
	for field := range object {
		switch strings.ToLower(strings.TrimSpace(field)) {
		case "utilization",
			"used_percentage",
			"remaining_percentage",
			"used_percent",
			"remaining_percent",
			"resets_at",
			"reset_at",
			"reset_time",
			"monthly_limit",
			"used_credits",
			"is_enabled":
			return true
		}
	}
	return false
}
