// Package quota provides read-only, provider-specific quota projections for
// Account Bridge. It deliberately keeps provider credentials out of cache keys,
// errors, and response values.
package quota

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/accountbridge"
	coreauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
	"golang.org/x/sync/singleflight"
)

const (
	DefaultCacheTTL      = 30 * time.Second
	defaultHTTPTimeout   = 10 * time.Second
	maxResponseBytes     = 1 << 20
	providerResetMinYear = 2000
	providerResetMaxYear = 2100
)

var (
	ErrMissingCredential = errors.New("account bridge quota: missing credential")
	ErrUnsupported       = errors.New("account bridge quota: unsupported connector")
	ErrMalformedResponse = errors.New("account bridge quota: malformed provider response")
)

// Window is one provider-defined quota window. Limit and Used may only be set
// when they describe the exact same provider window.
type Window struct {
	Label            string
	Limit            *float64
	Used             *float64
	RemainingPercent *float64
	ResetsAt         *time.Time
}

// Snapshot is a provider quota projection without route or account identity.
type Snapshot struct {
	State               string
	RemainingPercent    *float64
	LimitingWindowLabel *string
	ResetsAt            *time.Time
	Windows             []Window
	ObservedAt          time.Time
}

// Report contains either an account-wide snapshot or exact provider-declared
// per-model snapshots. Per-model data takes precedence over Account in
// ForModel.
// UnboundLimits retains provider-declared limit buckets whose applicability to
// a route is not established by the provider response. They are deliberately
// excluded from ForModel so a display name or metered feature is never guessed
// to be a model binding.
type Report struct {
	Account       *Snapshot
	Models        map[string]Snapshot
	UnboundLimits []UnboundLimitSnapshot
}

// UnboundLimitSnapshot is auditable provider quota detail that cannot safely
// be projected onto a route without a separate, exact provider binding.
type UnboundLimitSnapshot struct {
	LimitID   string
	LimitName string
	Snapshot  Snapshot
}

// ForModel returns an exact provider model match or the provider's account-wide
// result. It never derives applicability from a model display name, alias, or
// family token.
func (r Report) ForModel(modelID string) (Snapshot, bool) {
	snapshot, exactModel := r.Models[modelID]
	if exactModel {
		snapshot = cloneSnapshot(snapshot)
	} else {
		if r.Account == nil {
			return Snapshot{}, false
		}
		snapshot = cloneSnapshot(*r.Account)
	}
	if len(r.UnboundLimits) > 0 {
		// A provider-declared bucket without an exact route binding may be the
		// real limiting bucket. Preserve every real structured window for the
		// settings detail view, but label it as unbound and never include it in a
		// route aggregate. A confirmed account-wide exhaustion remains true;
		// otherwise applicability uncertainty makes the aggregate unknown.
		explicitExhausted := snapshot.State == "exhausted"
		for limitIndex, limit := range r.UnboundLimits {
			prefix := fmt.Sprintf("unbound-provider-limit-%d", limitIndex+1)
			if len(limit.Snapshot.Windows) == 0 {
				snapshot.Windows = append(snapshot.Windows, Window{Label: prefix})
				continue
			}
			for _, providerWindow := range limit.Snapshot.Windows {
				window := cloneWindow(providerWindow)
				if label := strings.TrimSpace(window.Label); label != "" {
					window.Label = prefix + "-" + label
				} else {
					window.Label = prefix
				}
				snapshot.Windows = append(snapshot.Windows, window)
			}
		}
		snapshot.RemainingPercent = nil
		snapshot.LimitingWindowLabel = nil
		snapshot.ResetsAt = nil
		if !explicitExhausted {
			snapshot.State = "unknown"
		}
	}
	return snapshot, true
}

// Endpoints are fixed provider endpoints. The exported shape exists so tests
// can use local servers; production uses DefaultEndpoints and does not derive a
// quota URL from credential metadata.
type Endpoints struct {
	OpenAI    string
	Anthropic string
	Google    string
	XAI       string
}

// DefaultEndpoints returns the audited, read-only provider endpoints.
func DefaultEndpoints() Endpoints {
	return Endpoints{
		OpenAI:    "https://chatgpt.com/backend-api/wham/usage",
		Anthropic: "https://api.anthropic.com/api/oauth/usage",
		Google:    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota",
		XAI:       "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
	}
}

