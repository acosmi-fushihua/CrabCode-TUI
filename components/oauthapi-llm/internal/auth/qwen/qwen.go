// Package qwen provides authentication and token management for Qwen coding
// accounts. It handles the RFC 8628 OAuth2 Device Authorization Grant flow
// (with PKCE S256) for secure authentication.
package qwen

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/auth/autherrors"
	"github.com/acosmi/OAuthAPI-LLM/internal/config"
	"github.com/acosmi/OAuthAPI-LLM/internal/util"
	"github.com/google/uuid"
	log "github.com/sirupsen/logrus"
	"golang.org/x/sync/singleflight"
)

const (
	// qwenClientID is qwen-code's public OAuth client ID.
	qwenClientID = "f0304373b74a44d2b584a3fb70ca9e56"
	// qwenOAuthHost is the OAuth server endpoint.
	qwenOAuthHost = "https://chat.qwen.ai"
	// qwenDeviceCodeURL is the endpoint for requesting device codes.
	qwenDeviceCodeURL = qwenOAuthHost + "/api/v1/oauth2/device/code"
	// qwenTokenURL is the endpoint for polling device tokens and refreshing.
	qwenTokenURL = qwenOAuthHost + "/api/v1/oauth2/token"
	// qwenScope is the OAuth scope requested during device authorization.
	qwenScope = "openid profile email model.completion"
	// qwenDeviceGrantType is the RFC 8628 device code grant type.
	qwenDeviceGrantType = "urn:ietf:params:oauth:grant-type:device_code"
	// DefaultAPIBaseURL is the OpenAI-compatible inference base used when the
	// token response does not carry a resource_url. It matches qwen-code's
	// DEFAULT_DASHSCOPE_BASE_URL and already ends with the /v1 suffix.
	DefaultAPIBaseURL = "https://dashscope.aliyuncs.com/compatible-mode/v1"
	// defaultPollInterval matches qwen-code's 2s device token poll cadence.
	defaultPollInterval = 2 * time.Second
	// maxPollInterval caps slow_down driven poll interval growth.
	maxPollInterval = 10 * time.Second
	// maxPollDuration is the maximum time to wait for user authorization.
	maxPollDuration = 15 * time.Minute
	// refreshThresholdSeconds is when to refresh the token before expiry.
	refreshThresholdSeconds = 300
)

var qwenRefreshGroup singleflight.Group

// QwenAuth handles the Qwen device flow authentication.
type QwenAuth struct {
	deviceClient *DeviceFlowClient
	cfg          *config.Config
}

// NewQwenAuth creates a new QwenAuth service instance.
func NewQwenAuth(cfg *config.Config) *QwenAuth {
	return &QwenAuth{
		deviceClient: NewDeviceFlowClient(cfg),
		cfg:          cfg,
	}
}

// StartDeviceFlow initiates the device flow authentication.
func (q *QwenAuth) StartDeviceFlow(ctx context.Context) (*DeviceCodeResponse, error) {
	return q.deviceClient.RequestDeviceCode(ctx)
}

// WaitForAuthorization polls for user authorization and returns the token data.
func (q *QwenAuth) WaitForAuthorization(ctx context.Context, deviceCode *DeviceCodeResponse) (*QwenTokenData, error) {
	return q.deviceClient.PollForToken(ctx, deviceCode)
}

// CreateTokenStorage creates a new QwenTokenStorage from raw token data.
func (q *QwenAuth) CreateTokenStorage(tokenData *QwenTokenData) *QwenTokenStorage {
	expired := ""
	if tokenData.ExpiresAt > 0 {
		expired = time.Unix(tokenData.ExpiresAt, 0).UTC().Format(time.RFC3339)
	}
	return &QwenTokenStorage{
		AccessToken:  tokenData.AccessToken,
		RefreshToken: tokenData.RefreshToken,
		TokenType:    tokenData.TokenType,
		Scope:        tokenData.Scope,
		ResourceURL:  tokenData.ResourceURL,
		Expired:      expired,
		Type:         "qwen",
	}
}

