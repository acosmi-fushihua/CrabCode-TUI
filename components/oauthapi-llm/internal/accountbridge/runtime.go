package accountbridge

import (
	"errors"
	"sort"
	"strings"
	"sync"
	"unicode"
	"unicode/utf8"

	coreauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
)

const (
	// ProtocolVersion is the independently versioned private supervisor/sidecar
	// bootstrap and readiness contract.
	ProtocolVersion    = 1
	ConnectorOpenAI    = "openai"
	ConnectorAnthropic = "anthropic"
	ConnectorGoogle    = "google"
	ConnectorXAI       = "xai"
	ConnectorQwen      = "qwen"
	ConnectorKimi      = "kimi"
	ConnectorZai       = "zai"
	AuthModeBrowser    = "browser"
	AuthModeDeviceCode = "device-code"

	// RegionPolicyGlobal marks a connector directory entry as usable from every
	// verified region, including CN. RegionPolicyNonCN keeps the historical CN
	// floor for that connector: even full membership in a second-generation CN
	// grant cannot enable it.
	RegionPolicyGlobal = "global"
	RegionPolicyNonCN  = "non-cn"

	connectorDisplayNameMaxRunes = 80
)

type ConnectorSource struct {
	ConnectorID string
	ProviderID  string
}

// ConnectorPolicy is the control-worker projection of one independently
// signed connector-directory entry. A feature flag alone is insufficient:
// enabled requires signed-off terms, passed real-account conformance, and a
// fixed verified release artifact. RegionPolicy is mandatory and fail-closed:
// only "global" lifts the per-connector CN floor. The zero value is
// fail-closed.
type ConnectorPolicy struct {
	ConnectorID           string `json:"connectorId"`
	DisplayName           string `json:"displayName"`
	AuthMode              string `json:"authMode"`
	FeatureEnabled        bool   `json:"featureEnabled"`
	TermsStatus           string `json:"termsStatus"`
	ConformancePassed     bool   `json:"conformancePassed"`
	FixedArtifactVerified bool   `json:"fixedArtifactVerified"`
	RegionPolicy          string `json:"regionPolicy"`
}

type connectorPolicyState struct {
	enabled        bool
	displayName    string
	authMode       string
	termsStatus    string
	disabledReason string
	regionPolicy   string
}

var connectorAllowlist = []ConnectorSource{
	{ConnectorID: ConnectorOpenAI, ProviderID: "codex"},
	{ConnectorID: ConnectorAnthropic, ProviderID: "claude"},
	{ConnectorID: ConnectorGoogle, ProviderID: "gemini-cli"},
	{ConnectorID: ConnectorXAI, ProviderID: "xai"},
	{ConnectorID: ConnectorQwen, ProviderID: "qwen"},
	{ConnectorID: ConnectorKimi, ProviderID: "kimi"},
	{ConnectorID: ConnectorZai, ProviderID: "zai"},
}

// legacyGenerationConnectorIDs is the exact first-generation connector policy
// directory. A host that predates the qwen/kimi/zai connectors keeps sending
// this four-entry set; anything between the two published generations is a
// contradictory directory and rejects the whole bootstrap.
var legacyGenerationConnectorIDs = []string{
	ConnectorOpenAI,
	ConnectorAnthropic,
	ConnectorGoogle,
	ConnectorXAI,
}

// Runtime owns the fail-closed grant and exact route table used by the private
// Account Bridge facade. The signed grant is intentionally write-only outside
// this package: no accessor can accidentally serialize it.
type Runtime struct {
	verifier *EligibilityVerifier
	grant    SignedEligibilityGrant
	routes   *RouteStore
	policies map[string]connectorPolicyState

	mu          sync.RWMutex
	authManager *coreauth.Manager
}

func NewRuntime(verifier *EligibilityVerifier, grant SignedEligibilityGrant, routes *RouteStore) (*Runtime, error) {
	return NewRuntimeWithConnectorPolicies(verifier, grant, routes, nil)
}

