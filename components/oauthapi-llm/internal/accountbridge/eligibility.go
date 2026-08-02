// Package accountbridge contains the security-critical primitives used by the
// narrow CrabCode Account Bridge facade.
package accountbridge

import (
	"bytes"
	"crypto/ed25519"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"strings"
	"time"
)

const (
	eligibilityGrantMaxTTL       = 5 * time.Minute
	maxEligibilityPayloadBytes   = 16 * 1024
	maxEligibilityConnectorCount = 64

	// eligibilityVersionV2 is the fixed second-generation payload version label.
	// A verifier accepts exactly two payload generations: the first-generation
	// version string it was constructed with, and this literal. Generation one
	// keeps the historical global gates (CN hard-block plus the signed
	// RegionAllowed bit). Generation two moves regional policy to per-connector
	// membership: the signed connector list is the server's regional decision
	// and the runtime applies a per-connector CN floor on top, so a CN grant no
	// longer denies the whole bridge.
	eligibilityVersionV2 = "v2"
)

// SignedEligibilityGrant carries an opaque, signed eligibility payload. Both
// fields use unpadded base64url. The signed grant must stay on the private
// control-worker-to-sidecar channel; callers must never expose it in a public
// facade, logs, telemetry, GUI, or TUI.
type SignedEligibilityGrant struct {
	PayloadBase64URL   string `json:"payload"`
	SignatureBase64URL string `json:"signature"`
}

// EligibilityClientBinding is echoed by the signed control-plane response and
// bound to one fresh worker request nonce. Release identity comes from trusted
// build inputs; only RequestNonce crosses the private bootstrap channel.
type EligibilityClientBinding struct {
	RequestNonce                  string `json:"requestNonce"`
	CrabCodeRelease               string `json:"crabCodeRelease"`
	AccountBridgeComponentVersion string `json:"accountBridgeComponentVersion"`
	AccountBridgeProtocolVersion  int    `json:"accountBridgeProtocolVersion"`
}

type InclusiveVersionRange struct {
	MinimumInclusive string `json:"minimumInclusive"`
	MaximumInclusive string `json:"maximumInclusive"`
}

type InclusiveProtocolRange struct {
	MinimumInclusive int `json:"minimumInclusive"`
	MaximumInclusive int `json:"maximumInclusive"`
}

type AllowedClientVersions struct {
	CrabCodeRelease               InclusiveVersionRange  `json:"crabCodeRelease"`
	AccountBridgeComponentVersion InclusiveVersionRange  `json:"accountBridgeComponentVersion"`
	AccountBridgeProtocolVersion  InclusiveProtocolRange `json:"accountBridgeProtocolVersion"`
}

// EligibilityPayload is the exact claim set authenticated by Ed25519.
//
// ConnectorIDs only grants regional eligibility for the named connectors. It
// deliberately has no terms status: a valid eligibility grant is not evidence
// that provider terms or commercial-distribution review was signed off.
type EligibilityPayload struct {
	Audience              string                   `json:"audience"`
	Version               string                   `json:"version"`
	Client                EligibilityClientBinding `json:"client"`
	AllowedClientVersions AllowedClientVersions    `json:"allowedClientVersions"`
	PolicyVersion         string                   `json:"policyVersion"`
	IssuedAt              int64                    `json:"issuedAt"`
	ExpiresAt             int64                    `json:"expiresAt"`
	CountryCode           string                   `json:"countryCode"`
	RegionAllowed         bool                     `json:"regionAllowed"`
	ConnectorIDs          []string                 `json:"connectors"`
}

// EligibilityReason is a stable, non-sensitive fail-closed decision reason.
type EligibilityReason string

const (
	EligibilityAllowed           EligibilityReason = "allowed"
	EligibilityMalformedGrant    EligibilityReason = "malformed_grant"
	EligibilityInvalidSignature  EligibilityReason = "invalid_signature"
	EligibilityAudienceMismatch  EligibilityReason = "audience_mismatch"
	EligibilityVersionMismatch   EligibilityReason = "version_mismatch"
	EligibilityRequestReplay     EligibilityReason = "request_replay"
	EligibilityClientMismatch    EligibilityReason = "client_version_mismatch"
	EligibilityClientUnsupported EligibilityReason = "client_version_unsupported"
	EligibilityInvalidPolicy     EligibilityReason = "invalid_policy_version"
	EligibilityInvalidIssuedAt   EligibilityReason = "invalid_issued_at"
	EligibilityFutureIssuedAt    EligibilityReason = "future_issued_at"
	EligibilityInvalidExpiry     EligibilityReason = "invalid_expiry"
	EligibilityExpired           EligibilityReason = "expired"
	EligibilityTTLExceeded       EligibilityReason = "ttl_exceeded"
	EligibilityUnknownRegion     EligibilityReason = "unknown_region"
	EligibilityBlockedCN         EligibilityReason = "blocked_cn"
	EligibilityRegionNotAllowed  EligibilityReason = "region_not_allowed"
	EligibilityInvalidConnectors EligibilityReason = "invalid_connectors"
)

