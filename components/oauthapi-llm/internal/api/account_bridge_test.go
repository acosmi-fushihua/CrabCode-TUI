package api

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/accountbridge"
	accountbridgequota "github.com/acosmi/OAuthAPI-LLM/internal/accountbridge/quota"
	managementHandlers "github.com/acosmi/OAuthAPI-LLM/internal/api/handlers/management"
	proxyconfig "github.com/acosmi/OAuthAPI-LLM/internal/config"
	"github.com/acosmi/OAuthAPI-LLM/internal/registry"
	sdkaccess "github.com/acosmi/OAuthAPI-LLM/sdk/access"
	coreauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
	sdkconfig "github.com/acosmi/OAuthAPI-LLM/sdk/config"
	gin "github.com/gin-gonic/gin"
)

type fakeAccountBridgeQuotaReader struct {
	mu      sync.Mutex
	report  accountbridgequota.Report
	err     error
	calls   []bool
	authIDs []string
}

func (reader *fakeAccountBridgeQuotaReader) Read(_ context.Context, credential *coreauth.Auth, _ string, forceRefresh bool) (accountbridgequota.Report, error) {
	reader.mu.Lock()
	defer reader.mu.Unlock()
	reader.calls = append(reader.calls, forceRefresh)
	reader.authIDs = append(reader.authIDs, credential.ID)
	return reader.report, reader.err
}

func newAccountBridgeRuntimeForHTTPTest(t *testing.T, policies ...accountbridge.ConnectorPolicy) *accountbridge.Runtime {
	t.Helper()
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("generate eligibility key: %v", err)
	}
	client := accountbridge.EligibilityClientBinding{
		RequestNonce:                  base64.RawURLEncoding.EncodeToString(bytes.Repeat([]byte{0x61}, 32)),
		CrabCodeRelease:               "1.0.13",
		AccountBridgeComponentVersion: "7.2.71-crabcode.4",
		AccountBridgeProtocolVersion:  accountbridge.ProtocolVersion,
	}
	verifier, err := accountbridge.NewEligibilityVerifier(publicKey, "test-account-bridge", "v1", client)
	if err != nil {
		t.Fatalf("create verifier: %v", err)
	}
	now := time.Now().UTC().Unix()
	payload, err := json.Marshal(accountbridge.EligibilityPayload{
		Audience: "test-account-bridge",
		Version:  "v1",
		Client:   client,
		AllowedClientVersions: accountbridge.AllowedClientVersions{
			CrabCodeRelease:               accountbridge.InclusiveVersionRange{MinimumInclusive: client.CrabCodeRelease, MaximumInclusive: client.CrabCodeRelease},
			AccountBridgeComponentVersion: accountbridge.InclusiveVersionRange{MinimumInclusive: client.AccountBridgeComponentVersion, MaximumInclusive: client.AccountBridgeComponentVersion},
			AccountBridgeProtocolVersion:  accountbridge.InclusiveProtocolRange{MinimumInclusive: client.AccountBridgeProtocolVersion, MaximumInclusive: client.AccountBridgeProtocolVersion},
		},
		PolicyVersion: "test-policy",
		IssuedAt:      now - 1,
		ExpiresAt:     now + 120,
		CountryCode:   "US",
		RegionAllowed: true,
		ConnectorIDs:  accountbridge.ConnectorIDs(),
	})
	if err != nil {
		t.Fatalf("marshal grant: %v", err)
	}
	encoding := base64.RawURLEncoding
	grant := accountbridge.SignedEligibilityGrant{
		PayloadBase64URL:   encoding.EncodeToString(payload),
		SignatureBase64URL: encoding.EncodeToString(ed25519.Sign(privateKey, payload)),
	}
	routes, err := accountbridge.NewRouteStore(filepath.Join(t.TempDir(), "route.seed"))
	if err != nil {
		t.Fatalf("create route store: %v", err)
	}
	directoryPolicies := []accountbridge.ConnectorPolicy{
		{ConnectorID: accountbridge.ConnectorOpenAI, DisplayName: "Directory OpenAI", AuthMode: accountbridge.AuthModeDeviceCode, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
		{ConnectorID: accountbridge.ConnectorAnthropic, DisplayName: "Directory Anthropic", AuthMode: accountbridge.AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
		{ConnectorID: accountbridge.ConnectorGoogle, DisplayName: "Directory Google", AuthMode: accountbridge.AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
		{ConnectorID: accountbridge.ConnectorXAI, DisplayName: "Directory xAI", AuthMode: accountbridge.AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
	}
	for _, override := range policies {
		replaced := false
		for index := range directoryPolicies {
			if directoryPolicies[index].ConnectorID != override.ConnectorID {
				continue
			}
			if override.DisplayName == "" {
				override.DisplayName = directoryPolicies[index].DisplayName
			}
			if override.AuthMode == "" {
				override.AuthMode = directoryPolicies[index].AuthMode
			}
			if override.RegionPolicy == "" {
				override.RegionPolicy = directoryPolicies[index].RegionPolicy
			}
			directoryPolicies[index] = override
			replaced = true
			break
		}
		if !replaced {
			directoryPolicies = append(directoryPolicies, override)
		}
	}
	runtime, err := accountbridge.NewRuntimeWithConnectorPolicies(verifier, grant, routes, directoryPolicies)
	if err != nil {
		t.Fatalf("create runtime: %v", err)
	}
	return runtime
}

func TestAccountBridgeEnabledFacadeProjectsEvidenceAndFiltersUsage(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "test-management-key")
	runtime := newAccountBridgeRuntimeForHTTPTest(t, accountbridge.ConnectorPolicy{
		ConnectorID:           accountbridge.ConnectorOpenAI,
		FeatureEnabled:        true,
		TermsStatus:           "signed-off",
		ConformancePassed:     true,
		FixedArtifactVerified: true,
	})
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))
	credential := &coreauth.Auth{ID: "enabled-private-auth", Provider: "codex", Status: coreauth.StatusActive, CreatedAt: time.Now().UTC()}
	if _, err := runtime.AuthManager().Register(context.Background(), credential); err != nil {
		t.Fatalf("register auth: %v", err)
	}
	modelID := "account-bridge-evidence-model"
	registry.GetGlobalRegistry().RegisterClient(credential.ID, credential.Provider, []*registry.ModelInfo{{
		ID:                         modelID,
		DisplayName:                "Evidence Model",
		SupportedGenerationMethods: []string{"messages"},
		SupportedParameters:        []string{"tools", "response_format"},
		SupportedInputModalities:   []string{"text", "image"},
		SupportedOutputModalities:  []string{"text"},
		ContextLength:              200000,
		MaxCompletionTokens:        64000,
		Thinking: &registry.ThinkingSupport{
			ZeroAllowed:    true,
			DynamicAllowed: true,
			Levels:         []string{"low", "medium", "high", "max"},
		},
	}})
	t.Cleanup(func() { registry.GetGlobalRegistry().UnregisterClient(credential.ID) })

	modelsResponse := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/models", nil, true)
	if modelsResponse.Code != http.StatusOK {
		t.Fatalf("models status=%d body=%s", modelsResponse.Code, modelsResponse.Body.String())
	}
	var modelsPayload struct {
		Models []facadeModel `json:"models"`
	}
	if err := json.Unmarshal(modelsResponse.Body.Bytes(), &modelsPayload); err != nil {
		t.Fatalf("decode models: %v", err)
	}
	if len(modelsPayload.Models) != 1 {
		t.Fatalf("models=%d body=%s", len(modelsPayload.Models), modelsResponse.Body.String())
	}
	model := modelsPayload.Models[0]
	if model.ConnectorLabel != "Directory OpenAI" {
		t.Fatalf("connector label=%q, want signed directory label", model.ConnectorLabel)
	}
	if model.ChatRuntimeSupported == nil || !*model.ChatRuntimeSupported || model.SupportsTools == nil || !*model.SupportsTools || model.SupportsVision == nil || !*model.SupportsVision || model.SupportsJSONMode == nil || !*model.SupportsJSONMode {
		t.Fatalf("evidence capabilities not projected: %+v", model)
	}
	if model.ContextWindow == nil || *model.ContextWindow != 200000 || model.MaxOutputTokens == nil || *model.MaxOutputTokens != 64000 {
		t.Fatalf("token limits not projected: %+v", model)
	}
	if got := strings.Join(model.SupportedThinkingModes, ","); got != "auto,off,standard,deep" {
		t.Fatalf("thinking modes=%q", got)
	}

	usageResponse := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/usage?routeId="+model.RouteID, nil, true)
	if usageResponse.Code != http.StatusOK || !strings.Contains(usageResponse.Body.String(), model.RouteID) {
		t.Fatalf("filtered usage status=%d body=%s", usageResponse.Code, usageResponse.Body.String())
	}
	unknownRoute := strings.Repeat("A", accountbridge.RouteIDLength)
	emptyResponse := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/usage?routeId="+unknownRoute, nil, true)
	if emptyResponse.Code != http.StatusOK || strings.TrimSpace(emptyResponse.Body.String()) != "{\"snapshots\":[]}" {
		t.Fatalf("unknown route usage status=%d body=%s", emptyResponse.Code, emptyResponse.Body.String())
	}
	invalidResponse := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/usage?routeId=raw-provider-id", nil, true)
	if invalidResponse.Code != http.StatusBadRequest {
		t.Fatalf("invalid route status=%d body=%s", invalidResponse.Code, invalidResponse.Body.String())
	}
}

