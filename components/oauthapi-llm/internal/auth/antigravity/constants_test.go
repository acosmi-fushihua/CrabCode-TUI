package antigravity

import (
	"strings"
	"testing"
)

func TestOAuthClientSecretReadsTrimmedRuntimeConfiguration(t *testing.T) {
	t.Setenv(ClientSecretEnv, "  injected-test-value  ")
	secret, err := OAuthClientSecret()
	if err != nil {
		t.Fatalf("OAuthClientSecret returned an error: %v", err)
	}
	if secret != "injected-test-value" {
		t.Fatalf("OAuthClientSecret = %q, want trimmed runtime value", secret)
	}
}

func TestOAuthClientSecretFailsBeforeNetworkWhenMissing(t *testing.T) {
	t.Setenv(ClientSecretEnv, "   ")
	secret, err := OAuthClientSecret()
	if err == nil {
		t.Fatal("OAuthClientSecret unexpectedly accepted missing configuration")
	}
	if secret != "" {
		t.Fatalf("OAuthClientSecret returned unexpected secret %q", secret)
	}
	if !strings.Contains(err.Error(), ClientSecretEnv) {
		t.Fatalf("error %q does not name required environment variable", err)
	}
}
