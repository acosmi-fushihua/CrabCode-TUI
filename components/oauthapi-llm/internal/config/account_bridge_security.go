package config

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strings"

	"gopkg.in/yaml.v3"
)

const accountBridgeFixedPluginID = "gemini-cli"

// ApplyAccountBridgeDefaults sets the fail-closed defaults for the bundled
// CrabCode sidecar. YAML may override them during parsing, but every process
// entry point must call ValidateAccountBridgeSecurity before doing work.
func ApplyAccountBridgeDefaults(cfg *Config) {
	if cfg == nil {
		return
	}
	cfg.Host = DefaultHost
	cfg.Port = 0
	cfg.AuthDir = DefaultAuthDir
	cfg.ProxyURL = "direct"
	cfg.LoggingToFile = false
	cfg.LogsMaxTotalSizeMB = 0
	cfg.ErrorLogsMaxFiles = 10
	cfg.UsageStatisticsEnabled = false
	cfg.RedisUsageQueueRetentionSeconds = 60
	cfg.DisableCooling = false
	cfg.SaveCooldownStatus = false
	cfg.TransientErrorCooldownSeconds = 0
	cfg.DisableImageGeneration = DisableImageGenerationOff
	cfg.WebsocketAuth = true
	cfg.Pprof.Enable = false
	cfg.Pprof.Addr = DefaultPprofAddr
	cfg.RemoteManagement.AllowRemote = false
	cfg.RemoteManagement.DisableControlPanel = true
	cfg.RemoteManagement.DisableAutoUpdatePanel = true
	cfg.RemoteManagement.PanelGitHubRepository = ""
	cfg.Plugins.Enabled = false
	cfg.DisableClaudeCloakMode = true
	cfg.Codex.IdentityConfuse = false
}

// ValidateAccountBridgeSecurity rejects every configuration knob that could
// widen the local companion boundary, download executable/UI code, disguise
// client identity, or route account traffic through a configured proxy.
func ValidateAccountBridgeSecurity(cfg *Config) error {
	if cfg == nil {
		return fmt.Errorf("account bridge config is required")
	}
	if strings.TrimSpace(cfg.Host) != DefaultHost {
		return fmt.Errorf("account bridge host must be %q", DefaultHost)
	}
	if cfg.Port != 0 {
		return fmt.Errorf("account bridge port must be dynamically assigned (0)")
	}
	if cfg.TLS.Enable || strings.TrimSpace(cfg.TLS.Cert) != "" || strings.TrimSpace(cfg.TLS.Key) != "" {
		return fmt.Errorf("account bridge listener TLS overrides must remain disabled")
	}
	if cfg.Home.Enabled {
		return fmt.Errorf("home control plane must remain disabled")
	}
	if cfg.RemoteManagement.AllowRemote {
		return fmt.Errorf("remote management must remain disabled")
	}
	if !cfg.RemoteManagement.DisableControlPanel || !cfg.RemoteManagement.DisableAutoUpdatePanel {
		return fmt.Errorf("management panel and its updater must remain disabled")
	}
	if strings.TrimSpace(cfg.RemoteManagement.PanelGitHubRepository) != "" {
		return fmt.Errorf("management panel repository must remain empty")
	}
	if err := validateAccountBridgePlugins(cfg); err != nil {
		return err
	}
	if cfg.Pprof.Enable {
		return fmt.Errorf("pprof must remain disabled")
	}
	if !cfg.WebsocketAuth {
		return fmt.Errorf("websocket authentication must remain enabled")
	}
	if cfg.UsageStatisticsEnabled {
		return fmt.Errorf("usage statistics must remain disabled")
	}
	if !isDirectProxy(cfg.ProxyURL) {
		return fmt.Errorf("global proxy must use direct mode")
	}
	if !cfg.DisableClaudeCloakMode {
		return fmt.Errorf("Claude request cloaking must remain disabled")
	}
	if cfg.Codex.IdentityConfuse {
		return fmt.Errorf("Codex identity confusion must remain disabled")
	}
	if len(cfg.APIKeys) == 0 || strings.TrimSpace(cfg.APIKeys[0]) == "" {
		return fmt.Errorf("at least one non-empty inference key is required")
	}
	if strings.TrimSpace(cfg.RemoteManagement.SecretKey) == "" {
		return fmt.Errorf("a management key is required")
	}
	if err := validatePerCredentialProxyOverrides(cfg); err != nil {
		return err
	}
	return nil
}

func validateAccountBridgePlugins(cfg *Config) error {
	if len(cfg.Plugins.StoreSources) != 0 || len(cfg.Plugins.StoreAuth) != 0 {
		return fmt.Errorf("plugin stores must remain disabled")
	}
	if !cfg.Plugins.Enabled {
		if len(cfg.Plugins.Configs) != 0 {
			return fmt.Errorf("plugins config must be empty while plugins are disabled")
		}
		return nil
	}

	if len(cfg.Plugins.Configs) != 1 {
		return fmt.Errorf("plugins may enable only the fixed %q plugin", accountBridgeFixedPluginID)
	}
	pluginCfg, okPlugin := cfg.Plugins.Configs[accountBridgeFixedPluginID]
	if !okPlugin {
		return fmt.Errorf("plugins may enable only the fixed %q plugin", accountBridgeFixedPluginID)
	}
	if pluginCfg.Enabled == nil || !*pluginCfg.Enabled {
		return fmt.Errorf("fixed plugin %q must be enabled", accountBridgeFixedPluginID)
	}
	if pluginCfg.Priority != 0 {
		return fmt.Errorf("fixed plugin %q priority must remain zero", accountBridgeFixedPluginID)
	}
	if !exactFixedPluginConfigNode(&pluginCfg.Raw) {
		return fmt.Errorf("fixed plugin %q config may contain only enabled: true", accountBridgeFixedPluginID)
	}

	pluginDir := strings.TrimSpace(cfg.Plugins.Dir)
	if pluginDir == "" || !filepath.IsAbs(pluginDir) || filepath.Clean(pluginDir) != pluginDir {
		return fmt.Errorf("fixed plugin directory must be an absolute clean path")
	}
	if filepath.Base(pluginDir) != "plugins" || filepath.Base(filepath.Dir(pluginDir)) != "account-bridge" {
		return fmt.Errorf("fixed plugin directory must be the packaged account-bridge/plugins directory")
	}
	if err := validateFixedPluginDirectory(pluginDir); err != nil {
		return err
	}
	return nil
}