// APIBaseURLFromResourceURL normalizes the dynamic resource_url delivered in
// Qwen token responses into the OpenAI-compatible inference base URL, exactly
// as qwen-code derives its endpoint: prepend https:// when the scheme is
// missing and ensure a single trailing /v1 path suffix. Empty input falls back
// to DefaultAPIBaseURL.
func APIBaseURLFromResourceURL(resourceURL string) string {
	base := strings.TrimSpace(resourceURL)
	if base == "" {
		return DefaultAPIBaseURL
	}
	base = strings.TrimRight(base, "/")
	if base == "" {
		return DefaultAPIBaseURL
	}
	lower := strings.ToLower(base)
	if !strings.HasPrefix(lower, "http://") && !strings.HasPrefix(lower, "https://") {
		base = "https://" + base
	}
	if !strings.HasSuffix(base, "/v1") {
		base += "/v1"
	}
	return base
}

// DeviceFlowClient handles the OAuth2 device flow for Qwen.
type DeviceFlowClient struct {
	httpClient *http.Client
	cfg        *config.Config
}

// NewDeviceFlowClient creates a new device flow client.
func NewDeviceFlowClient(cfg *config.Config) *DeviceFlowClient {
	return NewDeviceFlowClientWithProxyURL(cfg, "")
}

// NewDeviceFlowClientWithProxyURL creates a new device flow client with a
// proxy override. proxyURL takes precedence over cfg.ProxyURL when non-empty.
func NewDeviceFlowClientWithProxyURL(cfg *config.Config, proxyURL string) *DeviceFlowClient {
	client := &http.Client{Timeout: 30 * time.Second}
	effectiveProxyURL := strings.TrimSpace(proxyURL)
	var sdkCfg config.SDKConfig
	if cfg != nil {
		sdkCfg = cfg.SDKConfig
		if effectiveProxyURL == "" {
			effectiveProxyURL = strings.TrimSpace(cfg.ProxyURL)
		}
	}
	sdkCfg.ProxyURL = effectiveProxyURL
	client = util.SetProxy(&sdkCfg, client)

	return &DeviceFlowClient{
		httpClient: client,
		cfg:        cfg,
	}
}

// qwenOAuthError mirrors the standard OAuth error envelope returned by Qwen.
type qwenOAuthError struct {
	Error            string `json:"error"`
	ErrorDescription string `json:"error_description"`
}

// RequestDeviceCode initiates the device flow by requesting a device code
// from Qwen. The request carries the PKCE S256 challenge; the paired verifier
// is bound to the returned response for the token poll.
func (c *DeviceFlowClient) RequestDeviceCode(ctx context.Context) (*DeviceCodeResponse, error) {
	pkce, err := GeneratePKCECodes()
	if err != nil {
		return nil, fmt.Errorf("qwen: failed to generate PKCE codes: %w", err)
	}

	data := url.Values{}
	data.Set("client_id", qwenClientID)
	data.Set("scope", qwenScope)
	data.Set("code_challenge", pkce.CodeChallenge)
	data.Set("code_challenge_method", "S256")

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, qwenDeviceCodeURL, strings.NewReader(data.Encode()))
	if err != nil {
		return nil, fmt.Errorf("qwen: failed to create device code request: %w", err)
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("Accept", "application/json")
	req.Header.Set("x-request-id", uuid.New().String())

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("qwen: device code request failed: %w", err)
	}
	defer func() {
		if errClose := resp.Body.Close(); errClose != nil {
			log.Errorf("qwen device code: close body error: %v", errClose)
		}
	}()

	bodyBytes, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("qwen: failed to read device code response: %w", err)
	}

	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("qwen: device code request failed with status %d: %s", resp.StatusCode, string(bodyBytes))
	}

	var deviceCode DeviceCodeResponse
	if err = json.Unmarshal(bodyBytes, &deviceCode); err != nil {
		return nil, fmt.Errorf("qwen: failed to parse device code response: %w", err)
	}
	if strings.TrimSpace(deviceCode.DeviceCode) == "" {
		var oauthErr qwenOAuthError
		if json.Unmarshal(bodyBytes, &oauthErr) == nil && oauthErr.Error != "" {
			return nil, fmt.Errorf("qwen: device authorization failed: %s - %s", oauthErr.Error, oauthErr.ErrorDescription)
		}
		return nil, fmt.Errorf("qwen: device authorization response missing device_code")
	}
	deviceCode.CodeVerifier = pkce.CodeVerifier
	return &deviceCode, nil
}

