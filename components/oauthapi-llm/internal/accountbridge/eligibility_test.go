package accountbridge

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"reflect"
	"testing"
	"time"
)

const (
	testAudience = "oauthapi-llm-sidecar"
	testVersion  = "1"
)

var testClientBinding = EligibilityClientBinding{
	RequestNonce:                  base64.RawURLEncoding.EncodeToString(make([]byte, 32)),
	CrabCodeRelease:               "1.0.13",
	AccountBridgeComponentVersion: "7.2.71-crabcode.4",
	AccountBridgeProtocolVersion:  1,
}

func TestEligibilityVerifierAllowsOnlySignedConnectorMembership(t *testing.T) {
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Unix(1_800_000_000, 0).UTC()
	verifier := mustVerifier(t, publicKey, now)
	payload := validEligibilityPayload(now)
	grant := signEligibilityPayload(t, privateKey, payload)

	decision := verifier.Decide(grant)
	if !decision.Allowed() || decision.Reason != EligibilityAllowed {
		t.Fatalf("expected allowed decision, got %+v", decision)
	}
	if decision.CountryCode != "US" || decision.PolicyVersion != "policy-7" {
		t.Fatalf("unexpected verified projection: %+v", decision)
	}
	if !decision.AllowsConnector("openai") || !decision.AllowsConnector("anthropic") {
		t.Fatal("signed connector membership was not preserved")
	}
	if decision.AllowsConnector("google") {
		t.Fatal("unsigned connector membership must remain denied")
	}
	if got, want := decision.ConnectorIDs(), payload.ConnectorIDs; !reflect.DeepEqual(got, want) {
		t.Fatalf("connector order changed: got %v, want %v", got, want)
	}
	copyResult := decision.ConnectorIDs()
	copyResult[0] = "tampered"
	if !decision.AllowsConnector("openai") || decision.AllowsConnector("tampered") {
		t.Fatal("ConnectorIDs must return a defensive copy")
	}
}

