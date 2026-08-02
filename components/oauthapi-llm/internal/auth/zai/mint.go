package zai

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"

	log "github.com/sirupsen/logrus"
)

const (
	// zaiBizBaseURL hosts the business (organization/project/api-key) API used
	// to provision a standard coding-plan key from the OAuth login.
	zaiBizBaseURL = "https://api.z.ai"
	// mintKeyName is the name of the API key the sidecar provisions. It
	// matches the official ZCode client so a re-login reuses the same key
	// instead of accumulating new ones.
	mintKeyName = "zcode-api-key"
	// defaultOrgNameMarker / defaultProjectNameMarker identify the account's
	// default organization and project by display name. A matching entry is
	// preferred; otherwise the first entry with projects is used.
	defaultOrgNameMarker     = "默认机构"
	defaultProjectNameMarker = "默认项目"
)

// MintAPIKey exchanges the OAuth credentials for a standard coding-plan API
// key — the same provisioning the official ZCode client performs — and
// returns the composite key together with the Anthropic-compatible inference
// base URL. The Z.AI OAuth access token is first exchanged for a business
// token via /api/auth/z/login; the resulting "<apiKey>.<secretKey>" composite
// authenticates against the standard endpoint via the x-api-key header.
func (z *ZaiAuth) MintAPIKey(ctx context.Context, ready *ReadyResult) (apiKey, baseURL string, err error) {
	if ready == nil {
		return "", "", fmt.Errorf("zai: ready result is nil")
	}
	bizToken, err := z.bizLogin(ctx, ready.ZaiAccessToken)
	if err != nil {
		return "", "", err
	}
	key, err := z.mintBizAPIKey(ctx, "Bearer "+bizToken)
	if err != nil {
		return "", "", err
	}
	return key, ZAIAPIBaseURL, nil
}

// bizLogin exchanges the Z.AI OAuth access token for a business access token.
func (z *ZaiAuth) bizLogin(ctx context.Context, accessToken string) (string, error) {
	accessToken = strings.TrimSpace(accessToken)
	if accessToken == "" {
		return "", fmt.Errorf("zai: missing access token for business login")
	}
	body, err := json.Marshal(map[string]string{"token": accessToken})
	if err != nil {
		return "", fmt.Errorf("zai: failed to encode business login request: %w", err)
	}
	data, err := z.bizRequest(ctx, http.MethodPost, zaiBizBaseURL+"/api/auth/z/login", "", body)
	if err != nil {
		return "", fmt.Errorf("zai: business login failed: %w", err)
	}
	var login struct {
		AccessToken string `json:"access_token"`
	}
	if err = json.Unmarshal(data, &login); err != nil {
		return "", fmt.Errorf("zai: failed to parse business login response: %w", err)
	}
	if strings.TrimSpace(login.AccessToken) == "" {
		return "", fmt.Errorf("zai: business login response missing access_token")
	}
	return login.AccessToken, nil
}