// ClientFactory creates a proxy-aware transport for one credential.
type ClientFactory func(context.Context, *coreauth.Auth, time.Duration) *http.Client

// providerJSONValidator lets each provider reject ambiguous future quota
// shapes before encoding/json drops unknown fields. Validators may allow
// audited non-quota metadata, but an unknown quota/window/percent/reset field
// must fail closed so a partial response cannot overstate a route balance.
type providerJSONValidator interface {
	validateProviderJSON([]byte) error
}

// Option configures Service.
type Option func(*Service)

// WithClientFactory configures the transport factory.
func WithClientFactory(factory ClientFactory) Option {
	return func(service *Service) {
		if factory != nil {
			service.clientFactory = factory
		}
	}
}

// WithEndpoints replaces provider endpoints. It is intended for deterministic
// tests; callers should use the audited defaults in production.
func WithEndpoints(endpoints Endpoints) Option {
	return func(service *Service) { service.endpoints = endpoints }
}

// WithCacheTTL replaces the default 30-second successful-result TTL.
func WithCacheTTL(ttl time.Duration) Option {
	return func(service *Service) {
		if ttl >= 0 {
			service.cacheTTL = ttl
		}
	}
}

// WithClock configures a deterministic clock for tests.
func WithClock(now func() time.Time) Option {
	return func(service *Service) {
		if now != nil {
			service.now = now
		}
	}
}

type cacheEntry struct {
	report    Report
	expiresAt time.Time
}

// Service fetches and caches successful provider reports. Failed or malformed
// refreshes are returned to the caller so the API can fall back to runtime
// cooldown evidence. A failed forced refresh invalidates an earlier entry so a
// stale percentage cannot reappear on the next ordinary read.
type Service struct {
	clientFactory ClientFactory
	endpoints     Endpoints
	cacheTTL      time.Duration
	now           func() time.Time

	mu    sync.RWMutex
	cache map[string]cacheEntry
	group singleflight.Group
}

// NewService creates a quota service with fixed provider endpoints.
func NewService(options ...Option) *Service {
	service := &Service{
		clientFactory: func(_ context.Context, _ *coreauth.Auth, timeout time.Duration) *http.Client {
			return &http.Client{Timeout: timeout}
		},
		endpoints: DefaultEndpoints(),
		cacheTTL:  DefaultCacheTTL,
		now:       time.Now,
		cache:     make(map[string]cacheEntry),
	}
	for _, option := range options {
		option(service)
	}
	return service
}

// Read fetches one account-level provider report. forceRefresh bypasses the
// successful-result TTL; concurrent reads are still coalesced.
func (s *Service) Read(ctx context.Context, credential *coreauth.Auth, connectorID string, forceRefresh bool) (Report, error) {
	if s == nil || credential == nil || strings.TrimSpace(credential.ID) == "" {
		return Report{}, ErrMissingCredential
	}
	connectorID = strings.ToLower(strings.TrimSpace(connectorID))
	if !supportedConnector(connectorID) {
		return Report{}, ErrUnsupported
	}
	key := connectorID + "\x00" + credential.ID
	now := s.now().UTC()
	if !forceRefresh {
		if report, ok := s.cached(key, now); ok {
			return report, nil
		}
	}

	result := s.group.DoChan(key, func() (any, error) {
		// A second caller may have populated the cache while this caller waited.
		if !forceRefresh {
			if report, ok := s.cached(key, s.now().UTC()); ok {
				return report, nil
			}
		}
		report, err := s.fetch(ctx, credential, connectorID)
		if err != nil {
			if forceRefresh {
				s.mu.Lock()
				delete(s.cache, key)
				s.mu.Unlock()
			}
			return Report{}, err
		}
		observedAt := s.now().UTC()
		stampReport(&report, observedAt)
		s.mu.Lock()
		for cachedKey, entry := range s.cache {
			if !observedAt.Before(entry.expiresAt) {
				delete(s.cache, cachedKey)
			}
		}
		s.cache[key] = cacheEntry{report: cloneReport(report), expiresAt: observedAt.Add(s.cacheTTL)}
		s.mu.Unlock()
		return report, nil
	})
	select {
	case <-ctx.Done():
		return Report{}, ctx.Err()
	case completed := <-result:
		if completed.Err != nil {
			return Report{}, completed.Err
		}
		report, ok := completed.Val.(Report)
		if !ok {
			return Report{}, ErrMalformedResponse
		}
		return cloneReport(report), nil
	}
}

