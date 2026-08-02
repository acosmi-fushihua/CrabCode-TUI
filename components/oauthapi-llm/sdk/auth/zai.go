package auth

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/auth/zai"
	"github.com/acosmi/OAuthAPI-LLM/internal/browser"
	"github.com/acosmi/OAuthAPI-LLM/internal/config"
	coreauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
	log "github.com/sirupsen/logrus"
)

// ZaiAuthenticator implements the ZCode CLI OAuth login for Z.AI coding plans.
type ZaiAuthenticator struct{}

// NewZaiAuthenticator constructs a new Z.AI authenticator.
func NewZaiAuthenticator() Authenticator {
	return &ZaiAuthenticator{}
}

// Provider returns the provider key for zai.
func (ZaiAuthenticator) Provider() string {
	return "zai"
}

// RefreshLead returns nil: the minted coding-plan key is long-lived and the
// OAuth flow does not return a refresh token, so there is no scheduled
// refresh.
func (ZaiAuthenticator) RefreshLead() *time.Duration {
	return nil
}

// Login initiates the Z.AI poll-based OAuth authentication and provisions the
// coding-plan API key.
func (a ZaiAuthenticator) Login(ctx context.Context, cfg *config.Config, opts *LoginOptions) (*coreauth.Auth, error) {
	if cfg == nil {
		return nil, fmt.Errorf("cliproxy auth: configuration is required")
	}
	if opts == nil {
		opts = &LoginOptions{}
	}

	authSvc := zai.NewZaiAuth(cfg)

	fmt.Println("Starting Z.AI authentication...")
	initResp, err := authSvc.StartFlow(ctx)
	if err != nil {
		return nil, fmt.Errorf("zai: failed to start OAuth flow: %w", err)
	}

	fmt.Printf("\nTo authenticate, please visit:\n%s\n\n", initResp.AuthorizeURL)

	// Try to open the browser automatically
	if !opts.NoBrowser {
		if browser.IsAvailable() {
			if errOpen := browser.OpenURL(initResp.AuthorizeURL); errOpen != nil {
				log.Warnf("Failed to open browser automatically: %v", errOpen)
			} else {
				fmt.Println("Browser opened automatically.")
			}
		}
	}

	fmt.Println("Waiting for authorization...")

	ready, err := authSvc.WaitForAuthorization(ctx, initResp)
	if err != nil {
		return nil, fmt.Errorf("zai: %w", err)
	}

	fmt.Println("Provisioning coding-plan API key...")
	apiKey, baseURL, err := authSvc.MintAPIKey(ctx, ready)
	if err != nil {
		return nil, fmt.Errorf("zai: %w", err)
	}

	tokenStorage := authSvc.CreateTokenStorage(ready, apiKey, baseURL)

	metadata := map[string]any{
		"type":         "zai",
		"access_token": apiKey,
		"base_url":     baseURL,
		"timestamp":    time.Now().UnixMilli(),
	}
	if ready.ZaiAccessToken != "" {
		metadata["zai_access_token"] = ready.ZaiAccessToken
	}
	if ready.UserID != "" {
		metadata["user_id"] = ready.UserID
	}
	if ready.Email != "" {
		metadata["email"] = ready.Email
	}
	if ready.Name != "" {
		metadata["name"] = ready.Name
	}

	fileName := zai.CredentialFileName(ready.UserID, ready.Email)
	label := strings.TrimSpace(ready.Email)
	if label == "" {
		label = "Z.AI User"
	}

	fmt.Println("\nZ.AI authentication successful!")

	return &coreauth.Auth{
		ID:       fileName,
		Provider: a.Provider(),
		FileName: fileName,
		Label:    label,
		Storage:  tokenStorage,
		Metadata: metadata,
		Attributes: map[string]string{
			"auth_kind": "oauth",
			"base_url":  baseURL,
		},
	}, nil
}
