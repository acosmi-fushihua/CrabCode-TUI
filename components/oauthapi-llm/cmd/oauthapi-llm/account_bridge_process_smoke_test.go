package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/accountbridge"
	"github.com/acosmi/OAuthAPI-LLM/internal/fileperm"
)

const nativeProcessSmokeEnvironment = "ACCOUNT_BRIDGE_NATIVE_PROCESS_SMOKE"

type synchronizedBuffer struct {
	mu sync.Mutex
	b  bytes.Buffer
}

type nativeSmokeReadiness struct {
	Event           string `json:"event"`
	ProtocolVersion int    `json:"protocolVersion"`
	Address         string `json:"address"`
	Port            int    `json:"port"`
}

func (b *synchronizedBuffer) Write(data []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.b.Write(data)
}

func (b *synchronizedBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.b.String()
}

type smokeProcess struct {
	cmd     *exec.Cmd
	ready   <-chan nativeSmokeReadiness
	exited  <-chan error
	stdout  *synchronizedBuffer
	stderr  *synchronizedBuffer
	started time.Time
}

// TestAccountBridgeNativeProcessSmoke is opt-in because it compiles and starts
// a native child process. The release matrix enables it on all five target
// runners. Coverage is deliberately layered: the exact canonical binary gets
// loader plus malformed-bootstrap fail-closed checks; a second native binary
// built from the same source with an ephemeral test trust root exercises the
// successful bootstrap/readiness/health chain. Production eligibility signing
// material is never required by CI and never enters a test process.
func TestAccountBridgeNativeProcessSmoke(t *testing.T) {
	if os.Getenv(nativeProcessSmokeEnvironment) != "1" {
		t.Skip("native process smoke is enabled only by the release matrix")
	}
	setTestBootstrapReleaseIdentity(t)

	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("generate ephemeral eligibility signer: %v", err)
	}
	trustRoot := base64.RawURLEncoding.EncodeToString(publicKey)
	binary := buildNativeSmokeBinary(t, trustRoot)
	canonicalBinary := assertCanonicalReleaseBinaryLoads(t)

	runtimeRoot := t.TempDir()
	authDir := filepath.Join(runtimeRoot, "auth")
	configPath := filepath.Join(runtimeRoot, "config.json")
	fixedPluginDir := stageNativeSmokeFixedPlugin(t)
	const managementKey = "native-smoke-management-key"
	configBytes, err := json.Marshal(map[string]any{
		"host":     "127.0.0.1",
		"port":     0,
		"auth-dir": authDir,
		"api-keys": []string{"native-smoke-inference-key"},
		"remote-management": map[string]any{
			"allow-remote":              false,
			"secret-key":                managementKey,
			"disable-control-panel":     true,
			"disable-auto-update-panel": true,
			"panel-github-repository":   "",
		},
		"plugins": map[string]any{
			"enabled": true,
			"dir":     fixedPluginDir,
			"configs": map[string]any{"gemini-cli": map[string]any{"enabled": true}},
		},
		"pprof":                     map[string]any{"enable": false},
		"ws-auth":                   true,
		"usage-statistics-enabled":  false,
		"proxy-url":                 "direct",
		"disable-claude-cloak-mode": true,
		"codex":                     map[string]any{"identity-confuse": false},
	})
	if err != nil {
		t.Fatalf("marshal controlled config: %v", err)
	}
	// Deliberately start with ordinary creation permissions. The child must
	// tighten Unix modes / Windows DACLs before reading the secrets.
	if err = os.WriteFile(configPath, configBytes, 0o644); err != nil {
		t.Fatalf("write controlled config: %v", err)
	}
	assertCanonicalReleaseBinaryRejectsInvalidBootstrap(t, canonicalBinary, configPath)

	bootstrap := signedProcessSmokeBootstrap(t, privateKey)
	decoded, err := decodeAccountBridgeBootstrap(bytes.NewReader(bootstrap), trustRoot)
	if err != nil {
		t.Fatalf("self-check generated bootstrap: %v", err)
	}
	clearSecret(decoded.MasterKey)
	// Both published policy generations must decode: the child runs the full
	// seven-entry directory end to end, the legacy four-entry directory is
	// verified in process against the same trust root.
	var legacyWire accountBridgeBootstrapWire
	if err = json.Unmarshal(bootstrap, &legacyWire); err != nil {
		t.Fatalf("decode smoke bootstrap for legacy variant: %v", err)
	}
	legacyWire.ConnectorPolicies = legacyBootstrapConnectorPolicies()
	legacyBootstrap, err := json.Marshal(legacyWire)
	if err != nil {
		t.Fatalf("marshal legacy-generation smoke bootstrap: %v", err)
	}
	legacyDecoded, err := decodeAccountBridgeBootstrap(bytes.NewReader(legacyBootstrap), trustRoot)
	if err != nil {
		t.Fatalf("self-check legacy-generation bootstrap: %v", err)
	}
	clearSecret(legacyDecoded.MasterKey)
	child := startSmokeProcess(t, binary, configPath, bootstrap)
	readiness := awaitSmokeReadiness(t, child, 20*time.Second)
	if readiness.Event != "account-bridge-ready" ||
		readiness.ProtocolVersion != accountbridge.ProtocolVersion ||
		readiness.Address != "127.0.0.1" ||
		readiness.Port <= 0 {
		stopSmokeProcess(child)
		t.Fatalf("invalid readiness envelope: %+v", readiness)
	}

	baseURL := fmt.Sprintf("http://%s:%d", readiness.Address, readiness.Port)
	client := &http.Client{
		Timeout: 5 * time.Second,
		Transport: &http.Transport{
			Proxy:             nil,
			DisableKeepAlives: true,
		},
	}
	assertSmokeHealth(t, client, baseURL)
	assertSmokeFacadeFailsClosed(t, client, baseURL, managementKey)

	if err = fileperm.ValidatePrivateFile(configPath); err != nil {
		stopSmokeProcess(child)
		t.Fatalf("child did not protect config: %v", err)
	}
	if err = fileperm.ValidatePrivateDirectory(authDir); err != nil {
		stopSmokeProcess(child)
		t.Fatalf("child did not protect auth directory: %v", err)
	}
	seedPath := filepath.Join(authDir, ".account-bridge-route-seed")
	if err = fileperm.ValidatePrivateFile(seedPath); err != nil {
		stopSmokeProcess(child)
		t.Fatalf("child did not protect route seed: %v", err)
	}
	stopSmokeProcess(child)

	invalidBootstrap := corruptSmokeBootstrapSignature(t, bootstrap)
	rejected := startSmokeProcess(t, binary, configPath, invalidBootstrap)
	select {
	case readiness = <-rejected.ready:
		stopSmokeProcess(rejected)
		t.Fatalf("invalid bootstrap published readiness: %+v", readiness)
	case waitErr := <-rejected.exited:
		if waitErr == nil {
			t.Fatal("invalid bootstrap exited successfully instead of failing closed")
		}
	case <-time.After(10 * time.Second):
		stopSmokeProcess(rejected)
		t.Fatalf("invalid bootstrap did not fail closed; stdout=%s stderr=%s", rejected.stdout.String(), rejected.stderr.String())
	}
}

