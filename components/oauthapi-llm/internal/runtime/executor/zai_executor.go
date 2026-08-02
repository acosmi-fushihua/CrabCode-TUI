package executor

import (
	"context"
	"net/http"
	"strings"

	zaiauth "github.com/acosmi/OAuthAPI-LLM/internal/auth/zai"
	"github.com/acosmi/OAuthAPI-LLM/internal/config"
	"github.com/acosmi/OAuthAPI-LLM/internal/runtime/executor/helps"
	cliproxyauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/executor"
)

// ZaiExecutor is a stateless executor for Z.AI (ZCode / GLM) coding plans.
//
// The coding-plan endpoint speaks the Anthropic Messages protocol, so this
// executor reuses ClaudeExecutor verbatim: it only points the base URL at the
// Z.AI endpoint and lets ClaudeExecutor handle request/response translation
// from any source format. The minted "<apiKey>.<secretKey>" credential is
// read from auth metadata by claudeCreds and must be sent via the x-api-key
// header — the endpoint answers Authorization: Bearer with a bot challenge —
// so the auth clone forces the x-api-key scheme.
type ZaiExecutor struct {
	ClaudeExecutor
	cfg *config.Config
}

// NewZaiExecutor creates a new Z.AI executor. Both the embedded
// ClaudeExecutor and the outer struct receive cfg so delegated calls have
// full configuration; the provider key keeps usage attribution and thinking
// capability lookups on the zai catalog instead of claude.
func NewZaiExecutor(cfg *config.Config) *ZaiExecutor {
	return &ZaiExecutor{ClaudeExecutor: ClaudeExecutor{cfg: cfg, providerKey: "zai"}, cfg: cfg}
}

// Identifier returns the executor identifier used to route auths with
// provider "zai".
func (e *ZaiExecutor) Identifier() string { return "zai" }

// cloneAuthWithZaiDefaults returns a shallow copy of auth with a deep-copied
// Attributes map carrying the Z.AI coding-plan base URL and header scheme.
//
// The Auth object is shared across concurrent requests and its Attributes map
// is immutable configuration, so it must never be mutated in place: an
// in-place write races with concurrent readers. A base URL already present on
// the auth (or in metadata) takes precedence so deployments can override the
// endpoint.
func (e *ZaiExecutor) cloneAuthWithZaiDefaults(auth *cliproxyauth.Auth) *cliproxyauth.Auth {
	if auth == nil {
		return nil
	}
	cloned := *auth
	cloned.Attributes = make(map[string]string, len(auth.Attributes)+3)
	for key, value := range auth.Attributes {
		cloned.Attributes[key] = value
	}

	// Point at the Z.AI coding-plan endpoint unless an override is already set.
	if strings.TrimSpace(cloned.Attributes["base_url"]) == "" {
		base := zaiauth.ZAIAPIBaseURL
		if auth.Metadata != nil {
			if value, ok := auth.Metadata["base_url"].(string); ok && strings.TrimSpace(value) != "" {
				base = strings.TrimSpace(value)
			}
		}
		cloned.Attributes["base_url"] = base
	}

	// The coding-plan endpoint authenticates the minted key via the x-api-key
	// header, like the official ZCode client.
	if strings.TrimSpace(cloned.Attributes["anthropic_auth_scheme"]) == "" {
		cloned.Attributes["anthropic_auth_scheme"] = "x-api-key"
	}

	// Z.AI is not Anthropic, so Claude cloaking (system-prompt injection, fake
	// user IDs, sensitive-word obfuscation) must not rewrite GLM prompts.
	// ClaudeExecutor defaults to "auto", so force it off unless an operator
	// explicitly configured a mode on the credential.
	if strings.TrimSpace(cloned.Attributes["cloak_mode"]) == "" {
		cloned.Attributes["cloak_mode"] = "never"
	}

	return &cloned
}

// Execute performs a non-streaming request against the Z.AI coding-plan endpoint.
func (e *ZaiExecutor) Execute(ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (cliproxyexecutor.Response, error) {
	return e.ClaudeExecutor.Execute(ctx, e.cloneAuthWithZaiDefaults(auth), req, opts)
}

// ExecuteStream performs a streaming request against the Z.AI coding-plan endpoint.
func (e *ZaiExecutor) ExecuteStream(ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (*cliproxyexecutor.StreamResult, error) {
	return e.ClaudeExecutor.ExecuteStream(ctx, e.cloneAuthWithZaiDefaults(auth), req, opts)
}

// CountTokens proxies token counting to the Z.AI coding-plan endpoint.
func (e *ZaiExecutor) CountTokens(ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (cliproxyexecutor.Response, error) {
	return e.ClaudeExecutor.CountTokens(ctx, e.cloneAuthWithZaiDefaults(auth), req, opts)
}

// PrepareRequest injects Z.AI credentials into a raw HTTP request. It
// overrides the method promoted from the embedded ClaudeExecutor so requests
// built through the manager helpers also pass through the auth clone —
// applying the Z.AI base URL, the x-api-key scheme, and the no-cloak override
// instead of Claude's default Bearer behavior.
func (e *ZaiExecutor) PrepareRequest(req *http.Request, auth *cliproxyauth.Auth) error {
	return e.ClaudeExecutor.PrepareRequest(req, e.cloneAuthWithZaiDefaults(auth))
}

// HttpRequest injects Z.AI credentials and executes the request. It overrides
// the promoted ClaudeExecutor method for the same reason as PrepareRequest.
func (e *ZaiExecutor) HttpRequest(ctx context.Context, auth *cliproxyauth.Auth, req *http.Request) (*http.Response, error) {
	return e.ClaudeExecutor.HttpRequest(ctx, e.cloneAuthWithZaiDefaults(auth), req)
}

// Refresh is a home-aware no-op: the Z.AI coding-plan key is long-lived and
// the OAuth flow does not return a refresh token. Re-login is required if the
// key is revoked.
func (e *ZaiExecutor) Refresh(ctx context.Context, auth *cliproxyauth.Auth) (*cliproxyauth.Auth, error) {
	if refreshed, handled, err := helps.RefreshAuthViaHome(ctx, e.cfg, auth); handled {
		return refreshed, err
	}
	return auth, nil
}