func exactFixedPluginConfigNode(node *yaml.Node) bool {
	if node == nil || node.Kind != yaml.MappingNode || len(node.Content) != 2 {
		return false
	}
	key, value := node.Content[0], node.Content[1]
	if key == nil || value == nil || key.Kind != yaml.ScalarNode || key.Value != "enabled" {
		return false
	}
	var enabled bool
	return value.Decode(&enabled) == nil && enabled
}

func validateFixedPluginDirectory(pluginDir string) error {
	dirInfo, errDir := os.Lstat(pluginDir)
	if errDir != nil {
		return fmt.Errorf("fixed plugin directory is unavailable: %w", errDir)
	}
	if !dirInfo.IsDir() || dirInfo.Mode()&os.ModeSymlink != 0 {
		return fmt.Errorf("fixed plugin directory must be a real directory")
	}

	pluginName := "gemini-cli.so"
	allowed := map[string]struct{}{
		pluginName:           {},
		"gemini-cli-LICENSE": {},
		"gemini-cli.minisig": {},
	}
	switch runtime.GOOS {
	case "darwin":
		pluginName = "gemini-cli.dylib"
		allowed = map[string]struct{}{pluginName: {}, "gemini-cli-LICENSE": {}}
	case "windows":
		pluginName = "gemini-cli.dll"
		allowed = map[string]struct{}{pluginName: {}, "gemini-cli-LICENSE": {}}
	case "linux":
		// Defaults above are the exact Linux release layout.
	default:
		return fmt.Errorf("fixed plugin is unsupported on %s", runtime.GOOS)
	}

	entries, errRead := os.ReadDir(pluginDir)
	if errRead != nil {
		return fmt.Errorf("read fixed plugin directory: %w", errRead)
	}
	actualNames := make([]string, 0, len(entries))
	for _, entry := range entries {
		if entry == nil {
			continue
		}
		name := entry.Name()
		actualNames = append(actualNames, name)
		if _, okAllowed := allowed[name]; !okAllowed {
			return fmt.Errorf("fixed plugin directory contains unexpected entry %q", name)
		}
		info, errInfo := entry.Info()
		if errInfo != nil {
			return fmt.Errorf("inspect fixed plugin entry %q: %w", name, errInfo)
		}
		if !info.Mode().IsRegular() || info.Mode()&os.ModeSymlink != 0 {
			return fmt.Errorf("fixed plugin entry %q must be a regular non-symlink file", name)
		}
	}
	if len(actualNames) != len(allowed) {
		sort.Strings(actualNames)
		return fmt.Errorf("fixed plugin directory is incomplete: found %v", actualNames)
	}
	return nil
}

func isDirectProxy(value string) bool {
	value = strings.TrimSpace(value)
	return strings.EqualFold(value, "direct") || strings.EqualFold(value, "none")
}

func validatePerCredentialProxyOverrides(cfg *Config) error {
	check := func(kind string, index int, value string) error {
		if strings.TrimSpace(value) != "" {
			return fmt.Errorf("%s credential %d must not override proxy routing", kind, index)
		}
		return nil
	}
	for i := range cfg.GeminiKey {
		if err := check("gemini", i, cfg.GeminiKey[i].ProxyURL); err != nil {
			return err
		}
	}
	for i := range cfg.InteractionsKey {
		if err := check("interactions", i, cfg.InteractionsKey[i].ProxyURL); err != nil {
			return err
		}
	}
	for i := range cfg.ClaudeKey {
		if err := check("claude", i, cfg.ClaudeKey[i].ProxyURL); err != nil {
			return err
		}
		if cfg.ClaudeKey[i].Cloak != nil && !strings.EqualFold(strings.TrimSpace(cfg.ClaudeKey[i].Cloak.Mode), "never") {
			return fmt.Errorf("claude credential %d must not enable request cloaking", i)
		}
	}
	for i := range cfg.CodexKey {
		if err := check("codex", i, cfg.CodexKey[i].ProxyURL); err != nil {
			return err
		}
	}
	for i := range cfg.VertexCompatAPIKey {
		if err := check("vertex", i, cfg.VertexCompatAPIKey[i].ProxyURL); err != nil {
			return err
		}
	}
	for i := range cfg.OpenAICompatibility {
		for j := range cfg.OpenAICompatibility[i].APIKeyEntries {
			if err := check("openai-compatibility", j, cfg.OpenAICompatibility[i].APIKeyEntries[j].ProxyURL); err != nil {
				return err
			}
		}
	}
	return nil
}