func stageNativeSmokeFixedPlugin(t *testing.T) string {
	t.Helper()
	source := strings.TrimSpace(os.Getenv("ACCOUNT_BRIDGE_FIXED_PLUGIN_SMOKE_PATH"))
	if source == "" {
		t.Fatal("ACCOUNT_BRIDGE_FIXED_PLUGIN_SMOKE_PATH is required for native process smoke")
	}
	sourceInfo, errStat := os.Stat(source)
	if errStat != nil || !sourceInfo.Mode().IsRegular() {
		t.Fatalf("fixed plugin smoke artifact missing at %s: %v", source, errStat)
	}
	pluginDir := filepath.Join(t.TempDir(), "account-bridge", "plugins")
	if errMkdir := os.MkdirAll(pluginDir, 0o700); errMkdir != nil {
		t.Fatalf("create fixed plugin smoke directory: %v", errMkdir)
	}
	copyFile := func(from, to string) {
		input, errRead := os.ReadFile(from)
		if errRead != nil {
			t.Fatalf("read fixed plugin smoke artifact %s: %v", from, errRead)
		}
		if errWrite := os.WriteFile(to, input, 0o600); errWrite != nil {
			t.Fatalf("write fixed plugin smoke artifact %s: %v", to, errWrite)
		}
	}
	copyFile(source, filepath.Join(pluginDir, filepath.Base(source)))
	copyFile(
		filepath.Join(filepath.Dir(source), "gemini-cli-LICENSE"),
		filepath.Join(pluginDir, "gemini-cli-LICENSE"),
	)
	if runtime.GOOS == "darwin" {
		helperSource := strings.TrimSpace(os.Getenv("ACCOUNT_BRIDGE_PLUGIN_HELPER_PATH"))
		if helperSource == "" {
			t.Fatal("ACCOUNT_BRIDGE_PLUGIN_HELPER_PATH is required for Darwin native process smoke")
		}
		helperInfo, errHelperStat := os.Lstat(helperSource)
		if errHelperStat != nil || !helperInfo.Mode().IsRegular() || helperInfo.Mode().Perm()&0o111 == 0 {
			t.Fatalf("Darwin plugin helper smoke artifact missing or not executable at %s: %v", helperSource, errHelperStat)
		}
		helperDir := filepath.Join(filepath.Dir(pluginDir), "bin")
		if errMkdir := os.MkdirAll(helperDir, 0o700); errMkdir != nil {
			t.Fatalf("create Darwin plugin helper smoke directory: %v", errMkdir)
		}
		helperBytes, errRead := os.ReadFile(helperSource)
		if errRead != nil {
			t.Fatalf("read Darwin plugin helper smoke artifact %s: %v", helperSource, errRead)
		}
		if errWrite := os.WriteFile(filepath.Join(helperDir, "oauthapi-plugin-host"), helperBytes, 0o700); errWrite != nil {
			t.Fatalf("write Darwin plugin helper smoke artifact: %v", errWrite)
		}
	}
	if runtime.GOOS == "linux" {
		// Formal minisign verification runs after release signing. This pre-sign
		// native smoke only needs the exact production layout so the Go security
		// allowlist and purego loader execute together.
		if errWrite := os.WriteFile(filepath.Join(pluginDir, "gemini-cli.minisig"), []byte("pre-sign native smoke\n"), 0o600); errWrite != nil {
			t.Fatalf("write fixed plugin smoke signature placeholder: %v", errWrite)
		}
	}
	return pluginDir
}