func TestEligibilityVerifierFailsClosed(t *testing.T) {
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	otherPublicKey, otherPrivateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	_ = otherPublicKey
	now := time.Unix(1_800_000_000, 0).UTC()

	tests := []struct {
		name   string
		mutate func(*EligibilityPayload)
		key    ed25519.PrivateKey
		want   EligibilityReason
	}{
		{name: "bad signature", key: otherPrivateKey, want: EligibilityInvalidSignature},
		{name: "audience mismatch", mutate: func(p *EligibilityPayload) { p.Audience = "another-service" }, want: EligibilityAudienceMismatch},
		{name: "version mismatch", mutate: func(p *EligibilityPayload) { p.Version = "2" }, want: EligibilityVersionMismatch},
		{name: "request replay", mutate: func(p *EligibilityPayload) {
			p.Client.RequestNonce = base64.RawURLEncoding.EncodeToString([]byte("11111111111111111111111111111111"))
		}, want: EligibilityRequestReplay},
		{name: "component version mismatch", mutate: func(p *EligibilityPayload) { p.Client.AccountBridgeComponentVersion = "7.2.72-crabcode.1" }, want: EligibilityClientMismatch},
		{name: "protocol mismatch", mutate: func(p *EligibilityPayload) { p.Client.AccountBridgeProtocolVersion++ }, want: EligibilityClientMismatch},
		{name: "release range excludes client", mutate: func(p *EligibilityPayload) {
			p.AllowedClientVersions.CrabCodeRelease = InclusiveVersionRange{MinimumInclusive: "1.0.14", MaximumInclusive: "1.0.20"}
		}, want: EligibilityClientUnsupported},
		{name: "component range excludes client", mutate: func(p *EligibilityPayload) {
			p.AllowedClientVersions.AccountBridgeComponentVersion = InclusiveVersionRange{MinimumInclusive: "7.2.72-crabcode.1", MaximumInclusive: "8.0.0"}
		}, want: EligibilityClientUnsupported},
		{name: "protocol range excludes client", mutate: func(p *EligibilityPayload) {
			p.AllowedClientVersions.AccountBridgeProtocolVersion = InclusiveProtocolRange{MinimumInclusive: 2, MaximumInclusive: 3}
		}, want: EligibilityClientUnsupported},
		{name: "invalid version range", mutate: func(p *EligibilityPayload) {
			p.AllowedClientVersions.CrabCodeRelease = InclusiveVersionRange{MinimumInclusive: "1.0.14", MaximumInclusive: "1.0.13"}
		}, want: EligibilityMalformedGrant},
		{name: "empty policy version", mutate: func(p *EligibilityPayload) { p.PolicyVersion = "" }, want: EligibilityInvalidPolicy},
		{name: "zero issued at", mutate: func(p *EligibilityPayload) { p.IssuedAt = 0 }, want: EligibilityInvalidIssuedAt},
		{name: "future issued at", mutate: func(p *EligibilityPayload) {
			p.IssuedAt = now.Add(time.Second).Unix()
			p.ExpiresAt = now.Add(2 * time.Minute).Unix()
		}, want: EligibilityFutureIssuedAt},
		{name: "expiry before issue", mutate: func(p *EligibilityPayload) { p.ExpiresAt = p.IssuedAt }, want: EligibilityInvalidExpiry},
		{name: "expired", mutate: func(p *EligibilityPayload) { p.IssuedAt = now.Add(-5 * time.Minute).Unix(); p.ExpiresAt = now.Unix() }, want: EligibilityExpired},
		{name: "ttl too long", mutate: func(p *EligibilityPayload) { p.ExpiresAt = p.IssuedAt + int64((5*time.Minute)/time.Second) + 1 }, want: EligibilityTTLExceeded},
		{name: "cn despite allowed bit", mutate: func(p *EligibilityPayload) { p.CountryCode = "CN" }, want: EligibilityBlockedCN},
		{name: "empty region", mutate: func(p *EligibilityPayload) { p.CountryCode = "" }, want: EligibilityUnknownRegion},
		{name: "unknown region", mutate: func(p *EligibilityPayload) { p.CountryCode = "UNKNOWN" }, want: EligibilityUnknownRegion},
		{name: "unassigned region", mutate: func(p *EligibilityPayload) { p.CountryCode = "ZZ" }, want: EligibilityUnknownRegion},
		{name: "region denied", mutate: func(p *EligibilityPayload) { p.RegionAllowed = false }, want: EligibilityRegionNotAllowed},
		{name: "empty connectors", mutate: func(p *EligibilityPayload) { p.ConnectorIDs = nil }, want: EligibilityInvalidConnectors},
		{name: "duplicate connectors", mutate: func(p *EligibilityPayload) { p.ConnectorIDs = []string{"openai", "openai"} }, want: EligibilityInvalidConnectors},
		{name: "invalid connector", mutate: func(p *EligibilityPayload) { p.ConnectorIDs = []string{"OpenAI"} }, want: EligibilityInvalidConnectors},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			payload := validEligibilityPayload(now)
			if test.mutate != nil {
				test.mutate(&payload)
			}
			key := privateKey
			if test.key != nil {
				key = test.key
			}
			decision := mustVerifier(t, publicKey, now).Decide(signEligibilityPayload(t, key, payload))
			if decision.Allowed() || decision.Reason != test.want {
				t.Fatalf("expected denied %q, got %+v", test.want, decision)
			}
			if decision.AllowsConnector("openai") {
				t.Fatal("denied decision must not authorize a connector")
			}
		})
	}
}

