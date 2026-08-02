// Package qwen provides authentication and token management for Qwen coding
// accounts. It implements the RFC 8628 OAuth2 Device Authorization Grant with
// PKCE (RFC 7636, S256) used by the official qwen-code client.
package qwen

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"fmt"
)

// PKCECodes carries one PKCE verifier/challenge pair bound to a single device
// authorization flow.
type PKCECodes struct {
	// CodeVerifier is the high-entropy secret proved during the token poll.
	CodeVerifier string
	// CodeChallenge is the S256 challenge sent with the device code request.
	CodeChallenge string
}

// GeneratePKCECodes generates a new pair of PKCE (Proof Key for Code Exchange)
// codes. It creates a cryptographically random code verifier and its
// corresponding SHA256 code challenge, as specified in RFC 7636. Qwen's device
// authorization endpoint requires the challenge and the token endpoint
// verifies possession of the verifier during polling.
func GeneratePKCECodes() (*PKCECodes, error) {
	codeVerifier, err := generateCodeVerifier()
	if err != nil {
		return nil, fmt.Errorf("failed to generate code verifier: %w", err)
	}
	return &PKCECodes{
		CodeVerifier:  codeVerifier,
		CodeChallenge: generateCodeChallenge(codeVerifier),
	}, nil
}

// generateCodeVerifier creates a cryptographically secure random string to be
// used as the code verifier. 32 random bytes encode to a 43-character
// URL-safe string, matching the official qwen-code client and the RFC 7636
// minimum length.
func generateCodeVerifier() (string, error) {
	bytes := make([]byte, 32)
	if _, err := rand.Read(bytes); err != nil {
		return "", fmt.Errorf("failed to generate random bytes: %w", err)
	}
	return base64.RawURLEncoding.EncodeToString(bytes), nil
}

// generateCodeChallenge derives the S256 code challenge from a code verifier:
// the unpadded URL-safe base64 encoding of the verifier's SHA256 hash.
func generateCodeChallenge(codeVerifier string) string {
	hash := sha256.Sum256([]byte(codeVerifier))
	return base64.RawURLEncoding.EncodeToString(hash[:])
}