func TestFacadeModelCapabilitiesPreserveUnknownAsNull(t *testing.T) {
	capabilities := facadeModelCapabilities(&registry.ModelInfo{ID: "opaque-model"})
	if capabilities.ChatRuntimeSupported != nil || capabilities.SupportsTools != nil || capabilities.SupportsThinking != nil || capabilities.SupportsVision != nil || capabilities.SupportsJSONMode != nil || capabilities.DefaultThinkingMode != nil {
		t.Fatalf("unknown capability was fabricated: %+v", capabilities)
	}
	if got := strings.Join(capabilities.SupportedThinkingModes, ","); got != "auto" {
		t.Fatalf("unknown thinking modes=%q, want auto only", got)
	}
}

func accountBridgeHTTP(t *testing.T, server *Server, method, path string, body []byte, management bool) *httptest.ResponseRecorder {
	t.Helper()
	req := httptest.NewRequest(method, path, bytes.NewReader(body))
	if management {
		req.Header.Set("Authorization", "Bearer test-management-key")
	} else {
		req.Header.Set("Authorization", "Bearer test-key")
	}
	if len(body) > 0 {
		req.Header.Set("Content-Type", "application/json")
	}
	recorder := httptest.NewRecorder()
	server.engine.ServeHTTP(recorder, req)
	return recorder
}

func TestAccountBridgeFacadeActualHTTPIsRedactedAndDefaultDisabled(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "test-management-key")
	runtime := newAccountBridgeRuntimeForHTTPTest(t)
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))

	credential := &coreauth.Auth{
		ID:       "raw-auth-secret-id",
		Provider: "codex",
		Label:    "private-person@example.test",
		Status:   coreauth.StatusActive,
		Metadata: map[string]any{"access_token": "super-secret-token-value"},
	}
	if _, err := runtime.AuthManager().Register(context.Background(), credential); err != nil {
		t.Fatalf("register auth: %v", err)
	}
	modelID := "dynamic-model-from-registry"
	registry.GetGlobalRegistry().RegisterClient(credential.ID, credential.Provider, []*registry.ModelInfo{{
		ID:          modelID,
		DisplayName: "Dynamic Model",
	}})
	t.Cleanup(func() { registry.GetGlobalRegistry().UnregisterClient(credential.ID) })

	paths := []string{"connectors", "accounts", "models", "routes", "usage"}
	for _, endpoint := range paths {
		recorder := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/"+endpoint, nil, true)
		if recorder.Code != http.StatusOK {
			t.Fatalf("GET %s status=%d body=%s", endpoint, recorder.Code, recorder.Body.String())
		}
		body := recorder.Body.String()
		for _, secret := range []string{credential.ID, credential.Label, "access_token", "super-secret-token-value", "eligibilityGrant", "signature"} {
			if strings.Contains(body, secret) {
				t.Fatalf("GET %s leaked %q in %s", endpoint, secret, body)
			}
		}
	}

	connectors := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/connectors", nil, true)
	var catalog struct {
		Connectors []facadeConnector `json:"connectors"`
	}
	if err := json.Unmarshal(connectors.Body.Bytes(), &catalog); err != nil {
		t.Fatalf("decode connectors: %v", err)
	}
	if len(catalog.Connectors) != 7 {
		t.Fatalf("connectors=%d, want 7", len(catalog.Connectors))
	}
	wantSources := map[string]string{
		accountbridge.ConnectorOpenAI:    "codex",
		accountbridge.ConnectorAnthropic: "claude",
		accountbridge.ConnectorGoogle:    "gemini-cli",
		accountbridge.ConnectorXAI:       "xai",
		accountbridge.ConnectorQwen:      "qwen",
		accountbridge.ConnectorKimi:      "kimi",
		accountbridge.ConnectorZai:       "zai",
	}
	// The legacy four-entry policy generation carries no directory entries for
	// the qwen/kimi/zai connectors: they stay visible in the catalog with
	// empty presentation metadata and remain blocked.
	wantPresentation := map[string]struct {
		displayName string
		authMode    string
	}{
		accountbridge.ConnectorOpenAI:    {displayName: "Directory OpenAI", authMode: accountbridge.AuthModeDeviceCode},
		accountbridge.ConnectorAnthropic: {displayName: "Directory Anthropic", authMode: accountbridge.AuthModeBrowser},
		accountbridge.ConnectorGoogle:    {displayName: "Directory Google", authMode: accountbridge.AuthModeBrowser},
		accountbridge.ConnectorXAI:       {displayName: "Directory xAI", authMode: accountbridge.AuthModeBrowser},
		accountbridge.ConnectorQwen:      {},
		accountbridge.ConnectorKimi:      {},
		accountbridge.ConnectorZai:       {},
	}
	for _, connector := range catalog.Connectors {
		if connector.Enabled || connector.TermsStatus != "blocked" {
			t.Fatalf("connector was not default blocked: %+v", connector)
		}
		if connector.SourceProviderID != wantSources[connector.ConnectorID] {
			t.Fatalf("connector source drift: %+v", connector)
		}
		presentation := wantPresentation[connector.ConnectorID]
		if connector.DisplayName != presentation.displayName || connector.AuthMode != presentation.authMode {
			t.Fatalf("connector presentation did not come from directory: %+v", connector)
		}
		if connector.ConnectorID == accountbridge.ConnectorGoogle && (connector.DisabledReason == nil || *connector.DisabledReason != "fixed_plugin_missing") {
			t.Fatalf("google disabled reason=%v", connector.DisabledReason)
		}
	}

	for _, endpoint := range []string{"models", "routes", "usage"} {
		response := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/"+endpoint, nil, true)
		if strings.Contains(response.Body.String(), modelID) {
			t.Fatalf("blocked connector entered %s catalog: %s", endpoint, response.Body.String())
		}
	}

	loginStart := accountBridgeHTTP(t, server, http.MethodPost, "/v0/account-bridge/internal/login/start", []byte(`{"connectorId":"openai"}`), true)
	if loginStart.Code != http.StatusForbidden || !strings.Contains(loginStart.Body.String(), `"termsStatus":"blocked"`) {
		t.Fatalf("blocked login start status=%d body=%s", loginStart.Code, loginStart.Body.String())
	}
	state := "account-bridge-test-session"
	managementHandlers.RegisterOAuthSession(state, "codex")
	t.Cleanup(func() { managementHandlers.CompleteOAuthSession(state) })
	poll := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/login/poll?state="+state, nil, true)
	if poll.Code != http.StatusOK || !strings.Contains(poll.Body.String(), `"state":"pending"`) || !strings.Contains(poll.Body.String(), `"accountId":null`) {
		t.Fatalf("login poll status=%d body=%s", poll.Code, poll.Body.String())
	}
	cancel := accountBridgeHTTP(t, server, http.MethodDelete, "/v0/account-bridge/internal/login/cancel?state="+state, nil, true)
	if cancel.Code != http.StatusOK || !strings.Contains(cancel.Body.String(), `"cancelled":true`) {
		t.Fatalf("login cancel status=%d body=%s", cancel.Code, cancel.Body.String())
	}
}