// PollForToken polls the token endpoint until the user authorizes or the
// device code expires. The interval follows qwen-code: 2 seconds by default,
// grown by half (capped at 10 seconds) after a slow_down response and reset
// on any other pending response.
func (c *DeviceFlowClient) PollForToken(ctx context.Context, deviceCode *DeviceCodeResponse) (*QwenTokenData, error) {
	if deviceCode == nil {
		return nil, fmt.Errorf("qwen: device code is nil")
	}
	if strings.TrimSpace(deviceCode.CodeVerifier) == "" {
		return nil, fmt.Errorf("qwen: device code is missing its PKCE verifier")
	}

	interval := defaultPollInterval

	deadline := time.Now().Add(maxPollDuration)
	if deviceCode.ExpiresIn > 0 {
		codeDeadline := time.Now().Add(time.Duration(deviceCode.ExpiresIn) * time.Second)
		if codeDeadline.Before(deadline) {
			deadline = codeDeadline
		}
	}

	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return nil, fmt.Errorf("qwen: context cancelled: %w", ctx.Err())
		case <-ticker.C:
			if time.Now().After(deadline) {
				return nil, autherrors.Classify(autherrors.ErrAuthorizationTimeout, fmt.Errorf("qwen: device code expired"))
			}

			token, slowDown, pollErr, shouldContinue := c.exchangeDeviceCode(ctx, deviceCode.DeviceCode, deviceCode.CodeVerifier)
			if token != nil {
				return token, nil
			}
			if !shouldContinue {
				return nil, pollErr
			}
			if slowDown {
				interval = min(time.Duration(float64(interval)*1.5), maxPollInterval)
			} else {
				interval = defaultPollInterval
			}
			ticker.Reset(interval)
			// Continue polling.
		}
	}
}

// exchangeDeviceCode attempts one device code exchange at the token endpoint.
// Returns (token, slowDown, error, shouldContinue). RFC 8628 pending states
// are HTTP 400 + authorization_pending and HTTP 429 + slow_down; every other
// error is terminal.
func (c *DeviceFlowClient) exchangeDeviceCode(ctx context.Context, deviceCode, codeVerifier string) (*QwenTokenData, bool, error, bool) {
	data := url.Values{}
	data.Set("grant_type", qwenDeviceGrantType)
	data.Set("client_id", qwenClientID)
	data.Set("device_code", deviceCode)
	data.Set("code_verifier", codeVerifier)

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, qwenTokenURL, strings.NewReader(data.Encode()))
	if err != nil {
		return nil, false, fmt.Errorf("qwen: failed to create token request: %w", err), false
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("Accept", "application/json")

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return nil, false, autherrors.Classify(autherrors.ErrUpstreamUnavailable, fmt.Errorf("qwen: token request failed: %w", err)), false
	}
	defer func() {
		if errClose := resp.Body.Close(); errClose != nil {
			log.Errorf("qwen token exchange: close body error: %v", errClose)
		}
	}()

	bodyBytes, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, false, autherrors.Classify(autherrors.ErrUpstreamUnavailable, fmt.Errorf("qwen: failed to read token response: %w", err)), false
	}

	if resp.StatusCode != http.StatusOK {
		var oauthErr qwenOAuthError
		if json.Unmarshal(bodyBytes, &oauthErr) != nil || oauthErr.Error == "" {
			return nil, false, autherrors.Classify(autherrors.ErrUpstreamUnavailable, fmt.Errorf("qwen: token poll failed with status %d: %s", resp.StatusCode, string(bodyBytes))), false
		}
		if resp.StatusCode == http.StatusBadRequest && oauthErr.Error == "authorization_pending" {
			return nil, false, nil, true
		}
		if resp.StatusCode == http.StatusTooManyRequests && oauthErr.Error == "slow_down" {
			return nil, true, nil, true
		}
		switch oauthErr.Error {
		case "expired_token":
			return nil, false, autherrors.Classify(autherrors.ErrAuthorizationTimeout, fmt.Errorf("qwen: device code expired")), false
		case "access_denied":
			return nil, false, autherrors.Classify(autherrors.ErrAuthorizationDenied, fmt.Errorf("qwen: access denied by user")), false
		default:
			return nil, false, fmt.Errorf("qwen: OAuth error: %s - %s", oauthErr.Error, oauthErr.ErrorDescription), false
		}
	}

	var tokenResp struct {
		AccessToken  string  `json:"access_token"`
		RefreshToken string  `json:"refresh_token"`
		TokenType    string  `json:"token_type"`
		ExpiresIn    float64 `json:"expires_in"`
		Scope        string  `json:"scope"`
		ResourceURL  string  `json:"resource_url"`
	}
	if err = json.Unmarshal(bodyBytes, &tokenResp); err != nil {
		return nil, false, autherrors.Classify(autherrors.ErrUpstreamUnavailable, fmt.Errorf("qwen: failed to parse token response: %w", err)), false
	}
	if tokenResp.AccessToken == "" {
		return nil, false, autherrors.Classify(autherrors.ErrUpstreamUnavailable, fmt.Errorf("qwen: empty access token in response")), false
	}

	var expiresAt int64
	if tokenResp.ExpiresIn > 0 {
		expiresAt = time.Now().Unix() + int64(tokenResp.ExpiresIn)
	}

	return &QwenTokenData{
		AccessToken:  tokenResp.AccessToken,
		RefreshToken: tokenResp.RefreshToken,
		TokenType:    tokenResp.TokenType,
		ExpiresAt:    expiresAt,
		Scope:        tokenResp.Scope,
		ResourceURL:  tokenResp.ResourceURL,
	}, false, nil, false
}