func supportedConnector(connectorID string) bool {
	switch connectorID {
	case accountbridge.ConnectorOpenAI, accountbridge.ConnectorAnthropic, accountbridge.ConnectorGoogle, accountbridge.ConnectorXAI:
		return true
	default:
		return false
	}
}

func (s *Service) cached(key string, now time.Time) (Report, bool) {
	s.mu.RLock()
	entry, ok := s.cache[key]
	s.mu.RUnlock()
	if !ok || !now.Before(entry.expiresAt) {
		return Report{}, false
	}
	return cloneReport(entry.report), true
}

func (s *Service) fetch(ctx context.Context, credential *coreauth.Auth, connectorID string) (Report, error) {
	client := s.clientFactory(ctx, credential, defaultHTTPTimeout)
	if client == nil {
		return Report{}, errors.New("account bridge quota: transport unavailable")
	}
	switch connectorID {
	case accountbridge.ConnectorOpenAI:
		return fetchOpenAI(ctx, client, s.endpoints.OpenAI, credential)
	case accountbridge.ConnectorAnthropic:
		return fetchAnthropic(ctx, client, s.endpoints.Anthropic, credential)
	case accountbridge.ConnectorGoogle:
		return fetchGoogle(ctx, client, s.endpoints.Google, credential)
	case accountbridge.ConnectorXAI:
		return fetchXAI(ctx, client, s.endpoints.XAI, credential)
	default:
		return Report{}, ErrUnsupported
	}
}

func requestJSON(ctx context.Context, client *http.Client, method, endpoint string, body any, headers http.Header, target any) error {
	if strings.TrimSpace(endpoint) == "" {
		return ErrUnsupported
	}
	var encoded io.Reader
	if body != nil {
		payload, err := json.Marshal(body)
		if err != nil {
			return ErrMalformedResponse
		}
		encoded = bytes.NewReader(payload)
	}
	request, err := http.NewRequestWithContext(ctx, method, endpoint, encoded)
	if err != nil {
		return errors.New("account bridge quota: invalid provider endpoint")
	}
	for name, values := range headers {
		for _, value := range values {
			request.Header.Add(name, value)
		}
	}
	request.Header.Set("Accept", "application/json")
	if body != nil {
		request.Header.Set("Content-Type", "application/json")
	}

	// Quota endpoints are fixed. Denying redirects prevents bearer tokens and
	// account headers from crossing an unexpected redirect boundary.
	safeClient := *client
	if safeClient.Timeout <= 0 || safeClient.Timeout > defaultHTTPTimeout {
		safeClient.Timeout = defaultHTTPTimeout
	}
	safeClient.CheckRedirect = func(_ *http.Request, _ []*http.Request) error { return http.ErrUseLastResponse }
	response, err := safeClient.Do(request)
	if err != nil {
		return errors.New("account bridge quota: provider request failed")
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return fmt.Errorf("account bridge quota: provider status %d", response.StatusCode)
	}
	limited := io.LimitReader(response.Body, maxResponseBytes+1)
	payload, err := io.ReadAll(limited)
	if err != nil || len(payload) > maxResponseBytes {
		return ErrMalformedResponse
	}
	if err := validateNoDuplicateJSONKeys(payload); err != nil {
		return ErrMalformedResponse
	}
	if validator, ok := target.(providerJSONValidator); ok {
		if err := validator.validateProviderJSON(payload); err != nil {
			return ErrMalformedResponse
		}
	}
	decoder := json.NewDecoder(bytes.NewReader(payload))
	if err := decoder.Decode(target); err != nil {
		return ErrMalformedResponse
	}
	var trailing any
	if err := decoder.Decode(&trailing); !errors.Is(err, io.EOF) {
		return ErrMalformedResponse
	}
	return nil
}