func TestAccountBridgeLoginPollReturnsExactCompletedOpaqueAccount(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "test-management-key")
	runtime := newAccountBridgeRuntimeForHTTPTest(t, accountbridge.ConnectorPolicy{
		ConnectorID:           accountbridge.ConnectorOpenAI,
		FeatureEnabled:        true,
		TermsStatus:           "signed-off",
		ConformancePassed:     true,
		FixedArtifactVerified: true,
	})
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))
	target := &coreauth.Auth{ID: "reauthorized-private-auth", Provider: "codex", Status: coreauth.StatusActive}
	unrelated := &coreauth.Auth{ID: "concurrent-unrelated-auth", Provider: "codex", Status: coreauth.StatusActive}
	for _, credential := range []*coreauth.Auth{target, unrelated} {
		if _, err := runtime.AuthManager().Register(context.Background(), credential); err != nil {
			t.Fatalf("register %s: %v", credential.ID, err)
		}
	}
	wantAccountID, err := runtime.Routes().AccountID(accountbridge.ConnectorOpenAI, target.ID)
	if err != nil {
		t.Fatalf("derive target account ID: %v", err)
	}
	unrelatedAccountID, err := runtime.Routes().AccountID(accountbridge.ConnectorOpenAI, unrelated.ID)
	if err != nil {
		t.Fatalf("derive unrelated account ID: %v", err)
	}

	state := "account-bridge-exact-completed-login"
	managementHandlers.RegisterOAuthSession(state, "codex")
	managementHandlers.CompleteOAuthSessionWithAuthIDs(state, target.ID)
	poll := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/login/poll?state="+state, nil, true)
	if poll.Code != http.StatusOK {
		t.Fatalf("login poll status=%d body=%s", poll.Code, poll.Body.String())
	}
	var response struct {
		State     string  `json:"state"`
		AccountID *string `json:"accountId"`
		ErrorCode *string `json:"errorCode"`
	}
	if errDecode := json.Unmarshal(poll.Body.Bytes(), &response); errDecode != nil {
		t.Fatalf("decode login poll: %v", errDecode)
	}
	if response.State != "succeeded" || response.AccountID == nil || *response.AccountID != wantAccountID || response.ErrorCode != nil {
		t.Fatalf("exact login association response = %#v, want %q", response, wantAccountID)
	}
	if *response.AccountID == unrelatedAccountID || strings.Contains(poll.Body.String(), target.ID) || strings.Contains(poll.Body.String(), unrelated.ID) {
		t.Fatalf("login poll guessed or leaked a private auth identity: %s", poll.Body.String())
	}
}

func TestAccountBridgeLoginPollFailsClosedWithoutOneExactCompletedAuth(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "test-management-key")
	runtime := newAccountBridgeRuntimeForHTTPTest(t, accountbridge.ConnectorPolicy{
		ConnectorID:           accountbridge.ConnectorOpenAI,
		FeatureEnabled:        true,
		TermsStatus:           "signed-off",
		ConformancePassed:     true,
		FixedArtifactVerified: true,
	})
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))
	for _, credential := range []*coreauth.Auth{
		{ID: "first-private-auth", Provider: "codex", Status: coreauth.StatusActive},
		{ID: "second-private-auth", Provider: "codex", Status: coreauth.StatusActive},
		{ID: "wrong-provider-auth", Provider: "xai", Status: coreauth.StatusActive},
	} {
		if _, err := runtime.AuthManager().Register(context.Background(), credential); err != nil {
			t.Fatalf("register %s: %v", credential.ID, err)
		}
	}

	tests := []struct {
		state   string
		authIDs []string
	}{
		{state: "account-bridge-no-completed-auth"},
		{state: "account-bridge-multiple-completed-auths", authIDs: []string{"first-private-auth", "second-private-auth"}},
		{state: "account-bridge-cross-provider-completed-auth", authIDs: []string{"wrong-provider-auth"}},
	}
	for _, testCase := range tests {
		managementHandlers.RegisterOAuthSession(testCase.state, "codex")
		managementHandlers.CompleteOAuthSessionWithAuthIDs(testCase.state, testCase.authIDs...)
		poll := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/login/poll?state="+testCase.state, nil, true)
		if poll.Code != http.StatusOK || !strings.Contains(poll.Body.String(), `"state":"failed"`) || !strings.Contains(poll.Body.String(), `"errorCode":"login_account_association_unavailable"`) || !strings.Contains(poll.Body.String(), `"accountId":null`) {
			t.Fatalf("%s login poll did not fail closed: status=%d body=%s", testCase.state, poll.Code, poll.Body.String())
		}
	}
}

