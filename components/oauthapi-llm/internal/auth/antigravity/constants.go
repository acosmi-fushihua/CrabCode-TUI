// Package antigravity provides OAuth2 authentication functionality for the Antigravity provider.
package antigravity

import (
	"fmt"
	"os"
	"strings"
)

// OAuth client configuration. The public repository intentionally does not
// embed a provider client secret; operators inject it at runtime.
const (
	ClientID        = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
	ClientSecretEnv = "CRABCODE_ANTIGRAVITY_OAUTH_CLIENT_SECRET"
	CallbackPort    = 51121
)

// OAuthClientSecret returns the operator-provided Antigravity OAuth client
// secret. Missing configuration fails explicitly before any token request is
// sent, so the sidecar never degrades into an ambiguous provider error.
func OAuthClientSecret() (string, error) {
	secret := strings.TrimSpace(os.Getenv(ClientSecretEnv))
	if secret == "" {
		return "", fmt.Errorf("antigravity OAuth client secret is not configured; set %s", ClientSecretEnv)
	}
	return secret, nil
}

// Scopes defines the OAuth scopes required for Antigravity authentication
var Scopes = []string{
	"https://www.googleapis.com/auth/cloud-platform",
	"https://www.googleapis.com/auth/userinfo.email",
	"https://www.googleapis.com/auth/userinfo.profile",
	"https://www.googleapis.com/auth/cclog",
	"https://www.googleapis.com/auth/experimentsandconfigs",
}

// OAuth2 endpoints for Google authentication
const (
	TokenEndpoint    = "https://oauth2.googleapis.com/token"
	AuthEndpoint     = "https://accounts.google.com/o/oauth2/v2/auth"
	UserInfoEndpoint = "https://www.googleapis.com/oauth2/v2/userinfo?alt=json"
)

// Antigravity API configuration
const (
	APIEndpoint      = "https://cloudcode-pa.googleapis.com"
	DailyAPIEndpoint = "https://daily-cloudcode-pa.googleapis.com"
	APIVersion       = "v1internal"
)
