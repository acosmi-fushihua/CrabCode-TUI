package main

import (
	"bytes"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/accountbridge"
)

const (
	testBootstrapCrabCodeRelease  = "1.0.13"
	testBootstrapComponentVersion = "7.2.71-crabcode.4"
)

func testBootstrapClient() accountbridge.EligibilityClientBinding {
	return accountbridge.EligibilityClientBinding{
		RequestNonce:                  base64.RawURLEncoding.EncodeToString(bytes.Repeat([]byte{0x42}, 32)),
		CrabCodeRelease:               testBootstrapCrabCodeRelease,
		AccountBridgeComponentVersion: testBootstrapComponentVersion,
		AccountBridgeProtocolVersion:  accountbridge.ProtocolVersion,
	}
}

func exactBootstrapVersionRanges(client accountbridge.EligibilityClientBinding) accountbridge.AllowedClientVersions {
	return accountbridge.AllowedClientVersions{
		CrabCodeRelease:               accountbridge.InclusiveVersionRange{MinimumInclusive: client.CrabCodeRelease, MaximumInclusive: client.CrabCodeRelease},
		AccountBridgeComponentVersion: accountbridge.InclusiveVersionRange{MinimumInclusive: client.AccountBridgeComponentVersion, MaximumInclusive: client.AccountBridgeComponentVersion},
		AccountBridgeProtocolVersion:  accountbridge.InclusiveProtocolRange{MinimumInclusive: client.AccountBridgeProtocolVersion, MaximumInclusive: client.AccountBridgeProtocolVersion},
	}
}

func setTestBootstrapReleaseIdentity(t *testing.T) {
	t.Helper()
	previousRelease, previousComponent := AccountBridgeCrabCodeRelease, Version
	AccountBridgeCrabCodeRelease, Version = testBootstrapCrabCodeRelease, testBootstrapComponentVersion
	t.Cleanup(func() {
		AccountBridgeCrabCodeRelease, Version = previousRelease, previousComponent
	})
}