func validateNoDuplicateJSONKeys(payload []byte) error {
	decoder := json.NewDecoder(bytes.NewReader(payload))
	decoder.UseNumber()
	if err := consumeUniqueJSONValue(decoder); err != nil {
		return ErrMalformedResponse
	}
	if _, err := decoder.Token(); !errors.Is(err, io.EOF) {
		return ErrMalformedResponse
	}
	return nil
}

func consumeUniqueJSONValue(decoder *json.Decoder) error {
	token, err := decoder.Token()
	if err != nil {
		return err
	}
	delimiter, isDelimiter := token.(json.Delim)
	if !isDelimiter {
		return nil
	}
	switch delimiter {
	case '{':
		seen := make(map[string]struct{})
		for decoder.More() {
			keyToken, errKey := decoder.Token()
			if errKey != nil {
				return errKey
			}
			key, ok := keyToken.(string)
			if !ok {
				return ErrMalformedResponse
			}
			if _, duplicate := seen[key]; duplicate {
				return ErrMalformedResponse
			}
			seen[key] = struct{}{}
			if errValue := consumeUniqueJSONValue(decoder); errValue != nil {
				return errValue
			}
		}
		closing, errClosing := decoder.Token()
		if errClosing != nil || closing != json.Delim('}') {
			return ErrMalformedResponse
		}
	case '[':
		for decoder.More() {
			if errValue := consumeUniqueJSONValue(decoder); errValue != nil {
				return errValue
			}
		}
		closing, errClosing := decoder.Token()
		if errClosing != nil || closing != json.Delim(']') {
			return ErrMalformedResponse
		}
	default:
		return ErrMalformedResponse
	}
	return nil
}

func decodeJSONObject(raw json.RawMessage) (map[string]json.RawMessage, error) {
	if len(bytes.TrimSpace(raw)) == 0 || rawJSONNull(raw) {
		return nil, nil
	}
	var object map[string]json.RawMessage
	if err := json.Unmarshal(raw, &object); err != nil || object == nil {
		return nil, ErrMalformedResponse
	}
	return object, nil
}

func rawJSONNull(raw json.RawMessage) bool {
	return bytes.Equal(bytes.TrimSpace(raw), []byte("null"))
}

func rejectUnknownQuotaShapedFields(object map[string]json.RawMessage, allowed ...string) error {
	allowedSet := make(map[string]struct{}, len(allowed))
	for _, field := range allowed {
		allowedSet[field] = struct{}{}
	}
	for field := range object {
		if _, ok := allowedSet[field]; ok {
			continue
		}
		if quotaShapedFieldName(field) {
			return ErrMalformedResponse
		}
		if err := rejectNestedQuotaShapedFields(object[field]); err != nil {
			return err
		}
	}
	return nil
}

func rejectNestedQuotaShapedFields(raw json.RawMessage) error {
	if len(bytes.TrimSpace(raw)) == 0 || rawJSONNull(raw) {
		return nil
	}
	var object map[string]json.RawMessage
	if err := json.Unmarshal(raw, &object); err == nil && object != nil {
		for field, value := range object {
			if quotaShapedFieldName(field) {
				return ErrMalformedResponse
			}
			if errNested := rejectNestedQuotaShapedFields(value); errNested != nil {
				return errNested
			}
		}
		return nil
	}
	var array []json.RawMessage
	if err := json.Unmarshal(raw, &array); err == nil && array != nil {
		for _, value := range array {
			if errNested := rejectNestedQuotaShapedFields(value); errNested != nil {
				return errNested
			}
		}
	}
	return nil
}

func quotaShapedFieldName(field string) bool {
	var normalized strings.Builder
	for _, character := range strings.ToLower(strings.TrimSpace(field)) {
		if character >= 'a' && character <= 'z' {
			normalized.WriteRune(character)
		}
	}
	name := normalized.String()
	for _, marker := range []string{
		"quota", "ratelimit", "window", "period", "bucket", "percent",
		"fraction", "reset", "remaining", "limit", "usage", "used",
		"spend", "credit", "allowed", "reached", "exhaust", "month",
	} {
		if strings.Contains(name, marker) {
			return true
		}
	}
	return false
}

func bearerHeaders(token string) http.Header {
	headers := make(http.Header)
	headers.Set("Authorization", "Bearer "+token)
	return headers
}

