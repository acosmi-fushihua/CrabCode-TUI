package accountbridge

import (
	"crypto/ed25519"
	"crypto/rand"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func validConnectorPolicyDirectoryForTest() []ConnectorPolicy {
	return []ConnectorPolicy{
		{ConnectorID: ConnectorOpenAI, DisplayName: "Directory OpenAI", AuthMode: AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: RegionPolicyNonCN},
		{ConnectorID: ConnectorAnthropic, DisplayName: "Directory Anthropic", AuthMode: AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: RegionPolicyNonCN},
		{ConnectorID: ConnectorGoogle, DisplayName: "Directory Google", AuthMode: AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: RegionPolicyNonCN},
		{ConnectorID: ConnectorXAI, DisplayName: "Directory xAI", AuthMode: AuthModeDeviceCode, TermsStatus: "blocked", RegionPolicy: RegionPolicyNonCN},
	}
}

// fullConnectorPolicyDirectoryForTest returns the second-generation directory:
// the legacy four entries plus the qwen/kimi/zai connectors.
func fullConnectorPolicyDirectoryForTest() []ConnectorPolicy {
	return append(validConnectorPolicyDirectoryForTest(),
		ConnectorPolicy{ConnectorID: ConnectorQwen, DisplayName: "Directory Qwen", AuthMode: AuthModeDeviceCode, TermsStatus: "blocked", RegionPolicy: RegionPolicyGlobal},
		ConnectorPolicy{ConnectorID: ConnectorKimi, DisplayName: "Directory Kimi", AuthMode: AuthModeDeviceCode, TermsStatus: "blocked", RegionPolicy: RegionPolicyGlobal},
		ConnectorPolicy{ConnectorID: ConnectorZai, DisplayName: "Directory Z.AI", AuthMode: AuthModeDeviceCode, TermsStatus: "blocked", RegionPolicy: RegionPolicyGlobal},
	)
}

func enabledConnectorPolicy(connectorID, displayName, authMode, regionPolicy string) ConnectorPolicy {
	return ConnectorPolicy{
		ConnectorID:           connectorID,
		DisplayName:           displayName,
		AuthMode:              authMode,
		FeatureEnabled:        true,
		TermsStatus:           "signed-off",
		ConformancePassed:     true,
		FixedArtifactVerified: true,
		RegionPolicy:          regionPolicy,
	}
}

func TestConnectorPoliciesRequireEveryIndependentGate(t *testing.T) {
	now := time.Unix(1_800_000_000, 0).UTC()
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("GenerateKey: %v", err)
	}
	verifier := mustVerifier(t, publicKey, now)
	grant := signEligibilityPayload(t, privateKey, validEligibilityPayload(now))
	routes, err := NewRouteStore(filepath.Join(t.TempDir(), "route-seed"))
	if err != nil {
		t.Fatalf("NewRouteStore: %v", err)
	}

	tests := []struct {
		name   string
		policy ConnectorPolicy
		want   bool
		reason string
	}{
		{
			name:   "all gates",
			policy: ConnectorPolicy{ConnectorID: ConnectorOpenAI, FeatureEnabled: true, TermsStatus: "signed-off", ConformancePassed: true, FixedArtifactVerified: true},
			want:   true,
		},
		{
			name:   "feature flag",
			policy: ConnectorPolicy{ConnectorID: ConnectorOpenAI, TermsStatus: "signed-off", ConformancePassed: true, FixedArtifactVerified: true},
			reason: "feature_flag_disabled",
		},
		{
			name:   "terms",
			policy: ConnectorPolicy{ConnectorID: ConnectorOpenAI, FeatureEnabled: true, TermsStatus: "blocked", ConformancePassed: true, FixedArtifactVerified: true},
			reason: "terms_not_signed_off",
		},
		{
			name:   "conformance",
			policy: ConnectorPolicy{ConnectorID: ConnectorOpenAI, FeatureEnabled: true, TermsStatus: "signed-off", FixedArtifactVerified: true},
			reason: "conformance_not_passed",
		},
		{
			name:   "artifact",
			policy: ConnectorPolicy{ConnectorID: ConnectorOpenAI, FeatureEnabled: true, TermsStatus: "signed-off", ConformancePassed: true},
			reason: "fixed_artifact_unverified",
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			policies := validConnectorPolicyDirectoryForTest()
			test.policy.DisplayName = "Signed OpenAI Label"
			test.policy.AuthMode = AuthModeBrowser
			test.policy.RegionPolicy = RegionPolicyNonCN
			policies[0] = test.policy
			runtime, runtimeErr := NewRuntimeWithConnectorPolicies(verifier, grant, routes, policies)
			if runtimeErr != nil {
				t.Fatalf("NewRuntimeWithConnectorPolicies: %v", runtimeErr)
			}
			if got := runtime.ConnectorEnabled(ConnectorOpenAI); got != test.want {
				t.Fatalf("ConnectorEnabled()=%t, want %t", got, test.want)
			}
			if got := runtime.ConnectorDisabledReason(ConnectorOpenAI); got != test.reason {
				t.Fatalf("ConnectorDisabledReason()=%q, want %q", got, test.reason)
			}
			if got := runtime.ConnectorDisplayName(ConnectorOpenAI); got != "Signed OpenAI Label" {
				t.Fatalf("ConnectorDisplayName()=%q", got)
			}
			if got := runtime.ConnectorAuthMode(ConnectorOpenAI); got != AuthModeBrowser {
				t.Fatalf("ConnectorAuthMode()=%q", got)
			}
		})
	}
}