func legacyBootstrapConnectorPolicies() []accountbridge.ConnectorPolicy {
	return []accountbridge.ConnectorPolicy{
		{ConnectorID: accountbridge.ConnectorOpenAI, DisplayName: "Directory OpenAI", AuthMode: accountbridge.AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
		{ConnectorID: accountbridge.ConnectorAnthropic, DisplayName: "Directory Anthropic", AuthMode: accountbridge.AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
		{ConnectorID: accountbridge.ConnectorGoogle, DisplayName: "Directory Google", AuthMode: accountbridge.AuthModeBrowser, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
		{ConnectorID: accountbridge.ConnectorXAI, DisplayName: "Directory xAI", AuthMode: accountbridge.AuthModeDeviceCode, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyNonCN},
	}
}

func fullBootstrapConnectorPolicies() []accountbridge.ConnectorPolicy {
	return append(legacyBootstrapConnectorPolicies(),
		accountbridge.ConnectorPolicy{ConnectorID: accountbridge.ConnectorQwen, DisplayName: "Directory Qwen", AuthMode: accountbridge.AuthModeDeviceCode, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyGlobal},
		accountbridge.ConnectorPolicy{ConnectorID: accountbridge.ConnectorKimi, DisplayName: "Directory Kimi", AuthMode: accountbridge.AuthModeDeviceCode, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyGlobal},
		accountbridge.ConnectorPolicy{ConnectorID: accountbridge.ConnectorZai, DisplayName: "Directory Z.AI", AuthMode: accountbridge.AuthModeDeviceCode, TermsStatus: "blocked", RegionPolicy: accountbridge.RegionPolicyGlobal},
	)
}

func signedBootstrapForTest(t *testing.T) ([]byte, string) {
	t.Helper()
	setTestBootstrapReleaseIdentity(t)
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("generate key: %v", err)
	}
	now := time.Now().UTC().Unix()
	client := testBootstrapClient()
	payload, err := json.Marshal(accountbridge.EligibilityPayload{
		Audience:              accountBridgeGrantAudience,
		Version:               accountBridgeGrantVersion,
		Client:                client,
		AllowedClientVersions: exactBootstrapVersionRanges(client),
		PolicyVersion:         "test-policy",
		IssuedAt:              now - 1,
		ExpiresAt:             now + 60,
		CountryCode:           "US",
		RegionAllowed:         true,
		ConnectorIDs:          accountbridge.ConnectorIDs(),
	})
	if err != nil {
		t.Fatalf("marshal payload: %v", err)
	}
	encoding := base64.RawURLEncoding
	wire := accountBridgeBootstrapWire{
		MasterKeyBase64URL: encoding.EncodeToString(bytes.Repeat([]byte{0x5a}, 32)),
		RequestNonce:       client.RequestNonce,
		Grant: accountbridge.SignedEligibilityGrant{
			PayloadBase64URL:   encoding.EncodeToString(payload),
			SignatureBase64URL: encoding.EncodeToString(ed25519.Sign(privateKey, payload)),
		},
		ConnectorPolicies: legacyBootstrapConnectorPolicies(),
	}
	raw, err := json.Marshal(wire)
	if err != nil {
		t.Fatalf("marshal bootstrap: %v", err)
	}
	return raw, encoding.EncodeToString(publicKey)
}

func TestDecodeAccountBridgeBootstrapStrictAndFailClosed(t *testing.T) {
	raw, trustRoot := signedBootstrapForTest(t)
	bootstrap, err := decodeAccountBridgeBootstrap(bytes.NewReader(raw), trustRoot)
	if err != nil {
		t.Fatalf("decode valid bootstrap: %v", err)
	}
	if len(bootstrap.MasterKey) != 32 || !bootstrap.Verifier.Decide(bootstrap.Grant).Allowed() {
		t.Fatal("valid bootstrap did not produce a usable key and verifier")
	}
	if len(bootstrap.Policies) != 4 || bootstrap.Policies[0].DisplayName != "Directory OpenAI" || bootstrap.Policies[3].AuthMode != accountbridge.AuthModeDeviceCode {
		t.Fatalf("connector presentation metadata did not survive bootstrap DTO: %+v", bootstrap.Policies)
	}
	clearSecret(bootstrap.MasterKey)

	unknownField := append(append([]byte(nil), raw[:len(raw)-1]...), []byte(`,"extra":true}`)...)
	unknownPolicyField := bytes.Replace(
		raw,
		[]byte(`"connectorId":"openai"`),
		[]byte(`"connectorId":"openai","backend":"codex"`),
		1,
	)
	if bytes.Equal(unknownPolicyField, raw) {
		t.Fatal("connector fixture did not contain the expected DTO field")
	}
	trailingJSON := append(append([]byte(nil), raw...), []byte(`{}`)...)
	var invalidPolicyWire accountBridgeBootstrapWire
	if err := json.Unmarshal(raw, &invalidPolicyWire); err != nil {
		t.Fatalf("decode bootstrap fixture: %v", err)
	}
	invalidPolicyWire.ConnectorPolicies[0].AuthMode = "redirect"
	invalidPolicy, err := json.Marshal(invalidPolicyWire)
	if err != nil {
		t.Fatalf("marshal invalid policy bootstrap: %v", err)
	}
	invalidPolicyWire.ConnectorPolicies = invalidPolicyWire.ConnectorPolicies[:3]
	incompleteDirectory, err := json.Marshal(invalidPolicyWire)
	if err != nil {
		t.Fatalf("marshal incomplete policy bootstrap: %v", err)
	}
	var replayWire accountBridgeBootstrapWire
	if err = json.Unmarshal(raw, &replayWire); err != nil {
		t.Fatalf("decode replay bootstrap fixture: %v", err)
	}
	replayWire.RequestNonce = base64.RawURLEncoding.EncodeToString(bytes.Repeat([]byte{0x43}, 32))
	replayedResponse, err := json.Marshal(replayWire)
	if err != nil {
		t.Fatalf("marshal replay bootstrap: %v", err)
	}
	var mixedWire accountBridgeBootstrapWire
	if err = json.Unmarshal(raw, &mixedWire); err != nil {
		t.Fatalf("decode mixed-generation bootstrap fixture: %v", err)
	}
	mixedWire.ConnectorPolicies = append(mixedWire.ConnectorPolicies, fullBootstrapConnectorPolicies()[4])
	mixedGeneration, err := json.Marshal(mixedWire)
	if err != nil {
		t.Fatalf("marshal mixed-generation bootstrap: %v", err)
	}
	var missingRegionWire accountBridgeBootstrapWire
	if err = json.Unmarshal(raw, &missingRegionWire); err != nil {
		t.Fatalf("decode missing-region bootstrap fixture: %v", err)
	}
	missingRegionWire.ConnectorPolicies[0].RegionPolicy = ""
	missingRegionPolicy, err := json.Marshal(missingRegionWire)
	if err != nil {
		t.Fatalf("marshal missing-region bootstrap: %v", err)
	}
	for name, testCase := range map[string]struct {
		reader *bytes.Reader
		root   string
	}{
		"missing":                    {reader: bytes.NewReader(nil), root: trustRoot},
		"unknown field":              {reader: bytes.NewReader(unknownField), root: trustRoot},
		"unknown policy field":       {reader: bytes.NewReader(unknownPolicyField), root: trustRoot},
		"invalid trust root":         {reader: bytes.NewReader(raw), root: "invalid"},
		"invalid policy dto":         {reader: bytes.NewReader(invalidPolicy), root: trustRoot},
		"incomplete directory":       {reader: bytes.NewReader(incompleteDirectory), root: trustRoot},
		"mixed generation directory": {reader: bytes.NewReader(mixedGeneration), root: trustRoot},
		"missing region policy":      {reader: bytes.NewReader(missingRegionPolicy), root: trustRoot},
		"replayed response":          {reader: bytes.NewReader(replayedResponse), root: trustRoot},
		"trailing json":              {reader: bytes.NewReader(trailingJSON), root: trustRoot},
	} {
		t.Run(name, func(t *testing.T) {
			if _, err := decodeAccountBridgeBootstrap(testCase.reader, testCase.root); err == nil {
				t.Fatal("invalid bootstrap was accepted")
			}
		})
	}
}

// signedBootstrapV2ForTest builds a second-generation CN bootstrap: the grant
// payload uses the "v2" version label, names only the given connectors, and is
// paired with the given connector policy generation.
func signedBootstrapV2ForTest(t *testing.T, connectorIDs []string, policies []accountbridge.ConnectorPolicy) ([]byte, string) {
	t.Helper()
	setTestBootstrapReleaseIdentity(t)
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("generate key: %v", err)
	}
	now := time.Now().UTC().Unix()
	client := testBootstrapClient()
	payload, err := json.Marshal(accountbridge.EligibilityPayload{
		Audience:              accountBridgeGrantAudience,
		Version:               "v2",
		Client:                client,
		AllowedClientVersions: exactBootstrapVersionRanges(client),
		PolicyVersion:         "test-policy-v2",
		IssuedAt:              now - 1,
		ExpiresAt:             now + 60,
		CountryCode:           "CN",
		RegionAllowed:         false,
		ConnectorIDs:          connectorIDs,
	})
	if err != nil {
		t.Fatalf("marshal payload: %v", err)
	}
	encoding := base64.RawURLEncoding
	wire := accountBridgeBootstrapWire{
		MasterKeyBase64URL: encoding.EncodeToString(bytes.Repeat([]byte{0x5b}, 32)),
		RequestNonce:       client.RequestNonce,
		Grant: accountbridge.SignedEligibilityGrant{
			PayloadBase64URL:   encoding.EncodeToString(payload),
			SignatureBase64URL: encoding.EncodeToString(ed25519.Sign(privateKey, payload)),
		},
		ConnectorPolicies: policies,
	}
	raw, err := json.Marshal(wire)
	if err != nil {
		t.Fatalf("marshal bootstrap: %v", err)
	}
	return raw, encoding.EncodeToString(publicKey)
}

func TestDecodeAccountBridgeBootstrapAcceptsSecondGenerationCNGrant(t *testing.T) {
	raw, trustRoot := signedBootstrapV2ForTest(
		t,
		[]string{accountbridge.ConnectorQwen, accountbridge.ConnectorKimi, accountbridge.ConnectorZai},
		fullBootstrapConnectorPolicies(),
	)
	bootstrap, err := decodeAccountBridgeBootstrap(bytes.NewReader(raw), trustRoot)
	if err != nil {
		t.Fatalf("decode second-generation bootstrap: %v", err)
	}
	defer clearSecret(bootstrap.MasterKey)
	decision := bootstrap.Verifier.Decide(bootstrap.Grant)
	if !decision.BootAllowed() || decision.CountryCode != "CN" {
		t.Fatalf("second-generation CN grant did not boot: %+v", decision)
	}
	if len(bootstrap.Policies) != 7 {
		t.Fatalf("policies=%d, want the full seven-entry generation", len(bootstrap.Policies))
	}
}

func TestDecodeAccountBridgeBootstrapSecondGenerationFailClosed(t *testing.T) {
	t.Run("empty connector membership", func(t *testing.T) {
		raw, trustRoot := signedBootstrapV2ForTest(t, nil, fullBootstrapConnectorPolicies())
		if _, err := decodeAccountBridgeBootstrap(bytes.NewReader(raw), trustRoot); err == nil {
			t.Fatal("second-generation grant without connectors was accepted")
		}
	})
	t.Run("legacy directory with second-generation grant is a published pairing", func(t *testing.T) {
		// The grant generation and the directory generation are independent
		// contracts: an older host directory paired with a v2 grant must not
		// reject the bootstrap.
		raw, trustRoot := signedBootstrapV2ForTest(
			t,
			[]string{accountbridge.ConnectorQwen, accountbridge.ConnectorKimi, accountbridge.ConnectorZai},
			legacyBootstrapConnectorPolicies(),
		)
		bootstrap, err := decodeAccountBridgeBootstrap(bytes.NewReader(raw), trustRoot)
		if err != nil {
			t.Fatalf("decode legacy-directory second-generation bootstrap: %v", err)
		}
		clearSecret(bootstrap.MasterKey)
	})
}

func TestDecodeAccountBridgeBootstrapRejectsBuildIdentityMismatch(t *testing.T) {
	raw, trustRoot := signedBootstrapForTest(t)
	Version = "7.2.72-crabcode.1"
	if _, err := decodeAccountBridgeBootstrap(bytes.NewReader(raw), trustRoot); err == nil {
		t.Fatal("grant for another component build identity was accepted")
	}
}

func TestReadAccountBridgeBootstrapRequiresReadOnlyPipe(t *testing.T) {
	raw, trustRoot := signedBootstrapForTest(t)
	reader, writer, err := os.Pipe()
	if err != nil {
		t.Fatalf("create pipe: %v", err)
	}
	done := make(chan error, 1)
	go func() {
		_, writeErr := writer.Write(raw)
		closeErr := writer.Close()
		if writeErr != nil {
			done <- writeErr
			return
		}
		done <- closeErr
	}()
	bootstrap, err := readAccountBridgeBootstrapFD(reader.Fd(), trustRoot)
	if err != nil {
		t.Fatalf("read pipe bootstrap: %v", err)
	}
	clearSecret(bootstrap.MasterKey)
	if err = <-done; err != nil {
		t.Fatalf("write pipe bootstrap: %v", err)
	}

	regular, err := os.CreateTemp(t.TempDir(), "not-a-pipe")
	if err != nil {
		t.Fatalf("create regular file: %v", err)
	}
	defer regular.Close()
	if _, err = readAccountBridgeBootstrapFD(regular.Fd(), trustRoot); err == nil {
		t.Fatal("regular file was accepted as private bootstrap pipe")
	}
}

func TestBootstrapContractHasNoArgvOrEnvironmentFallback(t *testing.T) {
	source, err := os.ReadFile("account_bridge_bootstrap.go")
	if err != nil {
		t.Fatalf("read source: %v", err)
	}
	lower := strings.ToLower(string(source))
	for _, forbidden := range []string{"os.getenv", "os.lookupenv", "flag.", "master_key", "master-key"} {
		if strings.Contains(lower, forbidden) {
			t.Fatalf("bootstrap source contains forbidden fallback %q", forbidden)
		}
	}
}