func buildNativeSmokeBinary(t *testing.T, trustRoot string) string {
	t.Helper()
	name := "oauthapi-llm-smoke"
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	output := filepath.Join(t.TempDir(), name)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()
	command := exec.CommandContext(
		ctx,
		"go",
		"build",
		"-mod=readonly",
		"-trimpath",
		"-ldflags",
		"-s -w -X main.Version="+testBootstrapComponentVersion+
			" -X main.AccountBridgeCrabCodeRelease="+testBootstrapCrabCodeRelease+
			" -X main.AccountBridgeEligibilityPublicKeyBase64URL="+trustRoot,
		"-o",
		output,
		".",
	)
	command.Env = append(os.Environ(), "GOWORK=off")
	combined, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("build native smoke binary: %v\n%s", err, combined)
	}
	return output
}

func canonicalReleaseBinaryPath(t *testing.T) string {
	t.Helper()
	target := strings.TrimSpace(os.Getenv("ACCOUNT_BRIDGE_NATIVE_TARGET"))
	wantTarget := map[string]string{
		"linux/amd64":   "x64-linux",
		"linux/arm64":   "arm64-linux",
		"darwin/amd64":  "x64-darwin",
		"darwin/arm64":  "arm64-darwin",
		"windows/amd64": "x64-win32",
	}[runtime.GOOS+"/"+runtime.GOARCH]
	if target == "" || target != wantTarget {
		t.Fatalf("ACCOUNT_BRIDGE_NATIVE_TARGET=%q, want %q for %s/%s", target, wantTarget, runtime.GOOS, runtime.GOARCH)
	}
	binaryName := "oauthapi-llm"
	if runtime.GOOS == "windows" {
		binaryName += ".exe"
	}
	return filepath.Clean(filepath.Join("..", "..", "..", "..", "dist", "account-bridge", target, "bin", binaryName))
}

func assertCanonicalReleaseBinaryLoads(t *testing.T) string {
	t.Helper()
	binary := canonicalReleaseBinaryPath(t)
	info, err := os.Stat(binary)
	if err != nil || !info.Mode().IsRegular() {
		t.Fatalf("canonical native release binary missing at %s: %v", binary, err)
	}
	command := exec.Command(binary, "-h")
	command.Env = safeSmokeEnvironment()
	if output, err := command.CombinedOutput(); err != nil {
		t.Fatalf("canonical native release binary loader check failed: %v\n%s", err, output)
	}
	return binary
}

func assertCanonicalReleaseBinaryRejectsInvalidBootstrap(t *testing.T, binary, configPath string) {
	t.Helper()
	child := startSmokeProcess(t, binary, configPath, []byte(`{"invalid":true}`))
	select {
	case readiness := <-child.ready:
		stopSmokeProcess(child)
		t.Fatalf("canonical binary published readiness for invalid bootstrap: %+v", readiness)
	case waitErr := <-child.exited:
		if waitErr == nil {
			t.Fatal("canonical binary accepted invalid bootstrap")
		}
	case <-time.After(10 * time.Second):
		stopSmokeProcess(child)
		t.Fatalf("canonical binary did not reject invalid bootstrap; stdout=%s stderr=%s", child.stdout.String(), child.stderr.String())
	}
}

