package executor

import (
	"context"
	"fmt"
	"net/http"
	"strings"
	"time"

	qwenauth "github.com/acosmi/OAuthAPI-LLM/internal/auth/qwen"
	"github.com/acosmi/OAuthAPI-LLM/internal/config"
	"github.com/acosmi/OAuthAPI-LLM/internal/runtime/executor/helps"
	cliproxyauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/executor"
	log "github.com/sirupsen/logrus"
)

// QwenExecutor is a stateless executor for Qwen coding accounts.
//
// The Qwen coding endpoint is OpenAI-compatible, so this executor reuses
// OpenAICompatExecutor (constructed with provider key "qwen" so attribution
// and thinking capability lookups already resolve against the qwen catalog).
// It only resolves the per-account credentials before delegating: the OAuth
// access token lives in auth metadata — where Refresh keeps it current — and
// the base URL is derived from the dynamic resource_url delivered with the
// token response.
type QwenExecutor struct {
	*OpenAICompatExecutor
	cfg *config.Config
}

// NewQwenExecutor creates a new Qwen executor.
func NewQwenExecutor(cfg *config.Config) *QwenExecutor {
	return &QwenExecutor{OpenAICompatExecutor: NewOpenAICompatExecutor("qwen", cfg), cfg: cfg}
}

// cloneAuthWithQwenCreds returns a shallow copy of auth with a deep-copied
// Attributes map carrying the resolved OAuth credentials in the attribute
// slots OpenAICompatExecutor reads (api_key, base_url).
//
// The Auth object is shared across concurrent requests and its Attributes map
// is immutable configuration, so it must never be mutated in place. Metadata
// wins over any stale attribute copy for the token because Refresh rewrites
// metadata first; an explicit base_url attribute wins over metadata so
// deployments can override the endpoint.
func (e *QwenExecutor) cloneAuthWithQwenCreds(auth *cliproxyauth.Auth) *cliproxyauth.Auth {
	if auth == nil {
		return nil
	}
	cloned := *auth
	cloned.Attributes = make(map[string]string, len(auth.Attributes)+2)
	for key, value := range auth.Attributes {
		cloned.Attributes[key] = value
	}

	token := ""
	resourceURL := ""
	if auth.Metadata != nil {
		if value, ok := auth.Metadata["access_token"].(string); ok {
			token = strings.TrimSpace(value)
		}
		if value, ok := auth.Metadata["resource_url"].(string); ok {
			resourceURL = strings.TrimSpace(value)
		}
	}
	if token != "" {
		cloned.Attributes["api_key"] = token
	}
	if strings.TrimSpace(cloned.Attributes["base_url"]) == "" {
		cloned.Attributes["base_url"] = qwenauth.APIBaseURLFromResourceURL(resourceURL)
	}
	return &cloned
}

// Execute performs a non-streaming request against the Qwen coding endpoint.
func (e *QwenExecutor) Execute(ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (cliproxyexecutor.Response, error) {
	return e.OpenAICompatExecutor.Execute(ctx, e.cloneAuthWithQwenCreds(auth), req, opts)
}

// ExecuteStream performs a streaming request against the Qwen coding endpoint.
func (e *QwenExecutor) ExecuteStream(ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (*cliproxyexecutor.StreamResult, error) {
	return e.OpenAICompatExecutor.ExecuteStream(ctx, e.cloneAuthWithQwenCreds(auth), req, opts)
}

// CountTokens estimates token count for Qwen requests.
func (e *QwenExecutor) CountTokens(ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (cliproxyexecutor.Response, error) {
	return e.OpenAICompatExecutor.CountTokens(ctx, e.cloneAuthWithQwenCreds(auth), req, opts)
}

// PrepareRequest injects Qwen credentials into a raw HTTP request. It
// overrides the promoted OpenAICompatExecutor method so manager-built
// requests also read the OAuth token from metadata.
func (e *QwenExecutor) PrepareRequest(req *http.Request, auth *cliproxyauth.Auth) error {
	return e.OpenAICompatExecutor.PrepareRequest(req, e.cloneAuthWithQwenCreds(auth))
}

// HttpRequest injects Qwen credentials and executes the request. It overrides
// the promoted OpenAICompatExecutor method for the same reason as
// PrepareRequest.
func (e *QwenExecutor) HttpRequest(ctx context.Context, auth *cliproxyauth.Auth, req *http.Request) (*http.Response, error) {
	return e.OpenAICompatExecutor.HttpRequest(ctx, e.cloneAuthWithQwenCreds(auth), req)
}

// Refresh refreshes the Qwen token using the refresh token. The token
// response may also rotate the dynamic resource_url; both metadata and the
// base_url attribute are updated so subsequent requests use the fresh
// endpoint.
func (e *QwenExecutor) Refresh(ctx context.Context, auth *cliproxyauth.Auth) (*cliproxyauth.Auth, error) {
	log.Debugf("qwen executor: refresh called")
	if refreshed, handled, err := helps.RefreshAuthViaHome(ctx, e.cfg, auth); handled {
		return refreshed, err
	}
	if auth == nil {
		return nil, fmt.Errorf("qwen executor: auth is nil")
	}
	var refreshToken string
	if auth.Metadata != nil {
		if value, ok := auth.Metadata["refresh_token"].(string); ok && strings.TrimSpace(value) != "" {
			refreshToken = value
		}
	}
	if strings.TrimSpace(refreshToken) == "" {
		// Nothing to refresh.
		return auth, nil
	}

	client := qwenauth.NewDeviceFlowClientWithProxyURL(e.cfg, auth.ProxyURL)
	tokenData, err := client.RefreshToken(ctx, refreshToken)
	if err != nil {
		return nil, err
	}
	if auth.Metadata == nil {
		auth.Metadata = make(map[string]any)
	}
	auth.Metadata["access_token"] = tokenData.AccessToken
	if tokenData.RefreshToken != "" {
		auth.Metadata["refresh_token"] = tokenData.RefreshToken
	}
	if tokenData.ExpiresAt > 0 {
		auth.Metadata["expired"] = time.Unix(tokenData.ExpiresAt, 0).UTC().Format(time.RFC3339)
	}
	if strings.TrimSpace(tokenData.ResourceURL) != "" {
		auth.Metadata["resource_url"] = tokenData.ResourceURL
	}
	auth.Metadata["type"] = "qwen"
	auth.Metadata["last_refresh"] = time.Now().Format(time.RFC3339)
	// Mirror the dynamic base into attributes (xAI pattern) so attribute-only
	// consumers observe the rotated endpoint as well.
	if resource, ok := auth.Metadata["resource_url"].(string); ok && strings.TrimSpace(resource) != "" {
		if auth.Attributes == nil {
			auth.Attributes = make(map[string]string)
		}
		auth.Attributes["base_url"] = qwenauth.APIBaseURLFromResourceURL(resource)
	}
	return auth, nil
}
