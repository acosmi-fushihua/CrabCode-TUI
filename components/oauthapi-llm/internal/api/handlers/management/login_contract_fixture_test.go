package management

import (
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/auth/autherrors"
	xaiauth "github.com/acosmi/OAuthAPI-LLM/internal/auth/xai"
	"github.com/acosmi/OAuthAPI-LLM/internal/config"
)

// loginContractFixture is the component-local contract shared with the direct
// TUI consumer. It deliberately has no dependency on another application
// surface or repository tree.
type loginContractFixture struct {
	PollErrorCodes    []string `json:"pollErrorCodes"`
	PollReservedCodes []string `json:"pollReservedCodes"`
	Connectors        []struct {
		ConnectorID           string `json:"connectorId"`
		Flow                  string `json:"flow"`
		UserCode              string `json:"userCode"`
		ExpiresInFallbackSecs int    `json:"expiresInFallbackSecs"`
	} `json:"connectors"`
}

func loadLoginContractFixture(t *testing.T) loginContractFixture {
	t.Helper()
	path := filepath.Join("testdata", "account-bridge-login-contract.json")
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read fixture: %v", err)
	}
	var fixture loginContractFixture
	if err := json.Unmarshal(raw, &fixture); err != nil {
		t.Fatalf("parse fixture: %v", err)
	}
	return fixture
}

func TestOAuthSessionErrorCodeEnumMatchesFixture(t *testing.T) {
	fixture := loadLoginContractFixture(t)
	implemented := []string{
		OAuthSessionErrAuthorizationDenied,
		OAuthSessionErrLoginTimeout,
		OAuthSessionErrUpstreamUnavailable,
		OAuthSessionErrTokenExchangeFailed,
		OAuthSessionErrProvisioningFailed,
		OAuthSessionErrCredentialSaveFailed,
		OAuthSessionErrStateMismatch,
		OAuthSessionErrLoginFailed,
	}
	got := append([]string(nil), implemented...)
	want := append([]string(nil), fixture.PollErrorCodes...)
	sort.Strings(got)
	sort.Strings(want)
	if strings.Join(got, ",") != strings.Join(want, ",") {
		t.Fatalf("enum drift:\n  go      = %v\n  fixture = %v", got, want)
	}
	// Every enum member must survive normalization; anything else must not.
	for _, code := range implemented {
		if normalizeOAuthSessionErrorCode(code) != code {
			t.Fatalf("enum member %q rejected by normalize", code)
		}
	}
	if normalizeOAuthSessionErrorCode("made_up_code") != "" {
		t.Fatal("unknown code passed normalization")
	}
	// The association code is reserved on the wire but is not a store code.
	if len(fixture.PollReservedCodes) != 1 || fixture.PollReservedCodes[0] != "login_account_association_unavailable" {
		t.Fatalf("reserved codes drifted: %v", fixture.PollReservedCodes)
	}
}

// TestStartHandlersDeclareFixtureFlows pins the per-connector flow the Go
// builders actually hardcode (symbol-anchored source scan — the same spirit
// as the repo's counting gates) against the direct-TUI fixture, so the
// consumer catalog, this implementation, and the fixture cannot drift apart
// silently.
func TestStartHandlersDeclareFixtureFlows(t *testing.T) {
	fixture := loadLoginContractFixture(t)
	raw, err := os.ReadFile("auth_files.go")
	if err != nil {
		t.Fatalf("read auth_files.go: %v", err)
	}
	src := string(raw)
	builders := map[string]string{
		"openai":    "RequestCodexToken",
		"anthropic": "RequestAnthropicToken",
		"xai":       "RequestXAIToken",
		"qwen":      "RequestQwenToken",
		"kimi":      "RequestKimiToken",
		"zai":       "RequestZaiToken",
	}
	section := func(symbol string) string {
		start := strings.Index(src, "func (h *Handler) "+symbol+"(")
		if start < 0 {
			t.Fatalf("builder %s not found", symbol)
		}
		rest := src[start+1:]
		end := strings.Index(rest, "\nfunc ")
		if end < 0 {
			return src[start:]
		}
		return src[start : start+1+end]
	}
	for _, connector := range fixture.Connectors {
		symbol, ok := builders[connector.ConnectorID]
		if !ok {
			if connector.ConnectorID != "google" {
				t.Fatalf("fixture connector %q has no builder mapping", connector.ConnectorID)
			}
			continue // google delegates to the plugin host; no literal flow key.
		}
		body := section(symbol)
		declaresDevice := strings.Contains(body, `"flow": "device"`)
		wantDevice := connector.Flow == "device"
		if declaresDevice != wantDevice {
			t.Fatalf("%s (%s): flow declaration = device:%t, fixture wants device:%t",
				connector.ConnectorID, symbol, declaresDevice, wantDevice)
		}
	}
}

func TestDeviceFlowExpiresInFallbacksMatchFixture(t *testing.T) {
	fixture := loadLoginContractFixture(t)
	for _, connector := range fixture.Connectors {
		switch connector.ConnectorID {
		case "xai":
			if want := int(xaiauth.MaxPollDuration / time.Second); connector.ExpiresInFallbackSecs != want {
				t.Fatalf("xai fallback drift: fixture=%d go=%d", connector.ExpiresInFallbackSecs, want)
			}
		case "qwen", "kimi", "zai":
			if connector.ExpiresInFallbackSecs != deviceFlowExpiresInFallbackSecs {
				t.Fatalf("%s fallback drift: fixture=%d go=%d",
					connector.ConnectorID, connector.ExpiresInFallbackSecs, deviceFlowExpiresInFallbackSecs)
			}
		}
	}
}