func signedProcessSmokeBootstrap(t *testing.T, privateKey ed25519.PrivateKey) []byte {
	t.Helper()
	now := time.Now().UTC().Unix()
	client := testBootstrapClient()
	payload, err := json.Marshal(accountbridge.EligibilityPayload{
		Audience:              accountBridgeGrantAudience,
		Version:               accountBridgeGrantVersion,
		Client:                client,
		AllowedClientVersions: exactBootstrapVersionRanges(client),
		PolicyVersion:         "native-smoke-policy",
		IssuedAt:              now - 1,
		ExpiresAt:             now + 240,
		CountryCode:           "US",
		RegionAllowed:         true,
		ConnectorIDs:          accountbridge.ConnectorIDs(),
	})
	if err != nil {
		t.Fatalf("marshal smoke grant: %v", err)
	}
	encoding := base64.RawURLEncoding
	raw, err := json.Marshal(accountBridgeBootstrapWire{
		MasterKeyBase64URL: encoding.EncodeToString(bytes.Repeat([]byte{0x4d}, 32)),
		RequestNonce:       client.RequestNonce,
		Grant: accountbridge.SignedEligibilityGrant{
			PayloadBase64URL:   encoding.EncodeToString(payload),
			SignatureBase64URL: encoding.EncodeToString(ed25519.Sign(privateKey, payload)),
		},
		// The DTO requires exactly one published directory generation; smoke
		// exercises the full seven-entry generation end to end (the legacy
		// four-entry generation is covered by an in-process decode check).
		// Every independent gate stays false/blocked so valid regional
		// eligibility cannot enable a connector during smoke.
		ConnectorPolicies: fullBootstrapConnectorPolicies(),
	})
	if err != nil {
		t.Fatalf("marshal smoke bootstrap: %v", err)
	}
	return raw
}

func corruptSmokeBootstrapSignature(t *testing.T, raw []byte) []byte {
	t.Helper()
	var wire accountBridgeBootstrapWire
	if err := json.Unmarshal(raw, &wire); err != nil {
		t.Fatalf("decode smoke bootstrap for corruption: %v", err)
	}
	signature := []byte(wire.Grant.SignatureBase64URL)
	if len(signature) == 0 {
		t.Fatal("smoke bootstrap signature is empty")
	}
	if signature[0] == 'A' {
		signature[0] = 'B'
	} else {
		signature[0] = 'A'
	}
	wire.Grant.SignatureBase64URL = string(signature)
	corrupt, err := json.Marshal(wire)
	if err != nil {
		t.Fatalf("marshal corrupt bootstrap: %v", err)
	}
	return corrupt
}

func startSmokeProcess(t *testing.T, binary, configPath string, bootstrap []byte) *smokeProcess {
	t.Helper()
	reader, writer, err := os.Pipe()
	if err != nil {
		t.Fatalf("create bootstrap pipe: %v", err)
	}
	command := exec.Command(binary, "-config", configPath)
	command.Env = safeSmokeEnvironment()
	if runtime.GOOS == "windows" {
		command.Stdin = reader
	} else {
		command.ExtraFiles = []*os.File{reader}
	}
	stdoutPipe, err := command.StdoutPipe()
	if err != nil {
		_ = reader.Close()
		_ = writer.Close()
		t.Fatalf("create child stdout pipe: %v", err)
	}
	stderr := &synchronizedBuffer{}
	command.Stderr = stderr
	if err = command.Start(); err != nil {
		_ = reader.Close()
		_ = writer.Close()
		t.Fatalf("start native smoke child: %v", err)
	}
	_ = reader.Close()
	if _, err = writer.Write(bootstrap); err != nil {
		_ = writer.Close()
		_ = command.Process.Kill()
		_ = command.Wait()
		t.Fatalf("write child bootstrap: %v", err)
	}
	if err = writer.Close(); err != nil {
		_ = command.Process.Kill()
		_ = command.Wait()
		t.Fatalf("close child bootstrap pipe: %v", err)
	}

	ready := make(chan nativeSmokeReadiness, 1)
	stdout := &synchronizedBuffer{}
	go func() {
		scanner := bufio.NewScanner(io.TeeReader(stdoutPipe, stdout))
		for scanner.Scan() {
			var candidate nativeSmokeReadiness
			if json.Unmarshal(scanner.Bytes(), &candidate) == nil && candidate.Event == "account-bridge-ready" {
				select {
				case ready <- candidate:
				default:
				}
			}
		}
	}()
	exited := make(chan error, 1)
	go func() { exited <- command.Wait() }()
	return &smokeProcess{cmd: command, ready: ready, exited: exited, stdout: stdout, stderr: stderr, started: time.Now()}
}