func TestAccountBridgeInferenceWallActualHTTP(t *testing.T) {
	withoutGrant := newTestServer(t)
	noGrant := httptest.NewRequest(http.MethodPost, "/v1/messages", strings.NewReader(`{"model":"model-a","messages":[]}`))
	noGrant.Header.Set("Authorization", "Bearer test-key")
	noGrant.Header.Set(accountRouteHeader, strings.Repeat("A", accountbridge.RouteIDLength))
	noGrantRecorder := httptest.NewRecorder()
	withoutGrant.engine.ServeHTTP(noGrantRecorder, noGrant)
	if noGrantRecorder.Code != http.StatusForbidden {
		t.Fatalf("no-grant status=%d body=%s", noGrantRecorder.Code, noGrantRecorder.Body.String())
	}

	runtime := newAccountBridgeRuntimeForHTTPTest(t)
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))
	for _, credential := range []*coreauth.Auth{
		{ID: "account-a-private", Provider: "codex", Status: coreauth.StatusActive},
		{ID: "account-b-private", Provider: "codex", Status: coreauth.StatusActive},
	} {
		if _, err := runtime.AuthManager().Register(context.Background(), credential); err != nil {
			t.Fatalf("register %s: %v", credential.ID, err)
		}
	}
	registry.GetGlobalRegistry().RegisterClient("account-a-private", "codex", []*registry.ModelInfo{{ID: "model-a"}})
	registry.GetGlobalRegistry().RegisterClient("account-b-private", "codex", []*registry.ModelInfo{{ID: "model-b"}})
	t.Cleanup(func() {
		registry.GetGlobalRegistry().UnregisterClient("account-a-private")
		registry.GetGlobalRegistry().UnregisterClient("account-b-private")
	})
	routeA, err := runtime.Routes().Register(accountbridge.Binding{ConnectorID: accountbridge.ConnectorOpenAI, AuthID: "account-a-private", ModelID: "model-a"})
	if err != nil {
		t.Fatalf("register route A: %v", err)
	}
	routeB, err := runtime.Routes().Register(accountbridge.Binding{ConnectorID: accountbridge.ConnectorOpenAI, AuthID: "account-b-private", ModelID: "model-b"})
	if err != nil {
		t.Fatalf("register route B: %v", err)
	}
	staleRoute, err := runtime.Routes().Register(accountbridge.Binding{ConnectorID: accountbridge.ConnectorOpenAI, AuthID: "missing-private-account", ModelID: "model-a"})
	if err != nil {
		t.Fatalf("register stale route: %v", err)
	}

	tests := []struct {
		name       string
		routeID    string
		model      string
		wantStatus int
	}{
		{name: "missing route", model: "model-a", wantStatus: http.StatusBadRequest},
		{name: "unknown route", routeID: strings.Repeat("A", accountbridge.RouteIDLength), model: "model-a", wantStatus: http.StatusNotFound},
		{name: "stale route", routeID: staleRoute, model: "model-a", wantStatus: http.StatusNotFound},
		{name: "cross-account model mismatch", routeID: routeA, model: "model-b", wantStatus: http.StatusConflict},
		{name: "account A remains disabled", routeID: routeA, model: "model-a", wantStatus: http.StatusForbidden},
		{name: "account B remains disabled", routeID: routeB, model: "model-b", wantStatus: http.StatusForbidden},
	}
	for _, testCase := range tests {
		t.Run(testCase.name, func(t *testing.T) {
			req := httptest.NewRequest(http.MethodPost, "/v1/messages", strings.NewReader(`{"model":"`+testCase.model+`","messages":[]}`))
			req.Header.Set("Authorization", "Bearer test-key")
			if testCase.routeID != "" {
				req.Header.Set(accountRouteHeader, testCase.routeID)
			}
			recorder := httptest.NewRecorder()
			server.engine.ServeHTTP(recorder, req)
			if recorder.Code != testCase.wantStatus {
				t.Fatalf("status=%d want=%d body=%s", recorder.Code, testCase.wantStatus, recorder.Body.String())
			}
			if req.Header.Get(accountRouteHeader) != "" {
				t.Fatal("private route header remained on the execution request")
			}
		})
	}
}

func TestAccountBridgeDoesNotRegisterImplicitAccountPoolRoutes(t *testing.T) {
	runtime := newAccountBridgeRuntimeForHTTPTest(t)
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))

	for _, testCase := range []struct {
		method string
		path   string
	}{
		{http.MethodGet, "/v1/models"},
		{http.MethodPost, "/v1/chat/completions"},
		{http.MethodPost, "/v1/completions"},
		{http.MethodPost, "/v1/images/generations"},
		{http.MethodPost, "/v1/responses"},
		{http.MethodPost, "/openai/v1/videos"},
		{http.MethodPost, "/backend-api/codex/responses"},
		{http.MethodPost, "/v1beta/models/gemini:generateContent"},
	} {
		req := httptest.NewRequest(testCase.method, testCase.path, strings.NewReader(`{}`))
		req.Header.Set("Authorization", "Bearer test-key")
		recorder := httptest.NewRecorder()
		server.engine.ServeHTTP(recorder, req)
		if recorder.Code != http.StatusNotFound {
			t.Fatalf("%s %s status=%d body=%s, want route absent", testCase.method, testCase.path, recorder.Code, recorder.Body.String())
		}
	}
}

func TestAccountBridgeDoesNotExposeGenericPluginRoutes(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "test-management-key")
	runtime := newAccountBridgeRuntimeForHTTPTest(t)
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))

	for _, testCase := range []struct {
		method string
		path   string
	}{
		{http.MethodGet, "/v0/management/gemini-cli-auth-url"},
		{http.MethodGet, "/v0/management/plugins"},
		{http.MethodGet, "/v0/resource/plugins/gemini-cli/status"},
	} {
		req := httptest.NewRequest(testCase.method, testCase.path, nil)
		req.Header.Set("Authorization", "Bearer test-management-key")
		recorder := httptest.NewRecorder()
		server.engine.ServeHTTP(recorder, req)
		if recorder.Code != http.StatusNotFound {
			t.Fatalf("%s %s status=%d body=%s, want generic plugin route absent", testCase.method, testCase.path, recorder.Code, recorder.Body.String())
		}
	}
}