// RefreshToken exchanges a refresh token for a new access token. Concurrent
// refreshes of the same refresh token share one upstream call.
func (c *DeviceFlowClient) RefreshToken(ctx context.Context, refreshToken string) (*QwenTokenData, error) {
	if strings.TrimSpace(refreshToken) == "" {
		return nil, fmt.Errorf("qwen: refresh token is required")
	}
	if ctx == nil {
		ctx = context.Background()
	}
	refreshToken = strings.TrimSpace(refreshToken)

	result, err, _ := qwenRefreshGroup.Do(refreshToken, func() (interface{}, error) {
		return c.refreshTokenSingleFlight(context.WithoutCancel(ctx), refreshToken)
	})
	if err != nil {
		return nil, err
	}
	tokenData, ok := result.(*QwenTokenData)
	if !ok || tokenData == nil {
		return nil, fmt.Errorf("qwen: refresh token failed: invalid single-flight result")
	}
	return tokenData, nil
}

func (c *DeviceFlowClient) refreshTokenSingleFlight(ctx context.Context, refreshToken string) (*QwenTokenData, error) {
	data := url.Values{}
	data.Set("grant_type", "refresh_token")
	data.Set("refresh_token", refreshToken)
	data.Set("client_id", qwenClientID)

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, qwenTokenURL, strings.NewReader(data.Encode()))
	if err != nil {
		return nil, fmt.Errorf("qwen: failed to create refresh request: %w", err)
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("Accept", "application/json")

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("qwen: refresh request failed: %w", err)
	}
	defer func() {
		if errClose := resp.Body.Close(); errClose != nil {
			log.Errorf("qwen refresh token: close body error: %v", errClose)
		}
	}()

	bodyBytes, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, fmt.Errorf("qwen: failed to read refresh response: %w", err)
	}

	// Qwen answers an expired or revoked refresh token with 400/401; the
	// caller must fall back to a fresh device authorization.
	if resp.StatusCode == http.StatusBadRequest || resp.StatusCode == http.StatusUnauthorized {
		return nil, fmt.Errorf("qwen: refresh token rejected (status %d), re-authentication required", resp.StatusCode)
	}
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("qwen: refresh failed with status %d: %s", resp.StatusCode, string(bodyBytes))
	}

	var tokenResp struct {
		AccessToken  string  `json:"access_token"`
		RefreshToken string  `json:"refresh_token"`
		TokenType    string  `json:"token_type"`
		ExpiresIn    float64 `json:"expires_in"`
		Scope        string  `json:"scope"`
		ResourceURL  string  `json:"resource_url"`
	}
	if err = json.Unmarshal(bodyBytes, &tokenResp); err != nil {
		return nil, fmt.Errorf("qwen: failed to parse refresh response: %w", err)
	}
	if tokenResp.AccessToken == "" {
		return nil, fmt.Errorf("qwen: empty access token in refresh response")
	}

	var expiresAt int64
	if tokenResp.ExpiresIn > 0 {
		expiresAt = time.Now().Unix() + int64(tokenResp.ExpiresIn)
	}

	return &QwenTokenData{
		AccessToken:  tokenResp.AccessToken,
		RefreshToken: tokenResp.RefreshToken,
		TokenType:    tokenResp.TokenType,
		ExpiresAt:    expiresAt,
		Scope:        tokenResp.Scope,
		ResourceURL:  tokenResp.ResourceURL,
	}, nil
}
