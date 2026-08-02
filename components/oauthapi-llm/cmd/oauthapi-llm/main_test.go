package main

import (
	"testing"

	"github.com/acosmi/OAuthAPI-LLM/internal/config"
)

func TestShouldEnableExampleAPIKeySafeMode(t *testing.T) {
	cfgWithExampleKey := &config.Config{
		SDKConfig: config.SDKConfig{
			APIKeys: []string{"real-key", " your-api-key-1 "},
		},
	}
	cfgWithRealKey := &config.Config{
		SDKConfig: config.SDKConfig{
			APIKeys: []string{"real-key"},
		},
	}

	tests := []struct {
		name               string
		cfg                *config.Config
		commandMode        bool
		tuiMode            bool
		standalone         bool
		cloudConfigMissing bool
		homeMode           bool
		want               bool
	}{
		{
			name: "normal server with example key",
			cfg:  cfgWithExampleKey,
			want: true,
		},
		{
			name:       "standalone tui with example key",
			cfg:        cfgWithExampleKey,
			tuiMode:    true,
			standalone: true,
			want:       true,
		},
		{
			name:        "pure tui client is not blocked",
			cfg:         cfgWithExampleKey,
			tuiMode:     true,
			standalone:  false,
			commandMode: false,
			want:        false,
		},
		{
			name:        "one-shot command is not blocked",
			cfg:         cfgWithExampleKey,
			commandMode: true,
			want:        false,
		},
		{
			name:     "home mode is not blocked",
			cfg:      cfgWithExampleKey,
			homeMode: true,
			want:     false,
		},
		{
			name:               "cloud standby without config is not blocked",
			cfg:                cfgWithExampleKey,
			cloudConfigMissing: true,
			want:               false,
		},
		{
			name: "normal server with real key",
			cfg:  cfgWithRealKey,
			want: false,
		},
		{
			name: "nil config",
			cfg:  nil,
			want: false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := shouldEnableExampleAPIKeySafeMode(tt.cfg, tt.commandMode, tt.tuiMode, tt.standalone, tt.cloudConfigMissing, tt.homeMode)
			if got != tt.want {
				t.Fatalf("shouldEnableExampleAPIKeySafeMode() = %t, want %t", got, tt.want)
			}
		})
	}
}

func TestForbiddenAccountBridgeEnvironmentRejectsAlternateControlPlanes(t *testing.T) {
	for _, key := range []string{
		"HOME_JWT",
		"MANAGEMENT_PASSWORD",
		"GITSTORE_GIT_TOKEN",
		"OBJECTSTORE_ENDPOINT",
		"HTTP_PROXY",
		"SSL_CERT_FILE",
	} {
		t.Run(key, func(t *testing.T) {
			values := map[string]string{key: "unexpected"}
			if got := forbiddenAccountBridgeEnvironmentWithLookup(func(candidate string) (string, bool) {
				value, ok := values[candidate]
				return value, ok
			}); got != key {
				t.Fatalf("forbiddenAccountBridgeEnvironment() = %q, want %q", got, key)
			}
		})
	}
}