func TestFacadeUsageProjectsModelRuntimeEvidenceWithoutFabricatingPercent(t *testing.T) {
	now := time.Date(2026, 7, 13, 12, 0, 0, 0, time.UTC)
	cooldownUntil := now.Add(7 * time.Minute)
	quotaUntil := now.Add(23 * time.Minute)
	credential := &coreauth.Auth{
		Status: coreauth.StatusActive,
		ModelStates: map[string]*coreauth.ModelState{
			"available-model": {Status: coreauth.StatusActive},
			"cooldown-model": {
				Status:         coreauth.StatusError,
				Unavailable:    true,
				NextRetryAfter: cooldownUntil,
			},
			"cooldown-without-reset-model": {
				Status:      coreauth.StatusActive,
				Unavailable: true,
			},
			"exhausted-model": {
				Status:         coreauth.StatusError,
				Unavailable:    true,
				NextRetryAfter: quotaUntil,
				Quota: coreauth.QuotaState{
					Exceeded:      true,
					NextRecoverAt: quotaUntil,
				},
			},
			"unknown-model": {
				Status: coreauth.StatusError,
			},
			"nil-model": nil,
		},
	}

	tests := []struct {
		model       string
		wantState   string
		wantReset   time.Time
		wantWindows int
		wantLabel   string
	}{
		{model: "available-model", wantState: "available"},
		{model: "cooldown-model", wantState: "cooldown", wantReset: cooldownUntil, wantWindows: 1, wantLabel: "model-runtime-cooldown"},
		{model: "cooldown-without-reset-model", wantState: "cooldown", wantWindows: 1, wantLabel: "model-runtime-cooldown"},
		{model: "exhausted-model", wantState: "exhausted", wantReset: quotaUntil, wantWindows: 1, wantLabel: "model-runtime-quota"},
		{model: "unknown-model", wantState: "unknown"},
		{model: "nil-model", wantState: "unknown"},
	}
	for _, testCase := range tests {
		t.Run(testCase.model, func(t *testing.T) {
			projection := facadeUsageForModel(credential, testCase.model, now)
			if projection.State != testCase.wantState {
				t.Fatalf("state=%q want=%q", projection.State, testCase.wantState)
			}
			if projection.RemainingPercent != nil {
				t.Fatalf("runtime cooldown/quota state fabricated percentage=%v", *projection.RemainingPercent)
			}
			if len(projection.Windows) != testCase.wantWindows {
				t.Fatalf("windows=%d want=%d: %+v", len(projection.Windows), testCase.wantWindows, projection.Windows)
			}
			if testCase.wantWindows > 0 {
				window := projection.Windows[0]
				if window.Label != testCase.wantLabel || window.Limit != nil || window.Used != nil || window.RemainingPercent != nil {
					t.Fatalf("runtime evidence window fabricated structured quota: %+v", window)
				}
				if projection.LimitingWindowLabel == nil || *projection.LimitingWindowLabel != testCase.wantLabel {
					t.Fatalf("limitingWindowLabel=%v want=%q", projection.LimitingWindowLabel, testCase.wantLabel)
				}
				if testCase.wantReset.IsZero() {
					if projection.ResetsAt != nil || window.ResetsAt != nil {
						t.Fatalf("reset projection=%v window=%v want=nil", projection.ResetsAt, window.ResetsAt)
					}
				} else if projection.ResetsAt == nil || window.ResetsAt == nil || *projection.ResetsAt != testCase.wantReset.Format(time.RFC3339Nano) || *window.ResetsAt != *projection.ResetsAt {
					t.Fatalf("reset projection=%v window=%v want=%s", projection.ResetsAt, window.ResetsAt, testCase.wantReset.Format(time.RFC3339Nano))
				}
			} else if projection.ResetsAt != nil || projection.LimitingWindowLabel != nil {
				t.Fatalf("unexpected reset/limiting label: reset=%v label=%v", projection.ResetsAt, projection.LimitingWindowLabel)
			}
		})
	}

	accountQuota := &coreauth.Auth{
		Status: coreauth.StatusError,
		Quota: coreauth.QuotaState{
			Exceeded:      true,
			NextRecoverAt: quotaUntil,
		},
	}
	projection := facadeUsageForModel(accountQuota, "route-without-model-state", now)
	if projection.State != "exhausted" || len(projection.Windows) != 1 || projection.Windows[0].Label != "account-runtime-quota" {
		t.Fatalf("account fallback projection=%+v", projection)
	}
}

func TestAccountBridgeUsageForceRefreshRebuildsLiveRuntimeSnapshotAndRejectsAmbiguity(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "test-management-key")
	runtime := newAccountBridgeRuntimeForHTTPTest(t, accountbridge.ConnectorPolicy{
		ConnectorID:           accountbridge.ConnectorOpenAI,
		FeatureEnabled:        true,
		TermsStatus:           "signed-off",
		ConformancePassed:     true,
		FixedArtifactVerified: true,
	})
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))
	credential := &coreauth.Auth{
		ID:        "force-refresh-private-auth",
		Provider:  "codex",
		Status:    coreauth.StatusActive,
		CreatedAt: time.Now().UTC(),
	}
	if _, err := runtime.AuthManager().Register(context.Background(), credential); err != nil {
		t.Fatalf("register auth: %v", err)
	}
	modelID := "force-refresh-runtime-model"
	registry.GetGlobalRegistry().RegisterClient(credential.ID, credential.Provider, []*registry.ModelInfo{{ID: modelID}})
	t.Cleanup(func() { registry.GetGlobalRegistry().UnregisterClient(credential.ID) })

	initial := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/usage?forceRefresh=false", nil, true)
	var initialPayload struct {
		Snapshots []facadeUsage `json:"snapshots"`
	}
	if initial.Code != http.StatusOK || json.Unmarshal(initial.Body.Bytes(), &initialPayload) != nil || len(initialPayload.Snapshots) != 1 || initialPayload.Snapshots[0].State != "available" {
		t.Fatalf("initial usage status=%d body=%s", initial.Code, initial.Body.String())
	}

	quotaUntil := time.Now().UTC().Add(15 * time.Minute)
	credential.Status = coreauth.StatusError
	credential.ModelStates = map[string]*coreauth.ModelState{
		modelID: {
			Status:         coreauth.StatusError,
			Unavailable:    true,
			NextRetryAfter: quotaUntil,
			Quota: coreauth.QuotaState{
				Exceeded:      true,
				NextRecoverAt: quotaUntil,
			},
		},
	}
	if _, err := runtime.AuthManager().Update(context.Background(), credential); err != nil {
		t.Fatalf("update runtime state: %v", err)
	}
	refreshed := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/usage?forceRefresh=true", nil, true)
	var refreshedPayload struct {
		Snapshots []facadeUsage `json:"snapshots"`
	}
	if refreshed.Code != http.StatusOK || json.Unmarshal(refreshed.Body.Bytes(), &refreshedPayload) != nil || len(refreshedPayload.Snapshots) != 1 {
		t.Fatalf("refreshed usage status=%d body=%s", refreshed.Code, refreshed.Body.String())
	}
	snapshot := refreshedPayload.Snapshots[0]
	if snapshot.State != "exhausted" || snapshot.ResetsAt == nil || snapshot.LimitingWindowLabel == nil || *snapshot.LimitingWindowLabel != "model-runtime-quota" || len(snapshot.Windows) != 1 || snapshot.RemainingPercent != nil {
		t.Fatalf("force-refreshed runtime snapshot=%+v", snapshot)
	}
	if snapshot.ObservedAt == initialPayload.Snapshots[0].ObservedAt {
		t.Fatalf("force refresh reused observedAt=%q", snapshot.ObservedAt)
	}

	for _, path := range []string{
		"/v0/account-bridge/internal/usage?forceRefresh=1",
		"/v0/account-bridge/internal/usage?forceRefresh=true&forceRefresh=false",
	} {
		invalid := accountBridgeHTTP(t, server, http.MethodGet, path, nil, true)
		if invalid.Code != http.StatusBadRequest || !strings.Contains(invalid.Body.String(), "invalid_force_refresh") {
			t.Fatalf("invalid forceRefresh path=%s status=%d body=%s", path, invalid.Code, invalid.Body.String())
		}
	}
}