func TestEligibilityVerifierRejectsTamperingAndTermsClaim(t *testing.T) {
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Unix(1_800_000_000, 0).UTC()
	verifier := mustVerifier(t, publicKey, now)
	grant := signEligibilityPayload(t, privateKey, validEligibilityPayload(now))

	payloadBytes, err := base64.RawURLEncoding.DecodeString(grant.PayloadBase64URL)
	if err != nil {
		t.Fatal(err)
	}
	for i := range payloadBytes {
		if payloadBytes[i] == 'U' {
			payloadBytes[i] = 'C'
			break
		}
	}
	grant.PayloadBase64URL = base64.RawURLEncoding.EncodeToString(payloadBytes)
	if decision := verifier.Decide(grant); decision.Allowed() || decision.Reason != EligibilityInvalidSignature {
		t.Fatalf("tampered payload must fail signature verification, got %+v", decision)
	}

	// Even a correctly signed payload cannot smuggle a fabricated terms status
	// into the grant contract.
	rawWithTerms := []byte(`{"audience":"oauthapi-llm-sidecar","version":"1","policyVersion":"policy-7","issuedAt":1799999940,"expiresAt":1800000240,"countryCode":"US","regionAllowed":true,"connectors":["openai"],"termsStatus":"signed-off"}`)
	grantWithTerms := signRawEligibilityPayload(privateKey, rawWithTerms)
	if decision := verifier.Decide(grantWithTerms); decision.Allowed() || decision.Reason != EligibilityMalformedGrant {
		t.Fatalf("terms status is outside the eligibility contract, got %+v", decision)
	}
}

func TestEligibilityDecisionCannotBeForgedFromPublicProjection(t *testing.T) {
	forged := EligibilityDecision{
		Reason:        EligibilityAllowed,
		CountryCode:   "US",
		PolicyVersion: "forged",
		connectors:    map[string]struct{}{"openai": {}},
	}
	if forged.Allowed() || forged.AllowsConnector("openai") {
		t.Fatal("public projection fields must not fabricate an allowed decision")
	}
}

func TestEligibilityVerifierHonorsSignedNonCNRegionDecision(t *testing.T) {
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Unix(1_800_000_000, 0).UTC()
	for _, countryCode := range []string{"HK", "MO", "TW"} {
		t.Run(countryCode, func(t *testing.T) {
			payload := validEligibilityPayload(now)
			payload.CountryCode = countryCode
			decision := mustVerifier(t, publicKey, now).Decide(signEligibilityPayload(t, privateKey, payload))
			if !decision.Allowed() || !decision.AllowsConnector("openai") {
				t.Fatalf("signed non-CN decision unexpectedly denied: %+v", decision)
			}
		})
	}
}

func TestNewEligibilityVerifierRejectsInvalidConfiguration(t *testing.T) {
	publicKey, _, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		name     string
		key      ed25519.PublicKey
		audience string
		version  string
		client   EligibilityClientBinding
	}{
		{name: "bad key", key: publicKey[:10], audience: testAudience, version: testVersion, client: testClientBinding},
		{name: "empty audience", key: publicKey, version: testVersion, client: testClientBinding},
		{name: "empty version", key: publicKey, audience: testAudience, client: testClientBinding},
		{name: "invalid client nonce", key: publicKey, audience: testAudience, version: testVersion, client: EligibilityClientBinding{
			RequestNonce:                  "invalid",
			CrabCodeRelease:               testClientBinding.CrabCodeRelease,
			AccountBridgeComponentVersion: testClientBinding.AccountBridgeComponentVersion,
			AccountBridgeProtocolVersion:  testClientBinding.AccountBridgeProtocolVersion,
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			if _, err := NewEligibilityVerifier(test.key, test.audience, test.version, test.client); err == nil {
				t.Fatal("expected constructor to fail closed")
			}
		})
	}
}

func validEligibilityPayload(now time.Time) EligibilityPayload {
	return EligibilityPayload{
		Audience:              testAudience,
		Version:               testVersion,
		Client:                testClientBinding,
		AllowedClientVersions: exactAllowedClientVersionsForTest(testClientBinding),
		PolicyVersion:         "policy-7",
		IssuedAt:              now.Add(-time.Minute).Unix(),
		ExpiresAt:             now.Add(4 * time.Minute).Unix(),
		CountryCode:           "US",
		RegionAllowed:         true,
		ConnectorIDs:          []string{"openai", "anthropic"},
	}
}