// EligibilityDecision defaults to denied. Callers must check Allowed() or use
// AllowsConnector before enabling any Account Bridge operation.
type EligibilityDecision struct {
	Reason        EligibilityReason
	CountryCode   string
	PolicyVersion string
	IssuedAt      int64
	ExpiresAt     int64
	allowed       bool
	connectorIDs  []string
	connectors    map[string]struct{}
}

// Allowed reports whether the signed grant passed every fail-closed check for
// its generation. The underlying bit is private so another package cannot
// fabricate an allowed decision with a struct literal.
func (d EligibilityDecision) Allowed() bool {
	return d.allowed
}

// BootAllowed reports whether the signed grant permits booting the Account
// Bridge at all: the grant must be valid for its generation and must name at
// least one connector. It is the gate used by runtime construction and the
// facade middleware. For generation-one grants this is identical to Allowed()
// (a valid v1 grant always carries connectors and CN never validates); for
// generation-two grants it makes the boot contract explicit — a CN grant boots
// when the signed connector membership is non-empty, and per-connector gates
// decide what is reachable.
func (d EligibilityDecision) BootAllowed() bool {
	return d.allowed && len(d.connectors) > 0
}

// AllowsConnector reports whether both the signed regional decision and the
// signed connector membership permit connectorID. It does not assert that the
// connector's provider terms have been signed off.
func (d EligibilityDecision) AllowsConnector(connectorID string) bool {
	if !d.allowed {
		return false
	}
	_, ok := d.connectors[connectorID]
	return ok
}

// ConnectorIDs returns a defensive copy of the connector IDs authenticated by
// the grant. The order matches the signed payload.
func (d EligibilityDecision) ConnectorIDs() []string {
	if len(d.connectorIDs) == 0 {
		return nil
	}
	return append([]string(nil), d.connectorIDs...)
}

// EligibilityVerifier verifies grants for one exact audience and contract
// version. Its public constructor copies the key so later caller mutation
// cannot silently change trust roots.
type EligibilityVerifier struct {
	publicKey ed25519.PublicKey
	audience  string
	version   string
	client    EligibilityClientBinding
	now       func() time.Time
}

// NewEligibilityVerifier constructs an exact-audience, exact-version verifier.
func NewEligibilityVerifier(publicKey ed25519.PublicKey, audience, version string, client EligibilityClientBinding) (*EligibilityVerifier, error) {
	if len(publicKey) != ed25519.PublicKeySize {
		return nil, errors.New("accountbridge: invalid Ed25519 public key")
	}
	if audience == "" {
		return nil, errors.New("accountbridge: empty eligibility audience")
	}
	if version == "" {
		return nil, errors.New("accountbridge: empty eligibility version")
	}
	if !validEligibilityClientBinding(client) {
		return nil, errors.New("accountbridge: invalid eligibility client binding")
	}
	keyCopy := append(ed25519.PublicKey(nil), publicKey...)
	return &EligibilityVerifier{
		publicKey: keyCopy,
		audience:  audience,
		version:   version,
		client:    client,
		now:       time.Now,
	}, nil
}

