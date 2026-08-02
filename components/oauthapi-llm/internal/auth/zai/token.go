// Package zai provides authentication and token management functionality for
// Z.AI (ZCode / GLM) coding plans. It handles token storage, serialization,
// and retrieval for maintaining authenticated sessions with the
// Anthropic-compatible coding-plan endpoint.
package zai

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/misc"
)

// ZaiTokenStorage stores Z.AI coding-plan credentials.
//
// The OAuth flow provisions a long-lived coding-plan API key
// ("<apiKey>.<secretKey>") that is sent as the x-api-key credential to the
// Anthropic-compatible endpoint. The flow does not return a refresh token;
// when the key is rejected the user logs in again.
type ZaiTokenStorage struct {
	// AccessToken is the minted coding-plan API key ("apiKey.secretKey").
	AccessToken string `json:"access_token"`
	// ZaiAccessToken is the Z.AI OAuth access token the key was provisioned
	// from. It is retained so the key can be re-provisioned if needed.
	ZaiAccessToken string `json:"zai_access_token,omitempty"`
	// BaseURL is the Anthropic-compatible endpoint that accepts the key.
	BaseURL string `json:"base_url,omitempty"`
	// UserID identifies the authenticated Z.AI account.
	UserID string `json:"user_id,omitempty"`
	// Email is the authenticated account email, when provided.
	Email string `json:"email,omitempty"`
	// Name is the authenticated account display name, when provided.
	Name string `json:"name,omitempty"`
	// LastRefresh is the RFC3339 timestamp of the last credential update.
	LastRefresh string `json:"last_refresh,omitempty"`
	// Type indicates the authentication provider type, always "zai" for this storage.
	Type string `json:"type"`

	// Metadata holds arbitrary key-value pairs injected via hooks.
	// It is not exported to JSON directly to allow flattening during serialization.
	Metadata map[string]any `json:"-"`
}

// SetMetadata allows external callers to inject metadata into the storage before saving.
func (ts *ZaiTokenStorage) SetMetadata(meta map[string]any) {
	ts.Metadata = meta
}

// MarshalTokenJSON serializes credentials without creating plaintext files.
func (ts *ZaiTokenStorage) MarshalTokenJSON() ([]byte, error) {
	if ts == nil {
		return nil, fmt.Errorf("Z.AI token storage is nil")
	}
	ts.Type = "zai"
	data, err := misc.MergeMetadata(ts, ts.Metadata)
	if err != nil {
		return nil, fmt.Errorf("failed to merge metadata: %w", err)
	}
	return json.Marshal(data)
}

// SaveTokenToFile serializes the Z.AI token storage to a JSON file.
func (ts *ZaiTokenStorage) SaveTokenToFile(authFilePath string) error {
	misc.LogSavingCredentials(authFilePath)
	ts.Type = "zai"

	if err := os.MkdirAll(filepath.Dir(authFilePath), 0700); err != nil {
		return fmt.Errorf("failed to create directory: %v", err)
	}

	f, err := os.Create(authFilePath)
	if err != nil {
		return fmt.Errorf("failed to create token file: %w", err)
	}
	defer func() {
		_ = f.Close()
	}()

	// Merge metadata using helper
	data, errMerge := misc.MergeMetadata(ts, ts.Metadata)
	if errMerge != nil {
		return fmt.Errorf("failed to merge metadata: %w", errMerge)
	}

	encoder := json.NewEncoder(f)
	encoder.SetIndent("", "  ")
	if err = encoder.Encode(data); err != nil {
		return fmt.Errorf("failed to write token to file: %w", err)
	}
	return nil
}

// CredentialFileName returns the filename used for Z.AI credentials,
// namespaced by a stable account identifier when available.
func CredentialFileName(userID, email string) string {
	if segment := sanitizeFileSegment(email); segment != "" {
		return fmt.Sprintf("zai-%s.json", segment)
	}
	if segment := sanitizeFileSegment(userID); segment != "" {
		return fmt.Sprintf("zai-%s.json", segment)
	}
	return fmt.Sprintf("zai-%d.json", time.Now().UnixMilli())
}

// sanitizeFileSegment reduces an account identifier to filesystem-safe runes.
func sanitizeFileSegment(value string) string {
	value = strings.TrimSpace(value)
	if value == "" {
		return ""
	}
	var builder strings.Builder
	for _, r := range value {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9':
			builder.WriteRune(r)
		case r == '@' || r == '.' || r == '_' || r == '-':
			builder.WriteRune(r)
		default:
			builder.WriteRune('-')
		}
	}
	return strings.Trim(builder.String(), "-")
}