// validEligibilityPayloadV2 is a second-generation CN grant: regional policy
// moved to the signed connector membership, so the payload names only the
// globally distributable connectors and leaves the legacy RegionAllowed bit
// unset.
func validEligibilityPayloadV2(now time.Time) EligibilityPayload {
	payload := validEligibilityPayload(now)
	payload.Version = eligibilityVersionV2
	payload.CountryCode = "CN"
	payload.RegionAllowed = false
	payload.ConnectorIDs = []string{"qwen", "kimi", "zai"}
	return payload
}

func TestEligibilityVerifierSecondGenerationCNGrantBootsWithMembership(t *testing.T) {
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Unix(1_800_000_000, 0).UTC()
	verifier := mustVerifier(t, publicKey, now)
	decision := verifier.Decide(signEligibilityPayload(t, privateKey, validEligibilityPayloadV2(now)))
	if !decision.Allowed() || decision.Reason != EligibilityAllowed {
		t.Fatalf("second-generation CN grant unexpectedly denied: %+v", decision)
	}
	if !decision.BootAllowed() {
		t.Fatalf("second-generation CN grant with membership must boot: %+v", decision)
	}
	if decision.CountryCode != "CN" {
		t.Fatalf("verified country code must be exposed for the CN floor, got %q", decision.CountryCode)
	}
	for _, connectorID := range []string{"qwen", "kimi", "zai"} {
		if !decision.AllowsConnector(connectorID) {
			t.Fatalf("signed membership for %q was not preserved", connectorID)
		}
	}
	for _, connectorID := range []string{"openai", "anthropic", "google", "xai"} {
		if decision.AllowsConnector(connectorID) {
			t.Fatalf("connector %q outside the signed membership must remain denied", connectorID)
		}
	}
}

func TestEligibilityVerifierSecondGenerationEmptyConnectorsDoesNotBoot(t *testing.T) {
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Unix(1_800_000_000, 0).UTC()
	verifier := mustVerifier(t, publicKey, now)
	payload := validEligibilityPayloadV2(now)
	payload.ConnectorIDs = nil
	decision := verifier.Decide(signEligibilityPayload(t, privateKey, payload))
	if decision.BootAllowed() || decision.Allowed() || decision.Reason != EligibilityInvalidConnectors {
		t.Fatalf("empty second-generation membership must not boot: %+v", decision)
	}
}

func TestEligibilityVerifierSecondGenerationIgnoresLegacyRegionBitOutsideCN(t *testing.T) {
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Unix(1_800_000_000, 0).UTC()
	payload := validEligibilityPayloadV2(now)
	payload.CountryCode = "US"
	payload.RegionAllowed = false
	decision := mustVerifier(t, publicKey, now).Decide(signEligibilityPayload(t, privateKey, payload))
	if !decision.Allowed() || !decision.BootAllowed() || decision.CountryCode != "US" {
		t.Fatalf("second-generation grant must not depend on the legacy RegionAllowed bit: %+v", decision)
	}
}

func TestEligibilityVerifierSecondGenerationKeepsNonRegionalFailClosedChecks(t *testing.T) {
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Unix(1_800_000_000, 0).UTC()
	tests := []struct {
		name   string
		mutate func(*EligibilityPayload)
		want   EligibilityReason
	}{
		{name: "expired", mutate: func(p *EligibilityPayload) {
			p.IssuedAt = now.Add(-5 * time.Minute).Unix()
			p.ExpiresAt = now.Unix()
		}, want: EligibilityExpired},
		{name: "unknown region", mutate: func(p *EligibilityPayload) { p.CountryCode = "ZZ" }, want: EligibilityUnknownRegion},
		{name: "request replay", mutate: func(p *EligibilityPayload) {
			p.Client.RequestNonce = base64.RawURLEncoding.EncodeToString([]byte("11111111111111111111111111111111"))
		}, want: EligibilityRequestReplay},
		{name: "duplicate connectors", mutate: func(p *EligibilityPayload) {
			p.ConnectorIDs = []string{"qwen", "qwen"}
		}, want: EligibilityInvalidConnectors},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			payload := validEligibilityPayloadV2(now)
			test.mutate(&payload)
			decision := mustVerifier(t, publicKey, now).Decide(signEligibilityPayload(t, privateKey, payload))
			if decision.Allowed() || decision.BootAllowed() || decision.Reason != test.want {
				t.Fatalf("expected denied %q, got %+v", test.want, decision)
			}
		})
	}
}