// Decide verifies and evaluates a signed grant. Every malformed, unverifiable,
// stale, future-issued, or unknown-region input returns an Allowed() == false
// decision. Generation-one payloads additionally deny CN and region-denied
// grants; generation-two payloads leave regional policy to the signed
// connector membership and the runtime's per-connector CN floor.
func (v *EligibilityVerifier) Decide(grant SignedEligibilityGrant) EligibilityDecision {
	denied := func(reason EligibilityReason) EligibilityDecision {
		return EligibilityDecision{Reason: reason}
	}
	if v == nil || len(v.publicKey) != ed25519.PublicKeySize || v.now == nil {
		return denied(EligibilityMalformedGrant)
	}

	strictBase64URL := base64.RawURLEncoding.Strict()
	if len(grant.PayloadBase64URL) > strictBase64URL.EncodedLen(maxEligibilityPayloadBytes) {
		return denied(EligibilityMalformedGrant)
	}
	payloadBytes, err := strictBase64URL.DecodeString(grant.PayloadBase64URL)
	if err != nil || len(payloadBytes) == 0 || len(payloadBytes) > maxEligibilityPayloadBytes {
		return denied(EligibilityMalformedGrant)
	}
	if len(grant.SignatureBase64URL) != strictBase64URL.EncodedLen(ed25519.SignatureSize) {
		return denied(EligibilityInvalidSignature)
	}
	signature, err := strictBase64URL.DecodeString(grant.SignatureBase64URL)
	if err != nil || len(signature) != ed25519.SignatureSize {
		return denied(EligibilityInvalidSignature)
	}
	if !ed25519.Verify(v.publicKey, payloadBytes, signature) {
		return denied(EligibilityInvalidSignature)
	}

	payload, err := decodeEligibilityPayload(payloadBytes)
	if err != nil {
		return denied(EligibilityMalformedGrant)
	}
	if payload.Audience != v.audience {
		return denied(EligibilityAudienceMismatch)
	}
	// Exactly two payload generations are supported. The second-generation
	// literal is matched first so a verifier pinned to "v2" still evaluates
	// second-generation semantics; every other version string fails closed.
	var secondGeneration bool
	switch payload.Version {
	case eligibilityVersionV2:
		secondGeneration = true
	case v.version:
		secondGeneration = false
	default:
		return denied(EligibilityVersionMismatch)
	}
	if reason := validateSignedClientBinding(payload.Client, payload.AllowedClientVersions, v.client); reason != EligibilityAllowed {
		return denied(reason)
	}
	if payload.PolicyVersion == "" {
		return denied(EligibilityInvalidPolicy)
	}
	if payload.IssuedAt <= 0 {
		return denied(EligibilityInvalidIssuedAt)
	}
	if payload.ExpiresAt <= payload.IssuedAt {
		return denied(EligibilityInvalidExpiry)
	}

	nowUnix := v.now().UTC().Unix()
	if payload.IssuedAt > nowUnix {
		return denied(EligibilityFutureIssuedAt)
	}
	if payload.ExpiresAt <= nowUnix {
		return denied(EligibilityExpired)
	}
	if payload.ExpiresAt-payload.IssuedAt > int64(eligibilityGrantMaxTTL/time.Second) {
		return denied(EligibilityTTLExceeded)
	}

	countryCode, known := normalizeKnownCountryCode(payload.CountryCode)
	if !known {
		return denied(EligibilityUnknownRegion)
	}
	// Generation one keeps the historical global regional gates. Generation two
	// deliberately skips both: regional policy is the signed connector list plus
	// the runtime's per-connector CN floor, so a CN grant stays valid and the
	// verified CountryCode is exposed for that floor.
	if !secondGeneration {
		if countryCode == "CN" {
			return denied(EligibilityBlockedCN)
		}
		if !payload.RegionAllowed {
			return denied(EligibilityRegionNotAllowed)
		}
	}

	connectors, ok := validateConnectorIDs(payload.ConnectorIDs)
	if !ok {
		return denied(EligibilityInvalidConnectors)
	}
	return EligibilityDecision{
		Reason:        EligibilityAllowed,
		CountryCode:   countryCode,
		PolicyVersion: payload.PolicyVersion,
		IssuedAt:      payload.IssuedAt,
		ExpiresAt:     payload.ExpiresAt,
		allowed:       true,
		connectorIDs:  append([]string(nil), payload.ConnectorIDs...),
		connectors:    connectors,
	}
}

func validEligibilityClientBinding(client EligibilityClientBinding) bool {
	strictBase64URL := base64.RawURLEncoding.Strict()
	nonce, err := strictBase64URL.DecodeString(client.RequestNonce)
	if err != nil || len(nonce) != 32 || strictBase64URL.EncodeToString(nonce) != client.RequestNonce {
		clear(nonce)
		return false
	}
	clear(nonce)
	_, crabCodeOK := parseReleaseVersion(client.CrabCodeRelease)
	_, componentOK := parseReleaseVersion(client.AccountBridgeComponentVersion)
	return crabCodeOK && componentOK && client.AccountBridgeProtocolVersion > 0
}

func validInclusiveVersionRange(value InclusiveVersionRange) (releaseVersion, releaseVersion, bool) {
	minimum, minimumOK := parseReleaseVersion(value.MinimumInclusive)
	maximum, maximumOK := parseReleaseVersion(value.MaximumInclusive)
	return minimum, maximum, minimumOK && maximumOK && compareReleaseVersions(minimum, maximum) <= 0
}