// mintBizAPIKey resolves the organization and project, finds or creates the
// coding-plan API key, copies its secret, and returns the composite
// "<apiKey>.<secretKey>" credential.
func (z *ZaiAuth) mintBizAPIKey(ctx context.Context, authorization string) (string, error) {
	// 1. Resolve the organization and project from the customer info.
	data, err := z.bizRequest(ctx, http.MethodGet, zaiBizBaseURL+"/api/biz/customer/getCustomerInfo", authorization, nil)
	if err != nil {
		return "", fmt.Errorf("zai: failed to get customer info: %w", err)
	}
	var customer struct {
		Organizations []struct {
			OrganizationID   string `json:"organizationId"`
			OrganizationName string `json:"organizationName"`
			Projects         []struct {
				ProjectID   string `json:"projectId"`
				ProjectName string `json:"projectName"`
			} `json:"projects"`
		} `json:"organizations"`
	}
	if err = json.Unmarshal(data, &customer); err != nil {
		return "", fmt.Errorf("zai: failed to parse customer info: %w", err)
	}
	if len(customer.Organizations) == 0 {
		return "", fmt.Errorf("zai: no organization on account (no active coding plan?)")
	}
	// Prefer an organization that has projects, favoring the account's default
	// organization among those; keep the first organization otherwise so the
	// check below surfaces a clear "no project" error.
	org := customer.Organizations[0]
	for index := range customer.Organizations {
		candidate := customer.Organizations[index]
		if len(candidate.Projects) == 0 {
			continue
		}
		isDefault := strings.Contains(candidate.OrganizationName, defaultOrgNameMarker)
		if len(org.Projects) == 0 || isDefault {
			org = candidate
			if isDefault {
				break
			}
		}
	}
	if len(org.Projects) == 0 {
		return "", fmt.Errorf("zai: no project in organization")
	}
	project := org.Projects[0]
	for index := range org.Projects {
		if strings.Contains(org.Projects[index].ProjectName, defaultProjectNameMarker) {
			project = org.Projects[index]
			break
		}
	}
	if strings.TrimSpace(org.OrganizationID) == "" || strings.TrimSpace(project.ProjectID) == "" {
		return "", fmt.Errorf("zai: unable to resolve organization/project")
	}

	// 2. Find the existing coding-plan key, or create it.
	keysURL := fmt.Sprintf("%s/api/biz/v1/organization/%s/projects/%s/api_keys", zaiBizBaseURL, url.PathEscape(org.OrganizationID), url.PathEscape(project.ProjectID))
	apiKey := ""
	if listData, errList := z.bizRequest(ctx, http.MethodGet, keysURL, authorization, nil); errList == nil {
		var keys []struct {
			Name   string `json:"name"`
			APIKey string `json:"apiKey"`
		}
		if json.Unmarshal(listData, &keys) == nil {
			for _, key := range keys {
				if key.Name == mintKeyName {
					apiKey = strings.TrimSpace(key.APIKey)
					break
				}
			}
		}
	}
	if apiKey == "" {
		body, errEncode := json.Marshal(map[string]string{"name": mintKeyName})
		if errEncode != nil {
			return "", fmt.Errorf("zai: failed to encode api key request: %w", errEncode)
		}
		createData, errCreate := z.bizRequest(ctx, http.MethodPost, keysURL, authorization, body)
		if errCreate != nil {
			return "", fmt.Errorf("zai: failed to create api key: %w", errCreate)
		}
		var created struct {
			APIKey string `json:"apiKey"`
		}
		if err = json.Unmarshal(createData, &created); err != nil {
			return "", fmt.Errorf("zai: failed to parse created api key: %w", err)
		}
		apiKey = strings.TrimSpace(created.APIKey)
	}
	if apiKey == "" {
		return "", fmt.Errorf("zai: api key response missing apiKey")
	}

	// 3. Copy the secret key. The final credential is "<apiKey>.<secretKey>".
	copyData, err := z.bizRequest(ctx, http.MethodGet, keysURL+"/copy/"+url.PathEscape(apiKey), authorization, nil)
	if err != nil {
		return "", fmt.Errorf("zai: failed to copy api key secret: %w", err)
	}
	var copied struct {
		SecretKey string `json:"secretKey"`
	}
	if err = json.Unmarshal(copyData, &copied); err != nil {
		return "", fmt.Errorf("zai: failed to parse api key secret: %w", err)
	}
	secretKey := strings.TrimSpace(copied.SecretKey)
	if secretKey == "" {
		return "", fmt.Errorf("zai: api key copy response missing secretKey")
	}
	return apiKey + "." + secretKey, nil
}

// bizRequest performs a business API call and unwraps the {code,msg,data}
// envelope. The business API reports success as code 0 or 200; authorization
// is sent verbatim in the Authorization header when non-empty.
func (z *ZaiAuth) bizRequest(ctx context.Context, method, endpoint, authorization string, body []byte) (json.RawMessage, error) {
	var reader io.Reader
	if body != nil {
		reader = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, endpoint, reader)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	if authorization != "" {
		req.Header.Set("Authorization", authorization)
	}
	resp, err := z.httpClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer func() {
		if errClose := resp.Body.Close(); errClose != nil {
			log.Errorf("zai: close business response body error: %v", errClose)
		}
	}()

	raw, err := io.ReadAll(io.LimitReader(resp.Body, maxResponseBytes))
	if err != nil {
		return nil, err
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return nil, fmt.Errorf("HTTP %d: %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	var envelope struct {
		Code json.Number     `json:"code"`
		Msg  string          `json:"msg"`
		Data json.RawMessage `json:"data"`
	}
	if err = json.Unmarshal(raw, &envelope); err != nil {
		return nil, fmt.Errorf("failed to parse response envelope: %w", err)
	}
	if code := envelope.Code.String(); code != "" && code != "0" && code != "200" {
		msg := strings.TrimSpace(envelope.Msg)
		if msg == "" {
			msg = "business error " + code
		}
		return nil, fmt.Errorf("%s", msg)
	}
	return envelope.Data, nil
}