func TestEligibilityVerifierRejectsFutureGenerationVersion(t *testing.T) {
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Unix(1_800_000_000, 0).UTC()
	payload := validEligibilityPayloadV2(now)
	payload.Version = "v3"
	decision := mustVerifier(t, publicKey, now).Decide(signEligibilityPayload(t, privateKey, payload))
	if decision.Allowed() || decision.BootAllowed() || decision.Reason != EligibilityVersionMismatch {
		t.Fatalf("unknown payload generation must fail closed: %+v", decision)
	}
}

func TestEligibilityFirstGenerationBootGateMatchesAllowed(t *testing.T) {
	publicKey, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Unix(1_800_000_000, 0).UTC()
	verifier := mustVerifier(t, publicKey, now)
	allowed := verifier.Decide(signEligibilityPayload(t, privateKey, validEligibilityPayload(now)))
	if !allowed.Allowed() || !allowed.BootAllowed() {
		t.Fatalf("valid first-generation grant must boot: %+v", allowed)
	}
	cn := validEligibilityPayload(now)
	cn.CountryCode = "CN"
	denied := verifier.Decide(signEligibilityPayload(t, privateKey, cn))
	if denied.Allowed() || denied.BootAllowed() || denied.Reason != EligibilityBlockedCN {
		t.Fatalf("first-generation CN grant must keep the hard block: %+v", denied)
	}
}

func exactAllowedClientVersionsForTest(client EligibilityClientBinding) AllowedClientVersions {
	return AllowedClientVersions{
		CrabCodeRelease: InclusiveVersionRange{
			MinimumInclusive: client.CrabCodeRelease,
			MaximumInclusive: client.CrabCodeRelease,
		},
		AccountBridgeComponentVersion: InclusiveVersionRange{
			MinimumInclusive: client.AccountBridgeComponentVersion,
			MaximumInclusive: client.AccountBridgeComponentVersion,
		},
		AccountBridgeProtocolVersion: InclusiveProtocolRange{
			MinimumInclusive: client.AccountBridgeProtocolVersion,
			MaximumInclusive: client.AccountBridgeProtocolVersion,
		},
	}
}

func mustVerifier(t *testing.T, publicKey ed25519.PublicKey, now time.Time) *EligibilityVerifier {
	t.Helper()
	verifier, err := NewEligibilityVerifier(publicKey, testAudience, testVersion, testClientBinding)
	if err != nil {
		t.Fatal(err)
	}
	verifier.now = func() time.Time { return now }
	return verifier
}

func signEligibilityPayload(t *testing.T, privateKey ed25519.PrivateKey, payload EligibilityPayload) SignedEligibilityGrant {
	t.Helper()
	raw, err := json.Marshal(payload)
	if err != nil {
		t.Fatal(err)
	}
	return signRawEligibilityPayload(privateKey, raw)
}

func signRawEligibilityPayload(privateKey ed25519.PrivateKey, raw []byte) SignedEligibilityGrant {
	return SignedEligibilityGrant{
		PayloadBase64URL:   base64.RawURLEncoding.EncodeToString(raw),
		SignatureBase64URL: base64.RawURLEncoding.EncodeToString(ed25519.Sign(privateKey, raw)),
	}
}
