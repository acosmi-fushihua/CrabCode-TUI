// Package zai provides authentication and token management for Z.AI (ZCode /
// GLM) coding plans. It handles the ZCode CLI OAuth flow — a poll-based
// authorization grant with the same UX as an RFC 8628 device flow — and the
// follow-up provisioning of a standard coding-plan API key that authenticates
// against the Anthropic-compatible inference endpoint.
package zai

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/auth/autherrors"
	"github.com/acosmi/OAuthAPI-LLM/internal/config"
	"github.com/acosmi/OAuthAPI-LLM/internal/util"
	log "github.com/sirupsen/logrus"
)

const (
	// zaiOAuthBaseURL is the ZCode CLI OAuth base endpoint; the init and poll
	// endpoints are derived from it.
	zaiOAuthBaseURL = "https://zcode.z.ai/api/v1"
	// zaiProvider is the identity provider selector sent with the init request.
	// Only the international Z.AI provider is supported by this package.
	zaiProvider = "zai"
	// ZAIAPIBaseURL is the standard Anthropic-compatible coding-plan inference
	// endpoint that accepts the minted API key via the x-api-key header.
	ZAIAPIBaseURL = "https://api.z.ai/api/anthropic"
	// defaultPollInterval is used when the server does not advertise an interval.
	defaultPollInterval = 2 * time.Second
	// maxPollDuration bounds how long we wait for the user to authorize.
	maxPollDuration = 10 * time.Minute
	// pollTokenBytes is the size of the client-generated poll token.
	pollTokenBytes = 32
	// maxConsecutivePollErrors tolerates transient poll failures (network
	// blips, 5xx) during the multi-minute authorization window before the
	// login is aborted.
	maxConsecutivePollErrors = 5
	// maxResponseBytes bounds every OAuth and business API response body.
	maxResponseBytes = 1 << 20
)

// ZaiAuth handles the ZCode CLI OAuth flow for the international Z.AI
// identity provider.
type ZaiAuth struct {
	httpClient *http.Client
	cfg        *config.Config
}

// NewZaiAuth creates a new ZaiAuth service instance.
func NewZaiAuth(cfg *config.Config) *ZaiAuth {
	return NewZaiAuthWithProxyURL(cfg, "")
}

// NewZaiAuthWithProxyURL creates a new ZaiAuth with a proxy override.
// proxyURL takes precedence over cfg.ProxyURL when non-empty.
func NewZaiAuthWithProxyURL(cfg *config.Config, proxyURL string) *ZaiAuth {
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

	return &ZaiAuth{
		httpClient: client,
		cfg:        cfg,
	}
}

// InitResponse is the parsed payload of the OAuth init call.
type InitResponse struct {
	// FlowID identifies the pending authorization for the poll endpoint.
	FlowID string `json:"flow_id"`
	// PollToken is the bearer credential for the poll endpoint. The server
	// response is authoritative; the client-generated token is the fallback.
	PollToken string `json:"poll_token"`
	// AuthorizeURL is the browser URL the user must visit to authorize.
	AuthorizeURL string `json:"authorize_url"`
	// ExpiresAt is the Unix timestamp when the pending flow expires.
	ExpiresAt int64 `json:"expires_at"`
	// PollIntervalSec is the server-advertised poll interval in seconds.
	PollIntervalSec int `json:"poll_interval_sec"`
}

// ReadyResult holds the credentials returned when authorization completes.
type ReadyResult struct {
	// Token is the coding-plan token minted by the OAuth flow.
	Token string
	// ZaiAccessToken is the Z.AI account access token returned alongside the
	// token; the business API key provisioning is authorized with it.
	ZaiAccessToken string
	// UserID identifies the authenticated Z.AI account.
	UserID string
	// Email is the authenticated account email, when provided.
	Email string
	// Name is the authenticated account display name, when provided.
	Name string
}

// zaiEnvelope is the common {code,msg,data} wrapper used by the ZCode OAuth
// API. A code of 0 indicates success.
type zaiEnvelope struct {
	Code int             `json:"code"`
	Msg  string          `json:"msg"`
	Data json.RawMessage `json:"data"`
}