func TestConnectorPolicyDisableIsScopedToOneConnector(t *testing.T) {
	now := time.Unix(1_800_000_000, 0).UTC()
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("GenerateKey: %v", err)
	}
	verifier := mustVerifier(t, publicKey, now)
	grant := signEligibilityPayload(t, privateKey, validEligibilityPayload(now))
	routes, err := NewRouteStore(filepath.Join(t.TempDir(), "route-seed"))
	if err != nil {
		t.Fatalf("NewRouteStore: %v", err)
	}
	policies := validConnectorPolicyDirectoryForTest()
	policies[0] = ConnectorPolicy{
		ConnectorID:           ConnectorOpenAI,
		DisplayName:           "OpenAI",
		AuthMode:              AuthModeBrowser,
		FeatureEnabled:        false,
		TermsStatus:           "signed-off",
		ConformancePassed:     true,
		FixedArtifactVerified: true,
		RegionPolicy:          RegionPolicyNonCN,
	}
	policies[1] = ConnectorPolicy{
		ConnectorID:           ConnectorAnthropic,
		DisplayName:           "Anthropic",
		AuthMode:              AuthModeBrowser,
		FeatureEnabled:        true,
		TermsStatus:           "signed-off",
		ConformancePassed:     true,
		FixedArtifactVerified: true,
		RegionPolicy:          RegionPolicyNonCN,
	}
	runtime, err := NewRuntimeWithConnectorPolicies(verifier, grant, routes, policies)
	if err != nil {
		t.Fatalf("NewRuntimeWithConnectorPolicies: %v", err)
	}
	if runtime.ConnectorEnabled(ConnectorOpenAI) {
		t.Fatal("connector-scoped disable unexpectedly enabled OpenAI")
	}
	if !runtime.ConnectorEnabled(ConnectorAnthropic) {
		t.Fatal("disabling OpenAI unexpectedly disabled Anthropic")
	}
}

