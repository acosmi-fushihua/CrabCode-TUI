package main

import (
	"bytes"
	"crypto/ed25519"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"

	"github.com/acosmi/OAuthAPI-LLM/internal/accountbridge"
)

const (
	accountBridgeBootstrapFD       = uintptr(3)
	accountBridgeBootstrapMaxBytes = 64 << 10
	accountBridgeGrantAudience     = "crabcode-account-bridge"
	accountBridgeGrantVersion      = "v1"
)

// AccountBridgeEligibilityPublicKeyBase64URL and
// AccountBridgeCrabCodeRelease are trusted release-time inputs. Production
// builds set them with -ldflags -X; neither may come from the same bootstrap
// channel as the signed grant.
var (
	AccountBridgeEligibilityPublicKeyBase64URL string
	AccountBridgeCrabCodeRelease               string
)

type accountBridgeBootstrap struct {
	MasterKey []byte
	Grant     accountbridge.SignedEligibilityGrant
	Verifier  *accountbridge.EligibilityVerifier
	Policies  []accountbridge.ConnectorPolicy
}

type accountBridgeBootstrapWire struct {
	MasterKeyBase64URL string                               `json:"masterKey"`
	RequestNonce       string                               `json:"requestNonce"`
	Grant              accountbridge.SignedEligibilityGrant `json:"eligibilityGrant"`
	ConnectorPolicies  []accountbridge.ConnectorPolicy      `json:"connectorPolicies,omitempty"`
}

func readAccountBridgeBootstrapFD(fd uintptr, trustRoot string) (*accountBridgeBootstrap, error) {
	if !accountBridgeDescriptorIsOpen(fd) {
		return nil, errors.New("account bridge bootstrap is unavailable")
	}
	file := os.NewFile(fd, "account-bridge-bootstrap")
	if file == nil {
		return nil, errors.New("account bridge bootstrap is unavailable")
	}
	defer file.Close()
	return decodeAccountBridgeBootstrap(file, trustRoot)
}

func decodeAccountBridgeBootstrap(reader io.Reader, trustRoot string) (*accountBridgeBootstrap, error) {
	if reader == nil {
		return nil, errors.New("account bridge bootstrap is unavailable")
	}
	raw, err := io.ReadAll(io.LimitReader(reader, accountBridgeBootstrapMaxBytes+1))
	if err != nil || len(raw) == 0 || len(raw) > accountBridgeBootstrapMaxBytes {
		clear(raw)
		return nil, errors.New("account bridge bootstrap is invalid")
	}
	defer clear(raw)
	var wire accountBridgeBootstrapWire
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.DisallowUnknownFields()
	if err = decoder.Decode(&wire); err != nil {
		return nil, errors.New("account bridge bootstrap is invalid")
	}
	if err = decoder.Decode(&struct{}{}); !errors.Is(err, io.EOF) {
		return nil, errors.New("account bridge bootstrap is invalid")
	}

	strictBase64URL := base64.RawURLEncoding.Strict()
	masterKey, err := strictBase64URL.DecodeString(wire.MasterKeyBase64URL)
	wire.MasterKeyBase64URL = ""
	if err != nil || len(masterKey) != 32 {
		clear(masterKey)
		return nil, errors.New("account bridge bootstrap key is invalid")
	}
	publicKey, err := strictBase64URL.DecodeString(trustRoot)
	if err != nil || len(publicKey) != ed25519.PublicKeySize {
		clear(masterKey)
		return nil, errors.New("account bridge eligibility trust root is invalid")
	}
	verifier, err := accountbridge.NewEligibilityVerifier(
		ed25519.PublicKey(publicKey),
		accountBridgeGrantAudience,
		accountBridgeGrantVersion,
		accountbridge.EligibilityClientBinding{
			RequestNonce:                  wire.RequestNonce,
			CrabCodeRelease:               AccountBridgeCrabCodeRelease,
			AccountBridgeComponentVersion: Version,
			AccountBridgeProtocolVersion:  accountbridge.ProtocolVersion,
		},
	)
	clear(publicKey)
	if err != nil {
		clear(masterKey)
		return nil, fmt.Errorf("account bridge eligibility verifier: %w", err)
	}
	// Boot-level gate: the grant must be valid for its generation and name at
	// least one connector. Per-connector reachability is enforced later by the
	// runtime (membership, policy gates, CN floor).
	if !verifier.Decide(wire.Grant).BootAllowed() {
		clear(masterKey)
		return nil, errors.New("account bridge eligibility denied")
	}
	if err = accountbridge.ValidateConnectorPolicies(wire.ConnectorPolicies); err != nil {
		clear(masterKey)
		return nil, fmt.Errorf("account bridge connector policies: %w", err)
	}
	return &accountBridgeBootstrap{MasterKey: masterKey, Grant: wire.Grant, Verifier: verifier, Policies: wire.ConnectorPolicies}, nil
}

func clearSecret(secret []byte) {
	clear(secret)
}