// StartFlow initiates the OAuth flow and returns the authorization URL plus
// the poll credentials used to wait for completion.
func (z *ZaiAuth) StartFlow(ctx context.Context) (*InitResponse, error) {
	pollToken, err := newPollToken()
	if err != nil {
		return nil, fmt.Errorf("zai: failed to generate poll token: %w", err)
	}

	body, err := json.Marshal(map[string]string{"provider": zaiProvider})
	if err != nil {
		return nil, fmt.Errorf("zai: failed to encode init request: %w", err)
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, zaiOAuthBaseURL+"/oauth/cli/init", bytes.NewReader(body))
	if err != nil {
		return nil, fmt.Errorf("zai: failed to create init request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	req.Header.Set("Authorization", "Bearer "+pollToken)

	data, err := z.doEnvelope(req)
	if err != nil {
		return nil, err
	}
	var init InitResponse
	if err = json.Unmarshal(data, &init); err != nil {
		return nil, fmt.Errorf("zai: failed to parse init response: %w", err)
	}
	if strings.TrimSpace(init.FlowID) == "" || strings.TrimSpace(init.AuthorizeURL) == "" {
		return nil, fmt.Errorf("zai: init response missing flow_id or authorize_url")
	}
	// The server-issued poll token is authoritative; fall back to the
	// client-generated one only when the server omitted it.
	if strings.TrimSpace(init.PollToken) == "" {
		init.PollToken = pollToken
	}
	return &init, nil
}

// WaitForAuthorization polls until the user authorizes the request or the
// flow expires, then returns the OAuth credentials.
func (z *ZaiAuth) WaitForAuthorization(ctx context.Context, init *InitResponse) (*ReadyResult, error) {
	if init == nil {
		return nil, fmt.Errorf("zai: init response is nil")
	}

	interval := time.Duration(init.PollIntervalSec) * time.Second
	if interval < defaultPollInterval {
		interval = defaultPollInterval
	}

	deadline := time.Now().Add(maxPollDuration)
	if init.ExpiresAt > 0 {
		if flowDeadline := time.Unix(init.ExpiresAt, 0); flowDeadline.Before(deadline) {
			deadline = flowDeadline
		}
	}

	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	consecutiveErrors := 0
	for {
		select {
		case <-ctx.Done():
			return nil, fmt.Errorf("zai: context cancelled: %w", ctx.Err())
		case <-ticker.C:
			if time.Now().After(deadline) {
				return nil, autherrors.Classify(autherrors.ErrAuthorizationTimeout, fmt.Errorf("zai: authorization timed out"))
			}
			result, done, terminal, err := z.poll(ctx, init)
			if err != nil {
				if terminal {
					return nil, err
				}
				// The authorization window spans minutes, so transient
				// network or upstream errors are retried instead of failing
				// the whole login on the first hiccup.
				consecutiveErrors++
				if consecutiveErrors >= maxConsecutivePollErrors {
					return nil, autherrors.Classify(autherrors.ErrUpstreamUnavailable, fmt.Errorf("zai: polling failed after %d consecutive errors: %w", consecutiveErrors, err))
				}
				log.Warnf("zai: transient polling error (%d/%d), will retry: %v", consecutiveErrors, maxConsecutivePollErrors, err)
				continue
			}
			consecutiveErrors = 0
			if done {
				return result, nil
			}
			// Continue polling.
		}
	}
}

// poll performs a single poll request. It returns (result, done, terminal,
// error) where terminal reports whether the error is definitive (denied
// authorization or a protocol violation) and must not be retried.
func (z *ZaiAuth) poll(ctx context.Context, init *InitResponse) (*ReadyResult, bool, bool, error) {
	pollURL := fmt.Sprintf("%s/oauth/cli/poll/%s", zaiOAuthBaseURL, init.FlowID)
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, pollURL, nil)
	if err != nil {
		return nil, false, true, fmt.Errorf("zai: failed to create poll request: %w", err)
	}
	req.Header.Set("Accept", "application/json")
	req.Header.Set("Authorization", "Bearer "+init.PollToken)

	data, err := z.doEnvelope(req)
	if err != nil {
		// Network, HTTP, and envelope errors are transient: the caller retries.
		return nil, false, false, err
	}

	var poll struct {
		Status string `json:"status"`
		Token  string `json:"token"`
		User   struct {
			UserID string `json:"user_id"`
			Email  string `json:"email"`
			Name   string `json:"name"`
		} `json:"user"`
		ZAI struct {
			AccessToken string `json:"access_token"`
		} `json:"zai"`
	}
	if err = json.Unmarshal(data, &poll); err != nil {
		return nil, false, true, autherrors.Classify(autherrors.ErrUpstreamUnavailable, fmt.Errorf("zai: failed to parse poll response: %w", err))
	}

	switch poll.Status {
	case "pending", "":
		return nil, false, false, nil
	case "failed":
		return nil, false, true, autherrors.Classify(autherrors.ErrAuthorizationDenied, fmt.Errorf("zai: authorization failed or was denied"))
	case "ready":
		if strings.TrimSpace(poll.Token) == "" {
			return nil, false, true, autherrors.Classify(autherrors.ErrUpstreamUnavailable, fmt.Errorf("zai: ready response missing token"))
		}
		return &ReadyResult{
			Token:          poll.Token,
			ZaiAccessToken: poll.ZAI.AccessToken,
			UserID:         poll.User.UserID,
			Email:          poll.User.Email,
			Name:           poll.User.Name,
		}, true, false, nil
	default:
		return nil, false, true, autherrors.Classify(autherrors.ErrUpstreamUnavailable, fmt.Errorf("zai: unexpected poll status %q", poll.Status))
	}
}

// doEnvelope executes the request and unwraps the {code,msg,data} envelope.
func (z *ZaiAuth) doEnvelope(req *http.Request) (json.RawMessage, error) {
	resp, err := z.httpClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("zai: request failed: %w", err)
	}
	defer func() {
		if errClose := resp.Body.Close(); errClose != nil {
			log.Errorf("zai: close response body error: %v", errClose)
		}
	}()

	bodyBytes, err := io.ReadAll(io.LimitReader(resp.Body, maxResponseBytes))
	if err != nil {
		return nil, fmt.Errorf("zai: failed to read response: %w", err)
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return nil, fmt.Errorf("zai: HTTP %d: %s", resp.StatusCode, strings.TrimSpace(string(bodyBytes)))
	}

	var envelope zaiEnvelope
	if err = json.Unmarshal(bodyBytes, &envelope); err != nil {
		return nil, fmt.Errorf("zai: failed to parse response envelope: %w", err)
	}
	if envelope.Code != 0 {
		msg := strings.TrimSpace(envelope.Msg)
		if msg == "" {
			msg = fmt.Sprintf("business error %d", envelope.Code)
		}
		return nil, fmt.Errorf("zai: %s", msg)
	}
	return envelope.Data, nil
}

// CreateTokenStorage builds the on-disk token storage from the OAuth result,
// the minted coding-plan API key, and its Anthropic-compatible inference base
// URL.
func (z *ZaiAuth) CreateTokenStorage(ready *ReadyResult, apiKey, baseURL string) *ZaiTokenStorage {
	storage := &ZaiTokenStorage{
		AccessToken: apiKey,
		BaseURL:     baseURL,
		Type:        "zai",
	}
	if ready != nil {
		storage.ZaiAccessToken = ready.ZaiAccessToken
		storage.UserID = ready.UserID
		storage.Email = ready.Email
		storage.Name = ready.Name
	}
	return storage
}

// newPollToken generates the client-side poll token candidate: 32 random
// bytes, hex encoded.
func newPollToken() (string, error) {
	buf := make([]byte, pollTokenBytes)
	if _, err := rand.Read(buf); err != nil {
		return "", err
	}
	return hex.EncodeToString(buf), nil
}