func TestConnectorPoliciesRejectInvalidExactSetAndMetadata(t *testing.T) {
	now := time.Unix(1_800_000_000, 0).UTC()
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("GenerateKey: %v", err)
	}
	verifier := mustVerifier(t, publicKey, now)
	grant := signEligibilityPayload(t, privateKey, validEligibilityPayload(now))
	routes, err := NewRouteStore(filepath.Join(t.TempDir(), "route-seed"))
	if err != nil {
		t.Fatalf("NewRouteStore: %v", err)
	}
	tests := []struct {
		name   string
		mutate func([]ConnectorPolicy) []ConnectorPolicy
	}{
		{name: "incomplete", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy { return policies[:3] }},
		{name: "unknown", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[0].ConnectorID = "unknown"
			return policies
		}},
		{name: "non-canonical connector id", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[0].ConnectorID = "OpenAI"
			return policies
		}},
		{name: "duplicate", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[3].ConnectorID = ConnectorOpenAI
			return policies
		}},
		{name: "invalid terms", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[0].TermsStatus = "not-reviewed"
			return policies
		}},
		{name: "empty display name", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy { policies[0].DisplayName = ""; return policies }},
		{name: "padded display name", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[0].DisplayName = " OpenAI"
			return policies
		}},
		{name: "control display name", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[0].DisplayName = "Open\nAI"
			return policies
		}},
		{name: "format-control display name", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[0].DisplayName = "Open\u202eAI"
			return policies
		}},
		{name: "overlong display name", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[0].DisplayName = strings.Repeat("A", 81)
			return policies
		}},
		{name: "invalid auth mode", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy { policies[0].AuthMode = "redirect"; return policies }},
		{name: "missing region policy", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy { policies[0].RegionPolicy = ""; return policies }},
		{name: "invalid region policy", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[0].RegionPolicy = "cn-only"
			return policies
		}},
		{name: "non-canonical region policy", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[0].RegionPolicy = "Global"
			return policies
		}},
		{name: "padded region policy", mutate: func(policies []ConnectorPolicy) []ConnectorPolicy {
			policies[0].RegionPolicy = " global"
			return policies
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			policies := test.mutate(validConnectorPolicyDirectoryForTest())
			if _, runtimeErr := NewRuntimeWithConnectorPolicies(verifier, grant, routes, policies); runtimeErr == nil {
				t.Fatalf("policies=%+v unexpectedly accepted", policies)
			}
		})
	}
}

func TestConnectorPoliciesAcceptExactlyPublishedGenerations(t *testing.T) {
	now := time.Unix(1_800_000_000, 0).UTC()
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("GenerateKey: %v", err)
	}
	verifier := mustVerifier(t, publicKey, now)
	grant := signEligibilityPayload(t, privateKey, validEligibilityPayload(now))
	routes, err := NewRouteStore(filepath.Join(t.TempDir(), "route-seed"))
	if err != nil {
		t.Fatalf("NewRouteStore: %v", err)
	}

	for _, accepted := range []struct {
		name     string
		policies []ConnectorPolicy
	}{
		{name: "legacy four-entry generation", policies: validConnectorPolicyDirectoryForTest()},
		{name: "full seven-entry generation", policies: fullConnectorPolicyDirectoryForTest()},
	} {
		t.Run(accepted.name, func(t *testing.T) {
			runtime, runtimeErr := NewRuntimeWithConnectorPolicies(verifier, grant, routes, accepted.policies)
			if runtimeErr != nil {
				t.Fatalf("NewRuntimeWithConnectorPolicies: %v", runtimeErr)
			}
			if got := len(ConnectorIDs()); got != 7 {
				t.Fatalf("ConnectorIDs()=%d, want the full allowlist regardless of policy generation", got)
			}
			if runtime.ConnectorEnabled(ConnectorQwen) {
				t.Fatal("blocked directory entries must not enable a connector")
			}
		})
	}

	full := fullConnectorPolicyDirectoryForTest()
	for _, rejected := range []struct {
		name     string
		policies []ConnectorPolicy
	}{
		{name: "five entries", policies: full[:5]},
		{name: "six entries", policies: full[:6]},
		{name: "eight entries", policies: append(append([]ConnectorPolicy(nil), full...), full[6])},
		{name: "four entries mixing generations", policies: []ConnectorPolicy{full[0], full[1], full[2], full[4]}},
	} {
		t.Run(rejected.name, func(t *testing.T) {
			if _, runtimeErr := NewRuntimeWithConnectorPolicies(verifier, grant, routes, rejected.policies); runtimeErr == nil {
				t.Fatalf("policies=%+v unexpectedly accepted", rejected.policies)
			}
		})
	}
}