func metadataString(credential *coreauth.Auth, keys ...string) string {
	if credential == nil {
		return ""
	}
	for _, key := range keys {
		if value, ok := credential.Metadata[key].(string); ok && strings.TrimSpace(value) != "" {
			return strings.TrimSpace(value)
		}
	}
	for _, containerKey := range []string{"token", "tokens"} {
		container, ok := credential.Metadata[containerKey].(map[string]any)
		if !ok {
			continue
		}
		for _, key := range keys {
			if value, ok := container[key].(string); ok && strings.TrimSpace(value) != "" {
				return strings.TrimSpace(value)
			}
		}
	}
	return ""
}

func credentialValue(credential *coreauth.Auth, keys ...string) string {
	if value := metadataString(credential, keys...); value != "" {
		return value
	}
	if credential == nil {
		return ""
	}
	for _, key := range keys {
		if value := strings.TrimSpace(credential.Attributes[key]); value != "" {
			return value
		}
	}
	return ""
}

func remainingFromUsedPercent(used float64) *float64 {
	if math.IsNaN(used) || math.IsInf(used, 0) {
		return nil
	}
	remaining := clampPercent(100 - used)
	return &remaining
}

func strictProviderRemainingFromUsedPercent(used float64) (*float64, error) {
	if math.IsNaN(used) || math.IsInf(used, 0) || used < 0 || used > 100 {
		return nil, ErrMalformedResponse
	}
	return remainingFromUsedPercent(used), nil
}

func strictProviderRemainingPercent(remaining float64) (*float64, error) {
	if math.IsNaN(remaining) || math.IsInf(remaining, 0) || remaining < 0 || remaining > 100 {
		return nil, ErrMalformedResponse
	}
	return directRemainingPercent(remaining), nil
}

func strictProviderRemainingFraction(remaining float64) (*float64, error) {
	if math.IsNaN(remaining) || math.IsInf(remaining, 0) || remaining < 0 || remaining > 1 {
		return nil, ErrMalformedResponse
	}
	return directRemainingPercent(remaining * 100), nil
}

func directRemainingPercent(remaining float64) *float64 {
	if math.IsNaN(remaining) || math.IsInf(remaining, 0) {
		return nil
	}
	remaining = clampPercent(remaining)
	return &remaining
}

func clampPercent(value float64) float64 {
	if value < 0 {
		return 0
	}
	if value > 100 {
		return 100
	}
	return value
}

func normalizeSnapshot(windows []Window, explicitExhausted bool) Snapshot {
	normalized := make([]Window, 0, len(windows))
	var limiting *Window
	hasUnknownWindow := false
	for _, input := range windows {
		window := cloneWindow(input)
		window.Label = strings.TrimSpace(window.Label)
		if window.Label == "" {
			window.Label = "quota"
		}
		if window.RemainingPercent != nil {
			window.RemainingPercent = directRemainingPercent(*window.RemainingPercent)
		} else if window.Limit != nil && window.Used != nil && *window.Limit > 0 && *window.Used >= 0 && !math.IsNaN(*window.Limit) && !math.IsNaN(*window.Used) && !math.IsInf(*window.Limit, 0) && !math.IsInf(*window.Used, 0) {
			calculated := math.Floor(clampPercent(((*window.Limit - *window.Used) / *window.Limit) * 100))
			window.RemainingPercent = &calculated
		}
		normalized = append(normalized, window)
		if window.RemainingPercent == nil {
			hasUnknownWindow = true
		} else if limiting == nil || *window.RemainingPercent < *limiting.RemainingPercent {
			candidate := window
			limiting = &candidate
		}
	}

	snapshot := Snapshot{State: "unknown", Windows: normalized}
	// A partially known multi-window quota cannot safely expose an aggregate:
	// the unknown window may be the actual limiting window. Individual known
	// windows remain visible in the detailed response.
	if limiting != nil && !hasUnknownWindow {
		remaining := *limiting.RemainingPercent
		label := limiting.Label
		snapshot.RemainingPercent = &remaining
		snapshot.LimitingWindowLabel = &label
		if limiting.ResetsAt != nil {
			reset := limiting.ResetsAt.UTC()
			snapshot.ResetsAt = &reset
		}
		if remaining <= 0 {
			snapshot.State = "exhausted"
		} else {
			snapshot.State = "available"
		}
	}
	if explicitExhausted {
		snapshot.State = "exhausted"
	}
	return snapshot
}