func TestAccountBridgeUsageMergesProviderQuotaWithModelCooldownAndForwardsForceRefresh(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "test-management-key")
	runtime := newAccountBridgeRuntimeForHTTPTest(t, accountbridge.ConnectorPolicy{
		ConnectorID:           accountbridge.ConnectorOpenAI,
		FeatureEnabled:        true,
		TermsStatus:           "signed-off",
		ConformancePassed:     true,
		FixedArtifactVerified: true,
	})
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))
	cooldownUntil := time.Now().UTC().Add(11 * time.Minute)
	credential := &coreauth.Auth{
		ID:       "provider-quota-private-auth",
		Provider: "codex",
		Status:   coreauth.StatusActive,
		ModelStates: map[string]*coreauth.ModelState{
			"quota-available-model": {Status: coreauth.StatusActive},
			"quota-cooldown-model": {
				Status:         coreauth.StatusError,
				Unavailable:    true,
				NextRetryAfter: cooldownUntil,
			},
		},
		CreatedAt: time.Now().UTC(),
	}
	if _, err := runtime.AuthManager().Register(context.Background(), credential); err != nil {
		t.Fatalf("register auth: %v", err)
	}
	registry.GetGlobalRegistry().RegisterClient(credential.ID, credential.Provider, []*registry.ModelInfo{
		{ID: "quota-available-model"},
		{ID: "quota-cooldown-model"},
	})
	t.Cleanup(func() { registry.GetGlobalRegistry().UnregisterClient(credential.ID) })

	routesResponse := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/routes", nil, true)
	var routesPayload struct {
		Routes []facadeModel `json:"routes"`
	}
	if routesResponse.Code != http.StatusOK || json.Unmarshal(routesResponse.Body.Bytes(), &routesPayload) != nil || len(routesPayload.Routes) != 2 {
		t.Fatalf("routes status=%d body=%s", routesResponse.Code, routesResponse.Body.String())
	}
	routeByModel := make(map[string]string, len(routesPayload.Routes))
	for _, route := range routesPayload.Routes {
		routeByModel[route.ModelID] = route.RouteID
	}

	remaining := 42.0
	label := "weekly"
	providerReset := time.Date(2026, 7, 20, 0, 0, 0, 0, time.UTC)
	providerObserved := time.Date(2026, 7, 14, 18, 0, 0, 0, time.UTC)
	reader := &fakeAccountBridgeQuotaReader{report: accountbridgequota.Report{Account: &accountbridgequota.Snapshot{
		State:               "available",
		RemainingPercent:    &remaining,
		LimitingWindowLabel: &label,
		ResetsAt:            &providerReset,
		ObservedAt:          providerObserved,
		Windows: []accountbridgequota.Window{{
			Label:            label,
			RemainingPercent: &remaining,
			ResetsAt:         &providerReset,
		}},
	}}}
	server.accountBridgeQuota = reader

	usageResponse := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/usage?forceRefresh=false", nil, true)
	var usagePayload struct {
		Snapshots []facadeUsage `json:"snapshots"`
	}
	if usageResponse.Code != http.StatusOK || json.Unmarshal(usageResponse.Body.Bytes(), &usagePayload) != nil || len(usagePayload.Snapshots) != 2 {
		t.Fatalf("usage status=%d body=%s", usageResponse.Code, usageResponse.Body.String())
	}
	if len(reader.calls) != 1 || reader.calls[0] {
		t.Fatalf("quota calls=%v want one non-force account fetch", reader.calls)
	}
	for _, snapshot := range usagePayload.Snapshots {
		if snapshot.RemainingPercent == nil || *snapshot.RemainingPercent != remaining || snapshot.ObservedAt != providerObserved.Format(time.RFC3339Nano) {
			t.Fatalf("provider projection missing: %+v", snapshot)
		}
		switch snapshot.RouteID {
		case routeByModel["quota-available-model"]:
			if snapshot.State != "available" || len(snapshot.Windows) != 1 || snapshot.LimitingWindowLabel == nil || *snapshot.LimitingWindowLabel != label {
				t.Fatalf("available snapshot=%+v", snapshot)
			}
		case routeByModel["quota-cooldown-model"]:
			if snapshot.State != "cooldown" || len(snapshot.Windows) != 2 || snapshot.LimitingWindowLabel == nil || *snapshot.LimitingWindowLabel != label || snapshot.ResetsAt == nil || *snapshot.ResetsAt != providerReset.Format(time.RFC3339Nano) || snapshot.Windows[1].ResetsAt == nil || *snapshot.Windows[1].ResetsAt != cooldownUntil.Format(time.RFC3339Nano) {
				t.Fatalf("cooldown snapshot=%+v", snapshot)
			}
		default:
			t.Fatalf("unexpected route in usage: %+v", snapshot)
		}
	}

	filteredPath := "/v0/account-bridge/internal/usage?forceRefresh=true&routeId=" + routeByModel["quota-available-model"]
	filteredResponse := accountBridgeHTTP(t, server, http.MethodGet, filteredPath, nil, true)
	if filteredResponse.Code != http.StatusOK {
		t.Fatalf("filtered force refresh status=%d body=%s", filteredResponse.Code, filteredResponse.Body.String())
	}
	if len(reader.calls) != 2 || !reader.calls[1] {
		t.Fatalf("quota calls=%v want forceRefresh forwarded", reader.calls)
	}
}

func TestAccountBridgeUsageProviderFailureFallsBackWithoutFabricatingPercent(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "test-management-key")
	runtime := newAccountBridgeRuntimeForHTTPTest(t, accountbridge.ConnectorPolicy{
		ConnectorID:           accountbridge.ConnectorXAI,
		FeatureEnabled:        true,
		TermsStatus:           "signed-off",
		ConformancePassed:     true,
		FixedArtifactVerified: true,
	})
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))
	cooldownUntil := time.Now().UTC().Add(9 * time.Minute)
	credential := &coreauth.Auth{
		ID:             "quota-failure-auth",
		Provider:       "xai",
		Status:         coreauth.StatusError,
		Unavailable:    true,
		NextRetryAfter: cooldownUntil,
		CreatedAt:      time.Now().UTC(),
	}
	if _, err := runtime.AuthManager().Register(context.Background(), credential); err != nil {
		t.Fatalf("register auth: %v", err)
	}
	registry.GetGlobalRegistry().RegisterClient(credential.ID, credential.Provider, []*registry.ModelInfo{{ID: "grok-failure-model"}})
	t.Cleanup(func() { registry.GetGlobalRegistry().UnregisterClient(credential.ID) })
	reader := &fakeAccountBridgeQuotaReader{err: accountbridgequota.ErrMalformedResponse}
	server.accountBridgeQuota = reader

	response := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/usage?forceRefresh=true", nil, true)
	var payload struct {
		Snapshots []facadeUsage `json:"snapshots"`
	}
	if response.Code != http.StatusOK || json.Unmarshal(response.Body.Bytes(), &payload) != nil || len(payload.Snapshots) != 1 {
		t.Fatalf("usage status=%d body=%s", response.Code, response.Body.String())
	}
	snapshot := payload.Snapshots[0]
	if snapshot.State != "cooldown" || snapshot.RemainingPercent != nil || len(snapshot.Windows) != 1 || snapshot.Windows[0].RemainingPercent != nil || snapshot.ResetsAt == nil {
		t.Fatalf("provider failure did not fail safe to runtime evidence: %+v", snapshot)
	}
}

func TestMergeFacadeUsageRuntimeExhaustionSuppressesStalePositiveTopLevelPercent(t *testing.T) {
	runtimeReset := "2026-07-14T19:00:00Z"
	runtimeLabel := "model-runtime-quota"
	runtime := facadeUsage{
		State:               "exhausted",
		LimitingWindowLabel: &runtimeLabel,
		ResetsAt:            &runtimeReset,
		Windows: []facadeUsageWindow{{
			Label:    runtimeLabel,
			ResetsAt: &runtimeReset,
		}},
	}
	remaining := 63.0
	providerLabel := "weekly"
	provider := accountbridgequota.Snapshot{
		State:               "available",
		RemainingPercent:    &remaining,
		LimitingWindowLabel: &providerLabel,
		Windows: []accountbridgequota.Window{{
			Label:            providerLabel,
			RemainingPercent: &remaining,
		}},
	}
	merged := mergeFacadeUsageWithProvider(runtime, provider)
	if merged.State != "exhausted" || merged.RemainingPercent != nil || merged.LimitingWindowLabel == nil || *merged.LimitingWindowLabel != runtimeLabel || len(merged.Windows) != 2 {
		t.Fatalf("merged exhaustion=%+v", merged)
	}
}