func TestConnectorPoliciesLegacyGenerationLeavesNewConnectorsDisabledButVisible(t *testing.T) {
	now := time.Unix(1_800_000_000, 0).UTC()
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("GenerateKey: %v", err)
	}
	verifier := mustVerifier(t, publicKey, now)
	payload := validEligibilityPayload(now)
	payload.ConnectorIDs = ConnectorIDs()
	grant := signEligibilityPayload(t, privateKey, payload)
	routes, err := NewRouteStore(filepath.Join(t.TempDir(), "route-seed"))
	if err != nil {
		t.Fatalf("NewRouteStore: %v", err)
	}
	runtime, err := NewRuntimeWithConnectorPolicies(verifier, grant, routes, validConnectorPolicyDirectoryForTest())
	if err != nil {
		t.Fatalf("NewRuntimeWithConnectorPolicies: %v", err)
	}
	for _, connectorID := range []string{ConnectorQwen, ConnectorKimi, ConnectorZai} {
		if runtime.ConnectorEnabled(connectorID) {
			t.Fatalf("policy-absent connector %q must never be enabled", connectorID)
		}
		if got := runtime.ConnectorDisabledReason(connectorID); got != "feature_flag_disabled" {
			t.Fatalf("policy-absent connector %q reason=%q", connectorID, got)
		}
		if got := runtime.ConnectorTermsStatus(connectorID); got != "blocked" {
			t.Fatalf("policy-absent connector %q terms=%q", connectorID, got)
		}
		if _, ok := ProviderForConnector(connectorID); !ok {
			t.Fatalf("connector %q must stay resolvable for account enumeration", connectorID)
		}
	}
}

func TestConnectorEnabledAppliesPerConnectorCNFloor(t *testing.T) {
	now := time.Unix(1_800_000_000, 0).UTC()
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("GenerateKey: %v", err)
	}
	verifier := mustVerifier(t, publicKey, now)
	routes, err := NewRouteStore(filepath.Join(t.TempDir(), "route-seed"))
	if err != nil {
		t.Fatalf("NewRouteStore: %v", err)
	}

	// Fully signed-off directory: qwen is global, kimi keeps the CN floor.
	policies := fullConnectorPolicyDirectoryForTest()
	policies[4] = enabledConnectorPolicy(ConnectorQwen, "Qwen", AuthModeDeviceCode, RegionPolicyGlobal)
	policies[5] = enabledConnectorPolicy(ConnectorKimi, "Kimi", AuthModeDeviceCode, RegionPolicyNonCN)
	policies[6] = enabledConnectorPolicy(ConnectorZai, "Z.AI", AuthModeDeviceCode, RegionPolicyGlobal)

	cnPayload := validEligibilityPayloadV2(now)
	cnRuntime, err := NewRuntimeWithConnectorPolicies(verifier, signEligibilityPayload(t, privateKey, cnPayload), routes, policies)
	if err != nil {
		t.Fatalf("NewRuntimeWithConnectorPolicies (CN): %v", err)
	}
	if !cnRuntime.ConnectorEnabled(ConnectorQwen) || !cnRuntime.ConnectorEnabled(ConnectorZai) {
		t.Fatal("global connectors with full membership must be enabled inside CN")
	}
	if cnRuntime.ConnectorEnabled(ConnectorKimi) {
		t.Fatal("non-cn connector must stay disabled inside CN even with membership and signed-off gates")
	}
	if got := cnRuntime.ConnectorDisabledReason(ConnectorKimi); got != "connector_not_eligible" {
		t.Fatalf("CN floor reason=%q, want connector_not_eligible", got)
	}
	if cnRuntime.ConnectorEnabled(ConnectorOpenAI) {
		t.Fatal("legacy connector outside the signed CN membership must stay disabled")
	}
	if got := cnRuntime.ConnectorDisabledReason(ConnectorOpenAI); got != "connector_not_eligible" {
		t.Fatalf("membership-denied reason=%q, want connector_not_eligible", got)
	}

	// The same directory outside CN keeps every signed-off connector usable.
	usPayload := validEligibilityPayloadV2(now)
	usPayload.CountryCode = "US"
	usRuntime, err := NewRuntimeWithConnectorPolicies(verifier, signEligibilityPayload(t, privateKey, usPayload), routes, policies)
	if err != nil {
		t.Fatalf("NewRuntimeWithConnectorPolicies (US): %v", err)
	}
	if !usRuntime.ConnectorEnabled(ConnectorKimi) {
		t.Fatal("the CN floor must be scoped to CN grants")
	}
	if got := usRuntime.ConnectorDisabledReason(ConnectorKimi); got != "" {
		t.Fatalf("enabled connector reason=%q, want empty", got)
	}
}
