package config

import (
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"gopkg.in/yaml.v3"
)

func validAccountBridgeConfig() *Config {
	cfg := &Config{}
	ApplyAccountBridgeDefaults(cfg)
	cfg.APIKeys = []string{"inference-secret"}
	cfg.RemoteManagement.SecretKey = "management-secret"
	return cfg
}

func TestAccountBridgeDefaultsAreFailClosed(t *testing.T) {
	cfg := &Config{}
	ApplyAccountBridgeDefaults(cfg)
	if cfg.Host != "127.0.0.1" || cfg.Port != 0 {
		t.Fatalf("unexpected listener defaults: %q:%d", cfg.Host, cfg.Port)
	}
	if cfg.AuthDir != "~/.crabcode/account-bridge" {
		t.Fatalf("unexpected auth dir: %q", cfg.AuthDir)
	}
	if cfg.RemoteManagement.AllowRemote || !cfg.RemoteManagement.DisableControlPanel || !cfg.RemoteManagement.DisableAutoUpdatePanel {
		t.Fatal("remote management defaults are not fail-closed")
	}
	if cfg.Plugins.Enabled || cfg.Pprof.Enable || !cfg.WebsocketAuth || cfg.UsageStatisticsEnabled {
		t.Fatal("runtime defaults are not fail-closed")
	}
	if !cfg.DisableClaudeCloakMode || cfg.Codex.IdentityConfuse || cfg.ProxyURL != "direct" {
		t.Fatal("outbound identity/proxy defaults are not fail-closed")
	}
}

func TestValidateAccountBridgeSecurityAcceptsLockedConfig(t *testing.T) {
	if err := ValidateAccountBridgeSecurity(validAccountBridgeConfig()); err != nil {
		t.Fatalf("valid locked config rejected: %v", err)
	}
}

func TestValidateAccountBridgeSecurityAcceptsOnlyPackagedFixedPlugin(t *testing.T) {
	cfg := validAccountBridgeConfig()
	configureFixedAccountBridgePlugin(t, cfg)
	if err := ValidateAccountBridgeSecurity(cfg); err != nil {
		t.Fatalf("valid fixed plugin config rejected: %v", err)
	}
}

func TestValidateAccountBridgeSecurityRejectsFixedPluginAllowlistDrift(t *testing.T) {
	tests := []struct {
		name string
		edit func(*testing.T, *Config)
		want string
	}{
		{
			name: "unexpected plugin id",
			edit: func(t *testing.T, cfg *Config) {
				cfg.Plugins.Configs["other"] = cfg.Plugins.Configs[accountBridgeFixedPluginID]
			},
			want: "only the fixed",
		},
		{
			name: "plugin priority",
			edit: func(t *testing.T, cfg *Config) {
				item := cfg.Plugins.Configs[accountBridgeFixedPluginID]
				item.Priority = 1
				cfg.Plugins.Configs[accountBridgeFixedPluginID] = item
			},
			want: "priority",
		},
		{
			name: "plugin config injection",
			edit: func(t *testing.T, cfg *Config) {
				item := cfg.Plugins.Configs[accountBridgeFixedPluginID]
				item.Raw.Content = append(item.Raw.Content,
					&yaml.Node{Kind: yaml.ScalarNode, Tag: "!!str", Value: "store"},
					&yaml.Node{Kind: yaml.MappingNode, Tag: "!!map"},
				)
				cfg.Plugins.Configs[accountBridgeFixedPluginID] = item
			},
			want: "only enabled",
		},
		{
			name: "relative plugin directory",
			edit: func(t *testing.T, cfg *Config) {
				cfg.Plugins.Dir = "account-bridge/plugins"
			},
			want: "absolute clean path",
		},
		{
			name: "unexpected directory entry",
			edit: func(t *testing.T, cfg *Config) {
				if errWrite := os.WriteFile(filepath.Join(cfg.Plugins.Dir, "other.so"), []byte("other"), 0o600); errWrite != nil {
					t.Fatal(errWrite)
				}
			},
			want: "unexpected entry",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := validAccountBridgeConfig()
			configureFixedAccountBridgePlugin(t, cfg)
			tt.edit(t, cfg)
			err := ValidateAccountBridgeSecurity(cfg)
			if err == nil || !strings.Contains(err.Error(), tt.want) {
				t.Fatalf("got %v, want error containing %q", err, tt.want)
			}
		})
	}
}