func TestMergeFacadeUsageDoesNotFabricateRoutePercentForUnboundProviderLimit(t *testing.T) {
	remaining := 60.0
	report := accountbridgequota.Report{
		Account: &accountbridgequota.Snapshot{
			State:            "available",
			RemainingPercent: &remaining,
			Windows: []accountbridgequota.Window{{
				Label:            "primary",
				RemainingPercent: &remaining,
			}},
		},
		UnboundLimits: []accountbridgequota.UnboundLimitSnapshot{{
			LimitID: "codex_other",
			Snapshot: accountbridgequota.Snapshot{
				State: "available",
				Windows: []accountbridgequota.Window{{
					Label:            "primary",
					RemainingPercent: &remaining,
				}},
			},
		}},
	}
	provider, ok := report.ForModel("model-alpha")
	if !ok {
		t.Fatal("provider report did not project a route")
	}
	merged := mergeFacadeUsageWithProvider(facadeUsage{State: "available"}, provider)
	if merged.State != "unknown" || merged.RemainingPercent != nil || merged.LimitingWindowLabel != nil || len(merged.Windows) != 2 || merged.Windows[1].Label != "unbound-provider-limit-1-primary" || merged.Windows[1].RemainingPercent == nil || *merged.Windows[1].RemainingPercent != remaining {
		t.Fatalf("unbound provider limit fabricated a route balance: %+v", merged)
	}
}

func TestAccountBridgeUsageFiltersRoutesBeforeProviderFetch(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "test-management-key")
	runtime := newAccountBridgeRuntimeForHTTPTest(t, accountbridge.ConnectorPolicy{
		ConnectorID:           accountbridge.ConnectorOpenAI,
		FeatureEnabled:        true,
		TermsStatus:           "signed-off",
		ConformancePassed:     true,
		FixedArtifactVerified: true,
	})
	server := newTestServerWithOptions(t, WithAccountBridgeRuntime(runtime))
	credentials := []*coreauth.Auth{
		{ID: "filtered-provider-auth-a", Provider: "codex", Status: coreauth.StatusActive, CreatedAt: time.Now().UTC()},
		{ID: "filtered-provider-auth-b", Provider: "codex", Status: coreauth.StatusActive, CreatedAt: time.Now().UTC()},
	}
	for index, credential := range credentials {
		if _, err := runtime.AuthManager().Register(context.Background(), credential); err != nil {
			t.Fatalf("register auth %d: %v", index, err)
		}
		registry.GetGlobalRegistry().RegisterClient(credential.ID, credential.Provider, []*registry.ModelInfo{{ID: "filtered-model-" + string(rune('a'+index))}})
		t.Cleanup(func() { registry.GetGlobalRegistry().UnregisterClient(credential.ID) })
	}
	routesResponse := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/routes", nil, true)
	var routesPayload struct {
		Routes []facadeModel `json:"routes"`
	}
	if routesResponse.Code != http.StatusOK || json.Unmarshal(routesResponse.Body.Bytes(), &routesPayload) != nil || len(routesPayload.Routes) != 2 {
		t.Fatalf("routes status=%d body=%s", routesResponse.Code, routesResponse.Body.String())
	}
	selectedRoute := routesPayload.Routes[0].RouteID
	selectedBinding, err := runtime.Routes().Resolve(selectedRoute)
	if err != nil {
		t.Fatalf("resolve selected route: %v", err)
	}
	remaining := 50.0
	reader := &fakeAccountBridgeQuotaReader{report: accountbridgequota.Report{Account: &accountbridgequota.Snapshot{
		State: "available", RemainingPercent: &remaining,
	}}}
	server.accountBridgeQuota = reader

	response := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/usage?routeId="+selectedRoute, nil, true)
	if response.Code != http.StatusOK {
		t.Fatalf("filtered usage status=%d body=%s", response.Code, response.Body.String())
	}
	reader.mu.Lock()
	defer reader.mu.Unlock()
	if len(reader.authIDs) != 1 || reader.authIDs[0] != selectedBinding.AuthID {
		t.Fatalf("provider auth fetches=%v want only %q", reader.authIDs, selectedBinding.AuthID)
	}
}