func parseReset(value string) *time.Time {
	value = strings.TrimSpace(value)
	if value == "" {
		return nil
	}
	parsed, err := time.Parse(time.RFC3339Nano, value)
	if err != nil {
		return nil
	}
	parsed = parsed.UTC()
	return &parsed
}

func strictProviderReset(value string) (*time.Time, error) {
	value = strings.TrimSpace(value)
	if value == "" {
		return nil, nil
	}
	parsed, err := time.Parse(time.RFC3339Nano, value)
	if err != nil || !plausibleProviderReset(parsed) {
		return nil, ErrMalformedResponse
	}
	parsed = parsed.UTC()
	return &parsed, nil
}

func strictProviderUnixReset(value *int64) (*time.Time, error) {
	if value == nil {
		return nil, nil
	}
	if *value <= 0 {
		return nil, ErrMalformedResponse
	}
	parsed := time.Unix(*value, 0).UTC()
	if !plausibleProviderReset(parsed) {
		return nil, ErrMalformedResponse
	}
	return &parsed, nil
}

func plausibleProviderReset(value time.Time) bool {
	year := value.UTC().Year()
	return year >= providerResetMinYear && year <= providerResetMaxYear
}

func unixReset(value *int64) *time.Time {
	if value == nil || *value <= 0 {
		return nil
	}
	parsed := time.Unix(*value, 0).UTC()
	return &parsed
}

func stampReport(report *Report, observedAt time.Time) {
	if report.Account != nil {
		report.Account.ObservedAt = observedAt
	}
	for modelID, snapshot := range report.Models {
		snapshot.ObservedAt = observedAt
		report.Models[modelID] = snapshot
	}
	for index := range report.UnboundLimits {
		report.UnboundLimits[index].Snapshot.ObservedAt = observedAt
	}
}

func cloneReport(report Report) Report {
	copyReport := Report{}
	if report.Account != nil {
		snapshot := cloneSnapshot(*report.Account)
		copyReport.Account = &snapshot
	}
	if report.Models != nil {
		copyReport.Models = make(map[string]Snapshot, len(report.Models))
		for modelID, snapshot := range report.Models {
			copyReport.Models[modelID] = cloneSnapshot(snapshot)
		}
	}
	if report.UnboundLimits != nil {
		copyReport.UnboundLimits = make([]UnboundLimitSnapshot, len(report.UnboundLimits))
		for index, limit := range report.UnboundLimits {
			copyReport.UnboundLimits[index] = UnboundLimitSnapshot{
				LimitID:   limit.LimitID,
				LimitName: limit.LimitName,
				Snapshot:  cloneSnapshot(limit.Snapshot),
			}
		}
	}
	return copyReport
}

func cloneSnapshot(snapshot Snapshot) Snapshot {
	copySnapshot := snapshot
	copySnapshot.RemainingPercent = cloneFloat(snapshot.RemainingPercent)
	copySnapshot.LimitingWindowLabel = cloneString(snapshot.LimitingWindowLabel)
	copySnapshot.ResetsAt = cloneTime(snapshot.ResetsAt)
	copySnapshot.Windows = make([]Window, len(snapshot.Windows))
	for index, window := range snapshot.Windows {
		copySnapshot.Windows[index] = cloneWindow(window)
	}
	return copySnapshot
}

func cloneWindow(window Window) Window {
	window.Limit = cloneFloat(window.Limit)
	window.Used = cloneFloat(window.Used)
	window.RemainingPercent = cloneFloat(window.RemainingPercent)
	window.ResetsAt = cloneTime(window.ResetsAt)
	return window
}

func cloneFloat(value *float64) *float64 {
	if value == nil {
		return nil
	}
	copyValue := *value
	return &copyValue
}

func cloneString(value *string) *string {
	if value == nil {
		return nil
	}
	copyValue := *value
	return &copyValue
}

func cloneTime(value *time.Time) *time.Time {
	if value == nil {
		return nil
	}
	copyValue := *value
	return &copyValue
}
