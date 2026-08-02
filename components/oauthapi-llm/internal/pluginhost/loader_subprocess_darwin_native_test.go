//go:build darwin

package pluginhost

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

// This opt-in native smoke exercises the nested plugin -> host -> plugin
// callback path over the process-isolation protocol. Release jobs supply the
// exact helper produced by build-account-bridge.ts.
func TestDarwinPluginHelperHostCallbackNativeSmoke(t *testing.T) {
	helper := strings.TrimSpace(os.Getenv("ACCOUNT_BRIDGE_PLUGIN_HELPER_PATH"))
	if helper == "" {
		t.Skip("ACCOUNT_BRIDGE_PLUGIN_HELPER_PATH is not set")
	}
	if info, errStat := os.Stat(helper); errStat != nil || !info.Mode().IsRegular() {
		t.Fatalf("plugin helper is not a regular file: %v", errStat)
	}

	arch := "arm64"
	if runtime.GOARCH == "amd64" {
		arch = "x86_64"
	}
	plugin := filepath.Join(t.TempDir(), "callback-fixture.dylib")
	command := exec.Command(
		"clang",
		"-std=c11",
		"-Wall",
		"-Wextra",
		"-Werror",
		"-pthread",
		"-dynamiclib",
		"-arch",
		arch,
		"-mmacosx-version-min=12.0",
		"-o",
		plugin,
		filepath.Join("testdata", "darwin_callback_plugin.c"),
	)
	if output, errBuild := command.CombinedOutput(); errBuild != nil {
		t.Fatalf("build callback fixture: %v\n%s", errBuild, output)
	}

	client, errOpen := defaultPluginLoader().Open(
		pluginFile{ID: "callback-fixture", Path: plugin},
		New(),
	)
	if errOpen != nil {
		t.Fatalf("open callback fixture: %v", errOpen)
	}
	defer client.Shutdown()

	response, errCall := client.Call(context.Background(), "test.callback", nil)
	if errCall != nil {
		t.Fatalf("call callback fixture: %v", errCall)
	}
	if string(response) != `{"ok":true}` {
		t.Fatalf("callback fixture response = %s", response)
	}

	marker := filepath.Join(t.TempDir(), "async-callback.completed")
	response, errCall = client.Call(
		context.Background(),
		"test.async_callback",
		[]byte(marker),
	)
	if errCall != nil {
		t.Fatalf("start asynchronous callback fixture: %v", errCall)
	}
	if string(response) != `{"started":true}` {
		t.Fatalf("asynchronous callback fixture response = %s", response)
	}
	deadline := time.Now().Add(3 * time.Second)
	for {
		if _, errStat := os.Stat(marker); errStat == nil {
			break
		} else if !os.IsNotExist(errStat) {
			t.Fatalf("stat asynchronous callback marker: %v", errStat)
		}
		if time.Now().After(deadline) {
			t.Fatal("asynchronous host callback was not serviced after plugin call returned")
		}
		time.Sleep(10 * time.Millisecond)
	}
}