// F1 正/负对照 (§7 验收铁律: "managementCallbackURL 注入真实端口的正/负对照").
func TestManagementCallbackURLUsesRuntimeListenPort(t *testing.T) {
	h := &Handler{cfg: &config.Config{}}
	if _, err := h.managementCallbackURL("/anthropic/callback"); err == nil {
		t.Fatal("expected error with cfg.Port=0 and no runtime port")
	}
	h.SetRuntimeListenPort(0)
	h.SetRuntimeListenPort(70000)
	if _, err := h.managementCallbackURL("/anthropic/callback"); err == nil {
		t.Fatal("out-of-range runtime ports must be ignored")
	}
	h.SetRuntimeListenPort(43210)
	url, err := h.managementCallbackURL("anthropic/callback")
	if err != nil {
		t.Fatalf("callback URL with runtime port: %v", err)
	}
	if url != "http://127.0.0.1:43210/anthropic/callback" {
		t.Fatalf("callback URL = %q", url)
	}
	// An explicitly configured port still wins over the runtime observation.
	h.cfg.Port = 8317
	url, err = h.managementCallbackURL("/x")
	if err != nil || url != "http://127.0.0.1:8317/x" {
		t.Fatalf("configured-port URL = %q err=%v", url, err)
	}
}

func TestCallbackForwarderStartErrorCodeDistinguishesPortBusy(t *testing.T) {
	holder, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	defer func() { _ = holder.Close() }()
	port := holder.Addr().(*net.TCPAddr).Port

	_, errBusy := startCallbackForwarder(port, "test", "http://127.0.0.1:1/cb")
	if errBusy == nil {
		t.Fatal("expected bind conflict")
	}
	if code := callbackForwarderStartErrorCode(errBusy); code != "callback_port_busy" {
		t.Fatalf("busy classification = %q (err=%v)", code, errBusy)
	}
	if code := callbackForwarderStartErrorCode(errors.New("tls broke")); code != "failed to start callback server" {
		t.Fatalf("generic classification = %q", code)
	}
	if code := callbackForwarderStartErrorCode(nil); code != "failed to start callback server" {
		t.Fatalf("nil classification = %q", code)
	}
}

func TestRecoverGuardsStopGoroutinePanics(t *testing.T) {
	done := make(chan struct{})
	go func() {
		defer close(done)
		defer RecoverAccountBridgeGoroutine("exported guard")
		panic("boom-exported")
	}()
	<-done

	done = make(chan struct{})
	go func() {
		defer close(done)
		defer recoverOAuthGoroutine("internal guard")
		panic(fmt.Errorf("boom-internal"))
	}()
	<-done
	// Reaching here at all is the assertion: an unrecovered panic in either
	// goroutine would have killed the test binary.
}

func TestSetOAuthSessionErrorCodedRoundTrip(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	replaceOAuthSessionStoreForTest(t, store)

	store.Register("coded-state", "kimi")
	SetOAuthSessionErrorCoded("coded-state", OAuthSessionErrUpstreamUnavailable, "kimi: token request failed")
	if code := GetOAuthSessionErrorCode("coded-state"); code != OAuthSessionErrUpstreamUnavailable {
		t.Fatalf("stored code = %q", code)
	}

	store.Register("uncoded-state", "kimi")
	SetOAuthSessionError("uncoded-state", "legacy failure")
	if code := GetOAuthSessionErrorCode("uncoded-state"); code != "" {
		t.Fatalf("uncoded write produced code %q", code)
	}

	store.Register("bogus-code-state", "kimi")
	SetOAuthSessionErrorCoded("bogus-code-state", "not_in_enum", "message")
	if code := GetOAuthSessionErrorCode("bogus-code-state"); code != "" {
		t.Fatalf("out-of-enum code survived: %q", code)
	}
	if GetOAuthSessionErrorCode("missing-state") != "" {
		t.Fatal("missing session must report empty code")
	}
}

func TestOAuthWaitErrorCodeMapsSentinels(t *testing.T) {
	cases := []struct {
		err  error
		want string
	}{
		{nil, ""},
		{errors.New("plain"), ""},
		{autherrors.Classify(autherrors.ErrAuthorizationDenied, errors.New("kimi: access denied by user")), OAuthSessionErrAuthorizationDenied},
		{autherrors.Classify(autherrors.ErrAuthorizationTimeout, errors.New("qwen: device code expired")), OAuthSessionErrLoginTimeout},
		{autherrors.Classify(autherrors.ErrUpstreamUnavailable, errors.New("zai: polling failed")), OAuthSessionErrUpstreamUnavailable},
		{fmt.Errorf("wrapped: %w", autherrors.Classify(autherrors.ErrAuthorizationDenied, errors.New("inner"))), OAuthSessionErrAuthorizationDenied},
	}
	for index, testCase := range cases {
		if got := oauthWaitErrorCode(testCase.err); got != testCase.want {
			t.Fatalf("case %d: oauthWaitErrorCode = %q, want %q", index, got, testCase.want)
		}
	}
	// Message text must stay byte-identical through classification.
	classified := autherrors.Classify(autherrors.ErrAuthorizationDenied, errors.New("kimi: access denied by user"))
	if classified.Error() != "kimi: access denied by user" {
		t.Fatalf("classified message drifted: %q", classified.Error())
	}
}