// NewRuntimeWithConnectorPolicies constructs the production runtime from a
// verified regional grant plus independently verified connector policies.
// The bootstrap must contain exactly one published directory generation: the
// legacy four-entry set or the full allowlist. Missing, duplicate, malformed,
// mixed-generation, or contradictory entries reject the whole bootstrap rather
// than producing a partially labelled facade or widening access.
func NewRuntimeWithConnectorPolicies(verifier *EligibilityVerifier, grant SignedEligibilityGrant, routes *RouteStore, policies []ConnectorPolicy) (*Runtime, error) {
	if verifier == nil || routes == nil {
		return nil, errors.New("accountbridge: incomplete runtime")
	}
	if !verifier.Decide(grant).BootAllowed() {
		return nil, errors.New("accountbridge: eligibility denied")
	}
	parsed, err := parseConnectorPolicies(policies)
	if err != nil {
		return nil, err
	}
	return &Runtime{verifier: verifier, grant: grant, routes: routes, policies: parsed}, nil
}

func parseConnectorPolicies(policies []ConnectorPolicy) (map[string]connectorPolicyState, error) {
	if len(policies) != len(legacyGenerationConnectorIDs) && len(policies) != len(connectorAllowlist) {
		return nil, errors.New("accountbridge: connector policy directory is incomplete")
	}
	result := make(map[string]connectorPolicyState, len(connectorAllowlist))
	seen := make(map[string]struct{}, len(policies))
	for _, policy := range policies {
		connectorID := policy.ConnectorID
		if connectorID != strings.TrimSpace(connectorID) || connectorID != strings.ToLower(connectorID) {
			return nil, errors.New("accountbridge: connector policy contains an invalid connector id")
		}
		if _, ok := ProviderForConnector(connectorID); !ok {
			return nil, errors.New("accountbridge: connector policy contains an unknown connector")
		}
		if _, duplicate := seen[connectorID]; duplicate {
			return nil, errors.New("accountbridge: connector policy contains a duplicate connector")
		}
		seen[connectorID] = struct{}{}
		displayName := policy.DisplayName
		if !validConnectorDisplayName(displayName) {
			return nil, errors.New("accountbridge: connector policy has an invalid display name")
		}
		authMode := strings.TrimSpace(policy.AuthMode)
		if authMode != policy.AuthMode || (authMode != AuthModeBrowser && authMode != AuthModeDeviceCode) {
			return nil, errors.New("accountbridge: connector policy has an invalid auth mode")
		}
		termsStatus := strings.ToLower(strings.TrimSpace(policy.TermsStatus))
		if termsStatus != "blocked" && termsStatus != "signed-off" {
			return nil, errors.New("accountbridge: connector policy has an invalid terms status")
		}
		regionPolicy := policy.RegionPolicy
		if regionPolicy != RegionPolicyGlobal && regionPolicy != RegionPolicyNonCN {
			return nil, errors.New("accountbridge: connector policy has an invalid region policy")
		}
		state := connectorPolicyState{
			displayName:  displayName,
			authMode:     authMode,
			termsStatus:  termsStatus,
			regionPolicy: regionPolicy,
		}
		switch {
		case !policy.FixedArtifactVerified:
			state.disabledReason = "fixed_artifact_unverified"
			if connectorID == ConnectorGoogle {
				state.disabledReason = "fixed_plugin_missing"
			}
		case termsStatus != "signed-off":
			state.disabledReason = "terms_not_signed_off"
		case !policy.ConformancePassed:
			state.disabledReason = "conformance_not_passed"
		case !policy.FeatureEnabled:
			state.disabledReason = "feature_flag_disabled"
		default:
			state.enabled = true
		}
		result[connectorID] = state
	}
	// The id set must be exactly one published generation. A directory that
	// covers the full allowlist already proved that via per-entry allowlist
	// resolution plus the duplicate check; a four-entry directory must be the
	// exact legacy set, not an arbitrary allowlist subset.
	if len(seen) == len(legacyGenerationConnectorIDs) {
		for _, connectorID := range legacyGenerationConnectorIDs {
			if _, ok := seen[connectorID]; !ok {
				return nil, errors.New("accountbridge: connector policy directory is not a published generation")
			}
		}
	} else if len(seen) != len(connectorAllowlist) {
		return nil, errors.New("accountbridge: connector policy directory is incomplete")
	}
	return result, nil
}

func validConnectorDisplayName(displayName string) bool {
	if displayName == "" || displayName != strings.TrimSpace(displayName) || !utf8.ValidString(displayName) || utf8.RuneCountInString(displayName) > connectorDisplayNameMaxRunes {
		return false
	}
	for _, value := range displayName {
		if unicode.IsControl(value) || unicode.In(value, unicode.Cf) {
			return false
		}
	}
	return true
}

