package pluginhost

import (
	"context"
	"os"
	"strings"
	"testing"

	"github.com/acosmi/OAuthAPI-LLM/sdk/pluginabi"
)

// TestFixedPluginNativeSmoke is opt-in because the fixed Gemini plugin is a
// release artifact, not a source-tree fixture. Release jobs point the test at
// the exact artifact staged by build-account-bridge.ts. Besides dlopen and ABI
// initialization, plugin.register exercises request/response ownership across
// the native boundary.
func TestFixedPluginNativeSmoke(t *testing.T) {
	path := strings.TrimSpace(os.Getenv("ACCOUNT_BRIDGE_FIXED_PLUGIN_SMOKE_PATH"))
	if path == "" {
		t.Skip("ACCOUNT_BRIDGE_FIXED_PLUGIN_SMOKE_PATH is not set")
	}

	host := New()
	client, errOpen := defaultPluginLoader().Open(pluginFile{ID: "gemini-cli", Path: path}, host)
	if errOpen != nil {
		t.Fatalf("open fixed Gemini plugin: %v", errOpen)
	}
	defer client.Shutdown()

	plugin, errRegister := registerRPCPlugin(
		context.Background(),
		host,
		"gemini-cli",
		client,
		pluginabi.MethodPluginRegister,
		nil,
	)
	if errRegister != nil {
		t.Fatalf("register fixed Gemini plugin: %v", errRegister)
	}
	if strings.TrimSpace(plugin.Metadata.Name) == "" || strings.TrimSpace(plugin.Metadata.Version) == "" {
		t.Fatalf("fixed Gemini plugin returned incomplete metadata: %+v", plugin.Metadata)
	}
	if plugin.Capabilities.AuthProvider == nil || plugin.Capabilities.ModelProvider == nil || plugin.Capabilities.Executor == nil {
		t.Fatalf("fixed Gemini plugin returned incomplete required capabilities: %+v", plugin.Capabilities)
	}
}