func awaitSmokeReadiness(t *testing.T, child *smokeProcess, timeout time.Duration) nativeSmokeReadiness {
	t.Helper()
	select {
	case readiness := <-child.ready:
		return readiness
	case waitErr := <-child.exited:
		t.Fatalf("native smoke child exited before readiness: %v; stdout=%s stderr=%s", waitErr, child.stdout.String(), child.stderr.String())
	case <-time.After(timeout):
		stopSmokeProcess(child)
		t.Fatalf("native smoke readiness timed out after %s; stdout=%s stderr=%s", time.Since(child.started), child.stdout.String(), child.stderr.String())
	}
	return nativeSmokeReadiness{}
}

func stopSmokeProcess(child *smokeProcess) {
	if child == nil || child.cmd == nil || child.cmd.Process == nil {
		return
	}
	_ = child.cmd.Process.Kill()
	select {
	case <-child.exited:
	case <-time.After(5 * time.Second):
	}
}

func assertSmokeHealth(t *testing.T, client *http.Client, baseURL string) {
	t.Helper()
	response, err := client.Get(baseURL + "/healthz")
	if err != nil {
		t.Fatalf("GET /healthz: %v", err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		t.Fatalf("GET /healthz status=%d", response.StatusCode)
	}
	var body map[string]string
	if err = json.NewDecoder(response.Body).Decode(&body); err != nil || body["status"] != "ok" {
		t.Fatalf("GET /healthz body=%v err=%v", body, err)
	}
}

func assertSmokeFacadeFailsClosed(t *testing.T, client *http.Client, baseURL, managementKey string) {
	t.Helper()
	const path = "/v0/account-bridge/internal/connectors"
	request, err := http.NewRequest(http.MethodGet, baseURL+path, nil)
	if err != nil {
		t.Fatal(err)
	}
	response, err := client.Do(request)
	if err != nil {
		t.Fatalf("unauthenticated connector request: %v", err)
	}
	_ = response.Body.Close()
	if response.StatusCode != http.StatusUnauthorized {
		t.Fatalf("unauthenticated connector status=%d, want 401", response.StatusCode)
	}

	request, err = http.NewRequest(http.MethodGet, baseURL+path, nil)
	if err != nil {
		t.Fatal(err)
	}
	request.Header.Set("Authorization", "Bearer "+managementKey)
	response, err = client.Do(request)
	if err != nil {
		t.Fatalf("authenticated connector request: %v", err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		t.Fatalf("authenticated connector status=%d, want 200", response.StatusCode)
	}
	var body struct {
		Connectors []struct {
			ConnectorID    string  `json:"connectorId"`
			Enabled        bool    `json:"enabled"`
			DisabledReason *string `json:"disabledReasonCode"`
			TermsStatus    string  `json:"termsStatus"`
		} `json:"connectors"`
	}
	if err = json.NewDecoder(response.Body).Decode(&body); err != nil {
		t.Fatalf("decode connector response: %v", err)
	}
	if len(body.Connectors) != len(accountbridge.ConnectorIDs()) {
		t.Fatalf("connector count=%d, want %d", len(body.Connectors), len(accountbridge.ConnectorIDs()))
	}
	for _, connector := range body.Connectors {
		if connector.Enabled || connector.DisabledReason == nil || *connector.DisabledReason == "" || connector.TermsStatus != "blocked" {
			t.Fatalf("connector was not fail-closed without signed policy: %+v", connector)
		}
	}
}

func safeSmokeEnvironment() []string {
	allowed := map[string]struct{}{
		"APPDATA": {}, "COMSPEC": {}, "HOME": {}, "LANG": {}, "LC_ALL": {},
		"LOCALAPPDATA": {}, "PATH": {}, "SYSTEMDRIVE": {}, "SYSTEMROOT": {},
		"TEMP": {}, "TMP": {}, "TMPDIR": {}, "USERPROFILE": {}, "WINDIR": {},
	}
	result := make([]string, 0, len(allowed)+1)
	for _, entry := range os.Environ() {
		key, _, ok := strings.Cut(entry, "=")
		if !ok {
			continue
		}
		if _, keep := allowed[strings.ToUpper(key)]; keep {
			result = append(result, entry)
		}
	}
	return append(result, "NO_COLOR=1")
}