// ValidateConnectorPolicies applies the same exact-set and metadata checks at
// the private bootstrap DTO boundary before runtime construction.
func ValidateConnectorPolicies(policies []ConnectorPolicy) error {
	_, err := parseConnectorPolicies(policies)
	return err
}

// AttachAuthManager binds the server-owned manager after it is constructed.
// It is startup wiring only and does not expose credentials through the facade.
func (r *Runtime) AttachAuthManager(manager *coreauth.Manager) {
	if r == nil {
		return
	}
	r.mu.Lock()
	r.authManager = manager
	r.mu.Unlock()
}

func (r *Runtime) AuthManager() *coreauth.Manager {
	if r == nil {
		return nil
	}
	r.mu.RLock()
	defer r.mu.RUnlock()
	return r.authManager
}

func (r *Runtime) Routes() *RouteStore {
	if r == nil {
		return nil
	}
	return r.routes
}

func (r *Runtime) Decision() EligibilityDecision {
	if r == nil || r.verifier == nil {
		return EligibilityDecision{Reason: EligibilityMalformedGrant}
	}
	return r.verifier.Decide(r.grant)
}

func (r *Runtime) AllowsConnector(connectorID string) bool {
	return r.Decision().AllowsConnector(strings.TrimSpace(connectorID))
}

func (r *Runtime) ConnectorEnabled(connectorID string) bool {
	if r == nil {
		return false
	}
	decision := r.Decision()
	if !decision.AllowsConnector(strings.TrimSpace(connectorID)) {
		return false
	}
	policy, ok := r.policies[strings.ToLower(strings.TrimSpace(connectorID))]
	if !ok || !policy.enabled {
		return false
	}
	// Per-connector CN floor: inside CN only directory entries explicitly
	// marked global are usable, regardless of signed membership or the other
	// policy gates. Generation-one grants never verify for CN, so the floor is
	// vacuous for them.
	if decision.CountryCode == "CN" && policy.regionPolicy != RegionPolicyGlobal {
		return false
	}
	return true
}

func (r *Runtime) ConnectorTermsStatus(connectorID string) string {
	if r != nil {
		if policy, ok := r.policies[strings.ToLower(strings.TrimSpace(connectorID))]; ok && policy.termsStatus == "signed-off" {
			return "signed-off"
		}
	}
	return "blocked"
}

func (r *Runtime) ConnectorDisplayName(connectorID string) string {
	if r != nil {
		if policy, ok := r.policies[strings.ToLower(strings.TrimSpace(connectorID))]; ok {
			return policy.displayName
		}
	}
	return ""
}

func (r *Runtime) ConnectorAuthMode(connectorID string) string {
	if r != nil {
		if policy, ok := r.policies[strings.ToLower(strings.TrimSpace(connectorID))]; ok {
			return policy.authMode
		}
	}
	return ""
}

func (r *Runtime) ConnectorDisabledReason(connectorID string) string {
	if r == nil {
		return "connector_not_eligible"
	}
	decision := r.Decision()
	if !decision.AllowsConnector(strings.TrimSpace(connectorID)) {
		return "connector_not_eligible"
	}
	if policy, ok := r.policies[strings.ToLower(strings.TrimSpace(connectorID))]; ok {
		// The CN floor is a regional eligibility outcome, so it reuses the
		// existing wire reason instead of introducing a new code.
		if decision.CountryCode == "CN" && policy.regionPolicy != RegionPolicyGlobal {
			return "connector_not_eligible"
		}
		return policy.disabledReason
	}
	return "feature_flag_disabled"
}

func ConnectorIDs() []string {
	result := make([]string, 0, len(connectorAllowlist))
	for _, source := range connectorAllowlist {
		result = append(result, source.ConnectorID)
	}
	return result
}

func ConnectorForProvider(provider string) (string, bool) {
	provider = strings.ToLower(strings.TrimSpace(provider))
	for _, source := range connectorAllowlist {
		if source.ProviderID == provider {
			return source.ConnectorID, true
		}
	}
	return "", false
}

func ProviderForConnector(connectorID string) (string, bool) {
	connectorID = strings.ToLower(strings.TrimSpace(connectorID))
	for _, source := range connectorAllowlist {
		if source.ConnectorID == connectorID {
			return source.ProviderID, true
		}
	}
	return "", false
}

func SortedAuths(manager *coreauth.Manager) []*coreauth.Auth {
	if manager == nil {
		return nil
	}
	auths := manager.List()
	sort.Slice(auths, func(i, j int) bool { return auths[i].ID < auths[j].ID })
	return auths
}