// newAccountBridgeCNv2RuntimeForHTTPTest builds a runtime from a
// second-generation CN grant whose signed membership names only the
// qwen/kimi/zai connectors, paired with the full seven-entry policy
// generation. The qwen and zai entries pass every independent gate with a
// global region policy; kimi passes the gates but keeps the non-cn region
// policy so the per-connector CN floor applies to it.
func newAccountBridgeCNv2RuntimeForHTTPTest(t *testing.T) *accountbridge.Runtime {
	t.Helper()
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("generate eligibility key: %v", err)
	}
	client := accountbridge.EligibilityClientBinding{
		RequestNonce:                  base64.RawURLEncoding.EncodeToString(bytes.Repeat([]byte{0x62}, 32)),
		CrabCodeRelease:               "1.0.17",
		AccountBridgeComponentVersion: "7.2.71-crabcode.5",
		AccountBridgeProtocolVersion:  accountbridge.ProtocolVersion,
	}
	verifier, err := accountbridge.NewEligibilityVerifier(publicKey, "test-account-bridge", "v1", client)
	if err != nil {
		t.Fatalf("create verifier: %v", err)
	}
	now := time.Now().UTC().Unix()
	payload, err := json.Marshal(accountbridge.EligibilityPayload{
		Audience: "test-account-bridge",
		Version:  "v2",
		Client:   client,
		AllowedClientVersions: accountbridge.AllowedClientVersions{
			CrabCodeRelease:               accountbridge.InclusiveVersionRange{MinimumInclusive: client.CrabCodeRelease, MaximumInclusive: client.CrabCodeRelease},
			AccountBridgeComponentVersion: accountbridge.InclusiveVersionRange{MinimumInclusive: client.AccountBridgeComponentVersion, MaximumInclusive: client.AccountBridgeComponentVersion},
			AccountBridgeProtocolVersion:  accountbridge.InclusiveProtocolRange{MinimumInclusive: client.AccountBridgeProtocolVersion, MaximumInclusive: client.AccountBridgeProtocolVersion},
		},
		PolicyVersion: "test-policy-v2",
		IssuedAt:      now - 1,
		ExpiresAt:     now + 120,
		CountryCode:   "CN",
		RegionAllowed: false,
		ConnectorIDs:  []string{accountbridge.ConnectorQwen, accountbridge.ConnectorKimi, accountbridge.ConnectorZai},
	})
	if err != nil {
		t.Fatalf("marshal grant: %v", err)
	}
	encoding := base64.RawURLEncoding
	grant := accountbridge.SignedEligibilityGrant{
		PayloadBase64URL:   encoding.EncodeToString(payload),
		SignatureBase64URL: encoding.EncodeToString(ed25519.Sign(privateKey, payload)),
	}
	routes, err := accountbridge.NewRouteStore(filepath.Join(t.TempDir(), "route.seed"))
	if err != nil {
		t.Fatalf("create route store: %v", err)
	}
	enabled := func(connectorID, displayName, regionPolicy string) accountbridge.ConnectorPolicy {
		return accountbridge.ConnectorPolicy{
			ConnectorID:           connectorID,
			DisplayName:           displayName,
			AuthMode:              accountbridge.AuthModeDeviceCode,
			FeatureEnabled:        true,
			TermsStatus:           "signed-off",
			ConformancePassed:     true,
			FixedArtifactVerified: true,
			RegionPolicy:          regionPolicy,
		}
	}
	directoryPolicies := []accountbridge.ConnectorPolicy{
		{ConnectorID: accountbridge.ConnectorOpenAI, DisplayName: "Directory OpenAI", AuthMode: accountbridge.AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
		{ConnectorID: accountbridge.ConnectorAnthropic, DisplayName: "Directory Anthropic", AuthMode: accountbridge.AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
		{ConnectorID: accountbridge.ConnectorGoogle, DisplayName: "Directory Google", AuthMode: accountbridge.AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
		{ConnectorID: accountbridge.ConnectorXAI, DisplayName: "Directory xAI", AuthMode: accountbridge.AuthModeDeviceCode, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
		enabled(accountbridge.ConnectorQwen, "Directory Qwen", accountbridge.RegionPolicyGlobal),
		enabled(accountbridge.ConnectorKimi, "Directory Kimi", accountbridge.RegionPolicyNonCN),
		enabled(accountbridge.ConnectorZai, "Directory Z.AI", accountbridge.RegionPolicyGlobal),
	}
	runtime, err := accountbridge.NewRuntimeWithConnectorPolicies(verifier, grant, routes, directoryPolicies)
	if err != nil {
		t.Fatalf("create CN v2 runtime: %v", err)
	}
	return runtime
}

// newCNv2TestServer constructs a server around the CN v2 runtime whose
// outbound HTTP goes through a guaranteed-refused loopback proxy, so provider
// login handlers fail fast and deterministically instead of touching the
// network.
func newCNv2TestServer(t *testing.T, runtime *accountbridge.Runtime) *Server {
	t.Helper()
	gin.SetMode(gin.TestMode)

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("reserve refused port: %v", err)
	}
	refusedProxy := fmt.Sprintf("http://%s", listener.Addr().String())
	_ = listener.Close()

	tmpDir := t.TempDir()
	authDir := filepath.Join(tmpDir, "auth")
	if err := os.MkdirAll(authDir, 0o700); err != nil {
		t.Fatalf("failed to create auth dir: %v", err)
	}
	cfg := &proxyconfig.Config{
		SDKConfig: sdkconfig.SDKConfig{
			APIKeys:  []string{"test-key"},
			ProxyURL: refusedProxy,
		},
		Port:                   0,
		AuthDir:                authDir,
		Debug:                  true,
		LoggingToFile:          false,
		UsageStatisticsEnabled: false,
	}
	authManager := coreauth.NewManager(nil, nil, nil)
	accessManager := sdkaccess.NewManager()
	configPath := filepath.Join(tmpDir, "config.yaml")
	return NewServer(cfg, authManager, accessManager, configPath, WithAccountBridgeRuntime(runtime))
}

func TestAccountBridgeCNv2FacadeGatesComposePerConnector(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "test-management-key")
	runtime := newAccountBridgeCNv2RuntimeForHTTPTest(t)
	server := newCNv2TestServer(t, runtime)

	// Boot-level facade access works from a CN second-generation grant.
	connectors := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/connectors", nil, true)
	if connectors.Code != http.StatusOK {
		t.Fatalf("connectors status=%d body=%s", connectors.Code, connectors.Body.String())
	}
	var catalog struct {
		Connectors []facadeConnector `json:"connectors"`
	}
	if err := json.Unmarshal(connectors.Body.Bytes(), &catalog); err != nil {
		t.Fatalf("decode connectors: %v", err)
	}
	if len(catalog.Connectors) != 7 {
		t.Fatalf("connectors=%d, want 7", len(catalog.Connectors))
	}
	for _, connector := range catalog.Connectors {
		switch connector.ConnectorID {
		case accountbridge.ConnectorQwen, accountbridge.ConnectorZai:
			if !connector.Enabled {
				t.Fatalf("global connector must be enabled inside CN: %+v", connector)
			}
		case accountbridge.ConnectorKimi:
			// Signed membership plus signed-off gates, but the non-cn region
			// policy keeps the CN floor.
			if connector.Enabled || connector.DisabledReason == nil || *connector.DisabledReason != "connector_not_eligible" {
				t.Fatalf("non-cn connector must stay floored inside CN: %+v", connector)
			}
		default:
			if connector.Enabled || connector.DisabledReason == nil || *connector.DisabledReason != "connector_not_eligible" {
				t.Fatalf("legacy connector outside the CN membership must be ineligible: %+v", connector)
			}
		}
	}

	// Legacy connectors are rejected by the membership gate.
	openaiStart := accountBridgeHTTP(t, server, http.MethodPost, "/v0/account-bridge/internal/login/start", []byte(`{"connectorId":"openai"}`), true)
	if openaiStart.Code != http.StatusForbidden || !strings.Contains(openaiStart.Body.String(), "connector_not_eligible") {
		t.Fatalf("openai login start status=%d body=%s", openaiStart.Code, openaiStart.Body.String())
	}
	// The CN floor rejects kimi before any provider dispatch.
	kimiStart := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/login/kimi/start", nil, true)
	if kimiStart.Code != http.StatusForbidden || !strings.Contains(kimiStart.Body.String(), "connector_not_eligible") {
		t.Fatalf("kimi login start status=%d body=%s", kimiStart.Code, kimiStart.Body.String())
	}
	// Qwen passes every gate and reaches the provider handler. The refused
	// proxy fails the upstream device-code request deterministically, so the
	// dispatch evidence is the handler's own 500 error body rather than any
	// gate rejection.
	qwenStart := accountBridgeHTTP(t, server, http.MethodPost, "/v0/account-bridge/internal/login/start", []byte(`{"connectorId":"qwen"}`), true)
	if qwenStart.Code == http.StatusForbidden || strings.Contains(qwenStart.Body.String(), "connector_not_eligible") || strings.Contains(qwenStart.Body.String(), "connector_disabled") {
		t.Fatalf("qwen login start must clear the eligibility gates: status=%d body=%s", qwenStart.Code, qwenStart.Body.String())
	}
	if qwenStart.Code != http.StatusInternalServerError || !strings.Contains(qwenStart.Body.String(), "failed to generate authorization url") {
		t.Fatalf("qwen login start must reach the provider handler: status=%d body=%s", qwenStart.Code, qwenStart.Body.String())
	}
	// The dedicated per-connector route composes identically for zai.
	zaiStart := accountBridgeHTTP(t, server, http.MethodGet, "/v0/account-bridge/internal/login/zai/start", nil, true)
	if zaiStart.Code == http.StatusForbidden || strings.Contains(zaiStart.Body.String(), "connector_not_eligible") {
		t.Fatalf("zai login start must clear the eligibility gates: status=%d body=%s", zaiStart.Code, zaiStart.Body.String())
	}
	if zaiStart.Code != http.StatusInternalServerError || !strings.Contains(zaiStart.Body.String(), "failed to generate authorization url") {
		t.Fatalf("zai login start must reach the provider handler: status=%d body=%s", zaiStart.Code, zaiStart.Body.String())
	}
}