func TestValidateAccountBridgeSecurityRejectsBoundaryWidening(t *testing.T) {
	tests := []struct {
		name string
		edit func(*Config)
		want string
	}{
		{"all interfaces", func(c *Config) { c.Host = "" }, "host"},
		{"fixed port", func(c *Config) { c.Port = 8317 }, "dynamically assigned"},
		{"listener tls", func(c *Config) { c.TLS.Enable = true }, "TLS overrides"},
		{"home control plane", func(c *Config) { c.Home.Enabled = true }, "home control plane"},
		{"remote management", func(c *Config) { c.RemoteManagement.AllowRemote = true }, "remote management"},
		{"control panel", func(c *Config) { c.RemoteManagement.DisableControlPanel = false }, "management panel"},
		{"panel source", func(c *Config) { c.RemoteManagement.PanelGitHubRepository = "https://example.test/panel" }, "repository"},
		{"plugins", func(c *Config) { c.Plugins.Enabled = true }, "plugins"},
		{"dormant plugin config", func(c *Config) {
			enabled := false
			c.Plugins.Configs = map[string]PluginInstanceConfig{"sample": {Enabled: &enabled}}
		}, "config must be empty"},
		{"plugin store", func(c *Config) { c.Plugins.StoreSources = []string{"https://example.test/store.json"} }, "plugin stores"},
		{"pprof", func(c *Config) { c.Pprof.Enable = true }, "pprof"},
		{"ws auth", func(c *Config) { c.WebsocketAuth = false }, "websocket"},
		{"telemetry", func(c *Config) { c.UsageStatisticsEnabled = true }, "usage statistics"},
		{"proxy", func(c *Config) { c.ProxyURL = "http://127.0.0.1:8888" }, "global proxy"},
		{"cloak", func(c *Config) { c.DisableClaudeCloakMode = false }, "cloaking"},
		{"identity", func(c *Config) { c.Codex.IdentityConfuse = true }, "identity"},
		{"missing inference key", func(c *Config) { c.APIKeys = nil }, "inference key"},
		{"missing management key", func(c *Config) { c.RemoteManagement.SecretKey = "" }, "management key"},
		{"credential proxy", func(c *Config) { c.CodexKey = []CodexKey{{ProxyURL: "direct"}} }, "must not override"},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := validAccountBridgeConfig()
			tt.edit(cfg)
			err := ValidateAccountBridgeSecurity(cfg)
			if err == nil || !strings.Contains(err.Error(), tt.want) {
				t.Fatalf("got %v, want error containing %q", err, tt.want)
			}
		})
	}
}

func configureFixedAccountBridgePlugin(t *testing.T, cfg *Config) {
	t.Helper()
	pluginDir := filepath.Join(t.TempDir(), "account-bridge", "plugins")
	if errMkdir := os.MkdirAll(pluginDir, 0o700); errMkdir != nil {
		t.Fatal(errMkdir)
	}
	pluginName := "gemini-cli.so"
	files := []string{pluginName, "gemini-cli-LICENSE", "gemini-cli.minisig"}
	switch runtime.GOOS {
	case "darwin":
		pluginName = "gemini-cli.dylib"
		files = []string{pluginName, "gemini-cli-LICENSE"}
	case "windows":
		pluginName = "gemini-cli.dll"
		files = []string{pluginName, "gemini-cli-LICENSE"}
	case "linux":
	default:
		t.Skipf("fixed plugin allowlist test is unsupported on %s", runtime.GOOS)
	}
	for _, name := range files {
		if errWrite := os.WriteFile(filepath.Join(pluginDir, name), []byte(name), 0o600); errWrite != nil {
			t.Fatal(errWrite)
		}
	}
	enabled := true
	cfg.Plugins.Enabled = true
	cfg.Plugins.Dir = pluginDir
	cfg.Plugins.Configs = map[string]PluginInstanceConfig{
		accountBridgeFixedPluginID: {
			Enabled: &enabled,
			Raw: yaml.Node{
				Kind: yaml.MappingNode,
				Tag:  "!!map",
				Content: []*yaml.Node{
					{Kind: yaml.ScalarNode, Tag: "!!str", Value: "enabled"},
					{Kind: yaml.ScalarNode, Tag: "!!bool", Value: "true"},
				},
			},
		},
	}
}
