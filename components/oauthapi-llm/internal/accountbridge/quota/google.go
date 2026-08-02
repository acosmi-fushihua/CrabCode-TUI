package quota

import (
	"context"
	"encoding/json"
	"net/http"
	"strings"

	coreauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
)

type googleQuotaBucket struct {
	RemainingAmount   *string  `json:"remainingAmount"`
	RemainingFraction *float64 `json:"remainingFraction"`
	ResetTime         string   `json:"resetTime"`
	TokenType         string   `json:"tokenType"`
	ModelID           string   `json:"modelId"`
}

type googleQuotaResponse struct {
	Buckets []googleQuotaBucket `json:"buckets"`
}

func (response *googleQuotaResponse) validateProviderJSON(payload []byte) error {
	root, err := decodeJSONObject(payload)
	if err != nil || root == nil {
		return ErrMalformedResponse
	}
	if err = rejectUnknownQuotaShapedFields(root, "buckets"); err != nil {
		return err
	}
	raw := root["buckets"]
	if len(raw) == 0 || rawJSONNull(raw) {
		return nil
	}
	var buckets []json.RawMessage
	if err = json.Unmarshal(raw, &buckets); err != nil {
		return ErrMalformedResponse
	}
	for _, bucketRaw := range buckets {
		bucket, errBucket := decodeJSONObject(bucketRaw)
		if errBucket != nil || bucket == nil {
			return ErrMalformedResponse
		}
		if errBucket = rejectUnknownQuotaShapedFields(bucket, "remainingAmount", "remainingFraction", "resetTime", "tokenType", "modelId"); errBucket != nil {
			return errBucket
		}
	}
	return nil
}

func fetchGoogle(ctx context.Context, client *http.Client, endpoint string, credential *coreauth.Auth) (Report, error) {
	token := credentialValue(credential, "access_token")
	projectID := credentialValue(credential, "project_id", "projectId")
	if token == "" || projectID == "" {
		return Report{}, ErrMissingCredential
	}
	headers := bearerHeaders(token)
	var response googleQuotaResponse
	if err := requestJSON(ctx, client, http.MethodPost, endpoint, map[string]string{"project": projectID}, headers, &response); err != nil {
		return Report{}, err
	}
	grouped := make(map[string][]Window)
	seenBuckets := make(map[string]struct{}, len(response.Buckets))
	for _, bucket := range response.Buckets {
		modelID := strings.TrimSpace(bucket.ModelID)
		if modelID == "" {
			// BucketInfo.modelId is optional in the wire schema. Without it the
			// bucket's scope is unknowable, so applying only the named buckets
			// could overstate a route's remaining quota.
			return Report{}, ErrMalformedResponse
		}
		label := strings.ToLower(strings.TrimSpace(bucket.TokenType))
		if label == "" {
			label = "quota"
		}
		semanticKey := modelID + "\x00" + label
		if _, duplicate := seenBuckets[semanticKey]; duplicate {
			return Report{}, ErrMalformedResponse
		}
		seenBuckets[semanticKey] = struct{}{}
		resetAt, errReset := strictProviderReset(bucket.ResetTime)
		if errReset != nil {
			return Report{}, errReset
		}
		window := Window{Label: label, ResetsAt: resetAt}
		if bucket.RemainingFraction != nil {
			remaining, errRemaining := strictProviderRemainingFraction(*bucket.RemainingFraction)
			if errRemaining != nil {
				return Report{}, errRemaining
			}
			window.RemainingPercent = remaining
		}
		// remainingAmount is intentionally not converted into a percentage: the
		// provider does not pair it with a same-window limit in this response.
		grouped[modelID] = append(grouped[modelID], window)
	}
	models := make(map[string]Snapshot, len(grouped))
	for modelID, windows := range grouped {
		models[modelID] = normalizeSnapshot(windows, false)
	}
	return Report{Models: models}, nil
}