func validateSignedClientBinding(signed EligibilityClientBinding, allowed AllowedClientVersions, expected EligibilityClientBinding) EligibilityReason {
	if !validEligibilityClientBinding(signed) {
		return EligibilityMalformedGrant
	}
	if signed.RequestNonce != expected.RequestNonce {
		return EligibilityRequestReplay
	}
	if signed.CrabCodeRelease != expected.CrabCodeRelease ||
		signed.AccountBridgeComponentVersion != expected.AccountBridgeComponentVersion ||
		signed.AccountBridgeProtocolVersion != expected.AccountBridgeProtocolVersion {
		return EligibilityClientMismatch
	}
	crabMinimum, crabMaximum, crabOK := validInclusiveVersionRange(allowed.CrabCodeRelease)
	componentMinimum, componentMaximum, componentOK := validInclusiveVersionRange(allowed.AccountBridgeComponentVersion)
	if !crabOK || !componentOK ||
		allowed.AccountBridgeProtocolVersion.MinimumInclusive <= 0 ||
		allowed.AccountBridgeProtocolVersion.MaximumInclusive < allowed.AccountBridgeProtocolVersion.MinimumInclusive {
		return EligibilityMalformedGrant
	}
	crabCodeRelease, _ := parseReleaseVersion(expected.CrabCodeRelease)
	componentVersion, _ := parseReleaseVersion(expected.AccountBridgeComponentVersion)
	if compareReleaseVersions(crabCodeRelease, crabMinimum) < 0 ||
		compareReleaseVersions(crabCodeRelease, crabMaximum) > 0 ||
		compareReleaseVersions(componentVersion, componentMinimum) < 0 ||
		compareReleaseVersions(componentVersion, componentMaximum) > 0 ||
		expected.AccountBridgeProtocolVersion < allowed.AccountBridgeProtocolVersion.MinimumInclusive ||
		expected.AccountBridgeProtocolVersion > allowed.AccountBridgeProtocolVersion.MaximumInclusive {
		return EligibilityClientUnsupported
	}
	return EligibilityAllowed
}

func decodeEligibilityPayload(raw []byte) (EligibilityPayload, error) {
	var payload EligibilityPayload
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&payload); err != nil {
		return EligibilityPayload{}, err
	}
	if err := decoder.Decode(&struct{}{}); err != io.EOF {
		if err == nil {
			return EligibilityPayload{}, errors.New("accountbridge: multiple eligibility payload values")
		}
		return EligibilityPayload{}, err
	}
	return payload, nil
}

func validateConnectorIDs(connectorIDs []string) (map[string]struct{}, bool) {
	if len(connectorIDs) == 0 || len(connectorIDs) > maxEligibilityConnectorCount {
		return nil, false
	}
	result := make(map[string]struct{}, len(connectorIDs))
	for _, connectorID := range connectorIDs {
		if !validConnectorID(connectorID) {
			return nil, false
		}
		if _, duplicate := result[connectorID]; duplicate {
			return nil, false
		}
		result[connectorID] = struct{}{}
	}
	return result, true
}

func validConnectorID(connectorID string) bool {
	if len(connectorID) == 0 || len(connectorID) > 64 {
		return false
	}
	for index, character := range []byte(connectorID) {
		isLower := character >= 'a' && character <= 'z'
		isDigit := character >= '0' && character <= '9'
		if isLower || isDigit || (index > 0 && (character == '.' || character == '_' || character == '-')) {
			continue
		}
		return false
	}
	return true
}

func normalizeKnownCountryCode(raw string) (string, bool) {
	code := strings.ToUpper(strings.TrimSpace(raw))
	if len(code) != 2 {
		return "", false
	}
	return code, strings.Contains(iso3166Alpha2Codes, " "+code+" ")
}

// This list validates that a two-letter result is an assigned ISO 3166-1
// alpha-2 country/region code. It is not the regional policy: RegionAllowed is
// the signed server decision, with CN additionally hard-blocked as defense in
// depth by the Account Bridge contract.
const iso3166Alpha2Codes = " AD AE AF AG AI AL AM AO AQ AR AS AT AU AW AX AZ BA BB BD BE BF BG BH BI BJ BL BM BN BO BQ BR BS BT BV BW BY BZ CA CC CD CF CG CH CI CK CL CM CN CO CR CU CV CW CX CY CZ DE DJ DK DM DO DZ EC EE EG EH ER ES ET FI FJ FK FM FO FR GA GB GD GE GF GG GH GI GL GM GN GP GQ GR GS GT GU GW GY HK HM HN HR HT HU ID IE IL IM IN IO IQ IR IS IT JE JM JO JP KE KG KH KI KM KN KP KR KW KY KZ LA LB LC LI LK LR LS LT LU LV LY MA MC MD ME MF MG MH MK ML MM MN MO MP MQ MR MS MT MU MV MW MX MY MZ NA NC NE NF NG NI NL NO NP NR NU NZ OM PA PE PF PG PH PK PL PM PN PR PS PT PW PY QA RE RO RS RU RW SA SB SC SD SE SG SH SI SJ SK SL SM SN SO SR SS ST SV SX SY SZ TC TD TF TG TH TJ TK TL TM TN TO TR TT TV TW TZ UA UG UM US UY UZ VA VC VE VG VI VN VU WF WS YE YT ZA ZM ZW "
