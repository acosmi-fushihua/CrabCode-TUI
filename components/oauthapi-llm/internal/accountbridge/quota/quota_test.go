package quota

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/accountbridge"
	coreauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
)

func TestProviderQuotaFixtures(t *testing.T) {
	fixtures := map[string][]byte{}
	for _, name := range []string{"openai-usage.json", "anthropic-usage.json", "google-user-quota.json", "xai-billing.json"} {
		payload, err := os.ReadFile("testdata/" + name)
		if err != nil {
			t.Fatalf("read fixture %s: %v", name, err)
		}
		fixtures[name] = payload
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/openai", func(writer http.ResponseWriter, request *http.Request) {
		if request.Method != http.MethodGet || request.Header.Get("Authorization") != "Bearer openai-secret" || request.Header.Get("ChatGPT-Account-Id") != "acct-sanitized" || request.Header.Get("User-Agent") != "codex-cli" {
			t.Errorf("OpenAI request method=%s headers=%v", request.Method, sanitizedHeaders(request.Header))
		}
		_, _ = writer.Write(fixtures["openai-usage.json"])
	})
	mux.HandleFunc("/anthropic", func(writer http.ResponseWriter, request *http.Request) {
		if request.Method != http.MethodGet || request.Header.Get("Authorization") != "Bearer anthropic-secret" || request.Header.Get("anthropic-beta") != "oauth-2025-04-20" {
			t.Errorf("Anthropic request method=%s headers=%v", request.Method, sanitizedHeaders(request.Header))
		}
		_, _ = writer.Write(fixtures["anthropic-usage.json"])
	})
	mux.HandleFunc("/google", func(writer http.ResponseWriter, request *http.Request) {
		if request.Method != http.MethodPost || request.Header.Get("Authorization") != "Bearer google-secret" {
			t.Errorf("Google request method=%s headers=%v", request.Method, sanitizedHeaders(request.Header))
		}
		var body map[string]string
		if err := json.NewDecoder(request.Body).Decode(&body); err != nil || body["project"] != "projects/sanitized-project" {
			t.Errorf("Google body=%v err=%v", body, err)
		}
		_, _ = writer.Write(fixtures["google-user-quota.json"])
	})
	mux.HandleFunc("/xai", func(writer http.ResponseWriter, request *http.Request) {
		if request.Method != http.MethodGet || request.URL.RawQuery != "format=credits" || request.Header.Get("Authorization") != "Bearer xai-secret" || request.Header.Get("x-grok-client-mode") != "billing" || request.Header.Get("x-grok-client-version") != "0.2.101" {
			t.Errorf("xAI request method=%s query=%s headers=%v", request.Method, request.URL.RawQuery, sanitizedHeaders(request.Header))
		}
		_, _ = writer.Write(fixtures["xai-billing.json"])
	})
	server := httptest.NewServer(mux)
	t.Cleanup(server.Close)
	var credentialEndpointHits atomic.Int32
	credentialEndpoint := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
		credentialEndpointHits.Add(1)
		_, _ = io.WriteString(writer, `{}`)
	}))
	t.Cleanup(credentialEndpoint.Close)

	service := NewService(
		WithEndpoints(Endpoints{
			OpenAI:    server.URL + "/openai",
			Anthropic: server.URL + "/anthropic",
			Google:    server.URL + "/google",
			XAI:       server.URL + "/xai?format=credits",
		}),
		WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
	)

	tests := []struct {
		name          string
		connectorID   string
		credential    *coreauth.Auth
		modelID       string
		wantRemaining float64
		wantLabel     string
		wantReset     string
		wantWindows   int
		wantUnknown   bool
	}{
		{
			name: "openai multi-window minimum", connectorID: accountbridge.ConnectorOpenAI,
			credential: &coreauth.Auth{ID: "openai-auth", Attributes: map[string]string{"base_url": credentialEndpoint.URL}, Metadata: map[string]any{"access_token": "openai-secret", "account_id": "acct-sanitized"}},
			modelID:    "model-alpha", wantWindows: 4, wantUnknown: true,
		},
		{
			name: "anthropic utilization", connectorID: accountbridge.ConnectorAnthropic,
			credential: &coreauth.Auth{ID: "anthropic-auth", Metadata: map[string]any{"access_token": "anthropic-secret"}},
			modelID:    "model-alpha", wantWindows: 4, wantUnknown: true,
		},
		{
			name: "google exact model bucket", connectorID: accountbridge.ConnectorGoogle,
			credential: &coreauth.Auth{ID: "google-auth", Metadata: map[string]any{"access_token": "google-secret", "project_id": "projects/sanitized-project"}},
			modelID:    "model-alpha", wantRemaining: 75, wantLabel: "requests", wantReset: "2026-07-15T00:00:00Z", wantWindows: 2,
		},
		{
			name: "xai billing cycle", connectorID: accountbridge.ConnectorXAI,
			credential: &coreauth.Auth{ID: "xai-auth", Metadata: map[string]any{"access_token": "xai-secret"}},
			modelID:    "model-alpha", wantRemaining: 38.5, wantLabel: "billing-cycle", wantReset: "2026-08-01T00:00:00Z", wantWindows: 1,
		},
	}
	for _, testCase := range tests {
		t.Run(testCase.name, func(t *testing.T) {
			report, err := service.Read(context.Background(), testCase.credential, testCase.connectorID, false)
			if err != nil {
				t.Fatalf("Read: %v", err)
			}
			snapshot, ok := report.ForModel(testCase.modelID)
			if testCase.wantUnknown {
				if !ok || snapshot.State != "unknown" || snapshot.RemainingPercent != nil || snapshot.LimitingWindowLabel != nil || snapshot.ResetsAt != nil {
					t.Fatalf("snapshot=%+v ok=%v want fail-safe unknown", snapshot, ok)
				}
			} else {
				if !ok || snapshot.State != "available" || snapshot.RemainingPercent == nil || *snapshot.RemainingPercent != testCase.wantRemaining {
					t.Fatalf("snapshot=%+v ok=%v", snapshot, ok)
				}
				if snapshot.LimitingWindowLabel == nil || *snapshot.LimitingWindowLabel != testCase.wantLabel {
					t.Fatalf("limiting label=%v want=%s", snapshot.LimitingWindowLabel, testCase.wantLabel)
				}
				if snapshot.ResetsAt == nil || snapshot.ResetsAt.Format(time.RFC3339) != testCase.wantReset {
					t.Fatalf("reset=%v want=%s", snapshot.ResetsAt, testCase.wantReset)
				}
			}
			if len(snapshot.Windows) != testCase.wantWindows {
				t.Fatalf("windows=%d want=%d: %+v", len(snapshot.Windows), testCase.wantWindows, snapshot.Windows)
			}
		})
	}

	openAIReport, err := service.Read(context.Background(), tests[0].credential, accountbridge.ConnectorOpenAI, false)
	if err != nil {
		t.Fatalf("read cached OpenAI report: %v", err)
	}
	if openAIReport.Account == nil || openAIReport.Account.State != "available" || openAIReport.Account.RemainingPercent == nil || *openAIReport.Account.RemainingPercent != 35 || len(openAIReport.Account.Windows) != 3 {
		t.Fatalf("OpenAI confirmed global snapshot=%+v", openAIReport.Account)
	}
	if len(openAIReport.UnboundLimits) != 1 {
		t.Fatalf("OpenAI unbound limits=%d want=1: %+v", len(openAIReport.UnboundLimits), openAIReport.UnboundLimits)
	}
	additional := openAIReport.UnboundLimits[0]
	if additional.LimitID != "codex_other" || additional.LimitName != "codex_other" || additional.Snapshot.RemainingPercent == nil || *additional.Snapshot.RemainingPercent != 12 {
		t.Fatalf("OpenAI additional limit was not retained as independent audit detail: %+v", additional)
	}

	googleReport, err := service.Read(context.Background(), tests[2].credential, accountbridge.ConnectorGoogle, false)
	if err != nil {
		t.Fatalf("read cached Google report: %v", err)
	}
	unknownLimit, ok := googleReport.ForModel("model-unknown-limit")
	if !ok || unknownLimit.State != "unknown" || unknownLimit.RemainingPercent != nil || unknownLimit.Windows[0].Limit != nil || unknownLimit.Windows[0].Used != nil {
		t.Fatalf("remainingAmount was incorrectly extrapolated: snapshot=%+v ok=%v", unknownLimit, ok)
	}
	if _, ok := googleReport.ForModel("model-not-in-response"); ok {
		t.Fatal("Google adapter guessed an absent model alias")
	}
	anthropicReport, err := service.Read(context.Background(), tests[1].credential, accountbridge.ConnectorAnthropic, false)
	if err != nil {
		t.Fatalf("read cached Anthropic report: %v", err)
	}
	if len(anthropicReport.UnboundLimits) != 2 || anthropicReport.UnboundLimits[0].LimitID != "seven_day_sonnet" || anthropicReport.UnboundLimits[0].Snapshot.RemainingPercent == nil || *anthropicReport.UnboundLimits[0].Snapshot.RemainingPercent != 70 {
		t.Fatalf("Anthropic provider bucket was not retained as unbound audit detail: %+v", anthropicReport.UnboundLimits)
	}
	extraUsage := anthropicReport.UnboundLimits[1]
	if extraUsage.LimitID != "extra_usage" || extraUsage.Snapshot.RemainingPercent == nil || *extraUsage.Snapshot.RemainingPercent != 87 || len(extraUsage.Snapshot.Windows) != 1 || extraUsage.Snapshot.Windows[0].Limit == nil || *extraUsage.Snapshot.Windows[0].Limit != 1000 || extraUsage.Snapshot.Windows[0].Used == nil || *extraUsage.Snapshot.Windows[0].Used != 125.5 {
		t.Fatalf("Anthropic structured extra-usage detail was lost or projected as a route binding: %+v", extraUsage)
	}
	arbitrarySnapshot, ok := anthropicReport.ForModel("model-not-in-response")
	if !ok || arbitrarySnapshot.State != "unknown" || arbitrarySnapshot.RemainingPercent != nil || len(arbitrarySnapshot.Windows) != 4 || arbitrarySnapshot.Windows[2].Label != "unbound-provider-limit-1-seven_day_sonnet" || arbitrarySnapshot.Windows[2].RemainingPercent == nil || *arbitrarySnapshot.Windows[2].RemainingPercent != 70 || arbitrarySnapshot.Windows[3].Label != "unbound-provider-limit-2-extra_usage" {
		t.Fatalf("Anthropic provider bucket was guessed onto a route: snapshot=%+v ok=%v", arbitrarySnapshot, ok)
	}
	if credentialEndpointHits.Load() != 0 {
		t.Fatalf("credential-controlled base_url was used %d time(s)", credentialEndpointHits.Load())
	}
}

func TestNormalizeSnapshotSameWindowFormulaAndMinimum(t *testing.T) {
	limit, used := 300.0, 98.0
	direct := 72.5
	snapshot := normalizeSnapshot([]Window{
		{Label: "same-window", Limit: &limit, Used: &used},
		{Label: "direct", RemainingPercent: &direct},
	}, false)
	if snapshot.RemainingPercent == nil || *snapshot.RemainingPercent != 67 || snapshot.LimitingWindowLabel == nil || *snapshot.LimitingWindowLabel != "same-window" {
		t.Fatalf("snapshot=%+v", snapshot)
	}
	missingLimit := normalizeSnapshot([]Window{{Label: "unpaired", Used: &used}}, false)
	if missingLimit.State != "unknown" || missingLimit.RemainingPercent != nil {
		t.Fatalf("unpaired usage was extrapolated: %+v", missingLimit)
	}
	overused := 150.0
	clamped := normalizeSnapshot([]Window{{Label: "direct-used", RemainingPercent: remainingFromUsedPercent(overused)}}, false)
	if clamped.State != "exhausted" || clamped.RemainingPercent == nil || *clamped.RemainingPercent != 0 {
		t.Fatalf("direct percentage was not clamped: %+v", clamped)
	}
	partiallyKnown := normalizeSnapshot([]Window{
		{Label: "known", RemainingPercent: &direct},
		{Label: "unknown"},
	}, false)
	if partiallyKnown.State != "unknown" || partiallyKnown.RemainingPercent != nil || partiallyKnown.LimitingWindowLabel != nil {
		t.Fatalf("partially known windows fabricated an aggregate: %+v", partiallyKnown)
	}
}

func TestXAIWindowRepresentationsAreNeverMixed(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
		_, _ = io.WriteString(writer, `{
			"creditUsagePercent": 90,
			"end": "2026-08-01T00:00:00Z",
			"currentPeriod": {"end": "2026-08-02T00:00:00Z"}
		}`)
	}))
	t.Cleanup(server.Close)
	service := NewService(
		WithEndpoints(Endpoints{XAI: server.URL}),
		WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
	)
	_, err := service.Read(context.Background(), &coreauth.Auth{ID: "xai-shape", Metadata: map[string]any{"access_token": "secret"}}, accountbridge.ConnectorXAI, true)
	if !errors.Is(err, ErrMalformedResponse) {
		t.Fatalf("mixed top-level/currentPeriod shape err=%v want ErrMalformedResponse", err)
	}
}

func TestOpenAIOfficialUsageExtensionsDoNotGuessRouteBindings(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
		_, _ = io.WriteString(writer, `{
			"plan_type":"pro",
			"rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":40,"reset_at":1784062800}},
			"spend_control":{"reached":false,"individual_limit":{"remaining_percent":70,"reset_at":1784145600}},
			"additional_rate_limits":[{
				"limit_name":"model-alpha",
				"metered_feature":"codex_other",
				"rate_limit":{"allowed":false,"limit_reached":true,"primary_window":{"used_percent":99,"reset_at":1784066400}}
			}]
		}`)
	}))
	t.Cleanup(server.Close)
	service := NewService(
		WithEndpoints(Endpoints{OpenAI: server.URL}),
		WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
	)
	report, err := service.Read(context.Background(), &coreauth.Auth{ID: "openai-extended", Metadata: map[string]any{"access_token": "secret"}}, accountbridge.ConnectorOpenAI, true)
	if err != nil {
		t.Fatalf("Read: %v", err)
	}
	snapshot, ok := report.ForModel("model-alpha")
	if !ok || snapshot.State != "unknown" || snapshot.RemainingPercent != nil || snapshot.LimitingWindowLabel != nil || len(snapshot.Windows) != 3 || snapshot.Windows[2].Label != "unbound-provider-limit-1-primary" || snapshot.Windows[2].RemainingPercent == nil || *snapshot.Windows[2].RemainingPercent != 1 {
		t.Fatalf("unbound additional limit did not force a fail-safe route aggregate: snapshot=%+v ok=%v", snapshot, ok)
	}
	if report.Account == nil || report.Account.State != "available" || report.Account.RemainingPercent == nil || *report.Account.RemainingPercent != 60 || len(report.Account.Windows) != 2 {
		t.Fatalf("confirmed global windows were not retained independently: %+v", report.Account)
	}
	if len(report.UnboundLimits) != 1 {
		t.Fatalf("unbound limits=%d want=1", len(report.UnboundLimits))
	}
	unbound := report.UnboundLimits[0]
	if unbound.LimitID != "codex_other" || unbound.LimitName != "model-alpha" || unbound.Snapshot.State != "exhausted" || unbound.Snapshot.RemainingPercent == nil || *unbound.Snapshot.RemainingPercent != 1 {
		t.Fatalf("unbound audit snapshot=%+v", unbound)
	}

	// Returned cache values must not alias the internal audit snapshot.
	*report.UnboundLimits[0].Snapshot.RemainingPercent = 77
	cached, err := service.Read(context.Background(), &coreauth.Auth{ID: "openai-extended", Metadata: map[string]any{"access_token": "secret"}}, accountbridge.ConnectorOpenAI, false)
	if err != nil || cached.UnboundLimits[0].Snapshot.RemainingPercent == nil || *cached.UnboundLimits[0].Snapshot.RemainingPercent != 1 {
		t.Fatalf("cached unbound snapshot aliased caller mutation: report=%+v err=%v", cached.UnboundLimits, err)
	}
}

func TestOpenAIGlobalExhaustionEvidenceAndUnknownWindowsFailSafe(t *testing.T) {
	tests := []struct {
		name          string
		body          string
		wantState     string
		wantRemaining *float64
	}{
		{
			name:      "known reached type exhausts default bucket",
			body:      `{"rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":10}},"rate_limit_reached_type":{"type":"workspace_member_usage_limit_reached"}}`,
			wantState: "exhausted", wantRemaining: floatPointer(90),
		},
		{
			name:      "unknown reached type is not invented as exhaustion",
			body:      `{"rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":10}},"rate_limit_reached_type":{"type":"future_reason"}}`,
			wantState: "available", wantRemaining: floatPointer(90),
		},
		{
			name:      "spend control reached exhausts default bucket",
			body:      `{"rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":10}},"spend_control":{"reached":true,"individual_limit":{"remaining_percent":40}}}`,
			wantState: "exhausted", wantRemaining: floatPointer(40),
		},
		{
			name:      "present spend window without percentage makes aggregate unknown",
			body:      `{"rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":10}},"spend_control":{"reached":false,"individual_limit":{"reset_at":1784145600}}}`,
			wantState: "unknown", wantRemaining: nil,
		},
	}
	for _, testCase := range tests {
		t.Run(testCase.name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
				_, _ = io.WriteString(writer, testCase.body)
			}))
			t.Cleanup(server.Close)
			service := NewService(
				WithEndpoints(Endpoints{OpenAI: server.URL}),
				WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
			)
			report, err := service.Read(context.Background(), &coreauth.Auth{ID: "openai-state-" + testCase.name, Metadata: map[string]any{"access_token": "secret"}}, accountbridge.ConnectorOpenAI, true)
			if err != nil {
				t.Fatalf("Read: %v", err)
			}
			snapshot, ok := report.ForModel("any-model")
			if !ok || snapshot.State != testCase.wantState {
				t.Fatalf("snapshot=%+v ok=%v wantState=%s", snapshot, ok, testCase.wantState)
			}
			if testCase.wantRemaining == nil {
				if snapshot.RemainingPercent != nil {
					t.Fatalf("remaining=%v want nil", *snapshot.RemainingPercent)
				}
			} else if snapshot.RemainingPercent == nil || *snapshot.RemainingPercent != *testCase.wantRemaining {
				t.Fatalf("remaining=%v want=%v", snapshot.RemainingPercent, *testCase.wantRemaining)
			}
		})
	}
}

func TestAnthropicUnknownQuotaWindowsAndPercentAbnormalitiesFailSafe(t *testing.T) {
	tests := []struct {
		name          string
		body          string
		wantError     bool
		wantState     string
		wantRemaining *float64
		wantWindows   []float64
	}{
		{
			name:      "unknown utilization window is rejected",
			body:      `{"five_hour":{"utilization":10},"thirty_day":{"utilization":99,"resets_at":"2026-08-01T00:00:00Z"}}`,
			wantError: true,
		},
		{
			name:      "unknown reset-only window is rejected",
			body:      `{"five_hour":{"utilization":10},"new_window":{"reset_time":"2026-08-01T00:00:00Z"}}`,
			wantError: true,
		},
		{
			name:      "conflicting known percentages are rejected",
			body:      `{"five_hour":{"utilization":10,"used_percentage":11}}`,
			wantError: true,
		},
		{
			name:      "non-finite JSON number is rejected",
			body:      `{"five_hour":{"utilization":1e309}}`,
			wantError: true,
		},
		{
			name:      "non-quota metadata remains forward compatible",
			body:      `{"five_hour":{"utilization":10},"telemetry":{"captured_at":"2026-07-14T00:00:00Z","sample":1}}`,
			wantState: "available", wantRemaining: floatPointer(90),
		},
		{
			name:      "out of range provider percentages are rejected",
			body:      `{"five_hour":{"utilization":-20},"seven_day":{"utilization":120}}`,
			wantError: true,
		},
	}
	for _, testCase := range tests {
		t.Run(testCase.name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
				_, _ = io.WriteString(writer, testCase.body)
			}))
			t.Cleanup(server.Close)
			service := NewService(
				WithEndpoints(Endpoints{Anthropic: server.URL}),
				WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
			)
			report, err := service.Read(context.Background(), &coreauth.Auth{ID: "anthropic-state-" + testCase.name, Metadata: map[string]any{"access_token": "secret"}}, accountbridge.ConnectorAnthropic, true)
			if testCase.wantError {
				if !errors.Is(err, ErrMalformedResponse) {
					t.Fatalf("err=%v want ErrMalformedResponse", err)
				}
				return
			}
			if err != nil {
				t.Fatalf("Read: %v", err)
			}
			snapshot, ok := report.ForModel("model-alpha")
			if !ok || snapshot.State != testCase.wantState || snapshot.RemainingPercent == nil || testCase.wantRemaining == nil || *snapshot.RemainingPercent != *testCase.wantRemaining {
				t.Fatalf("snapshot=%+v ok=%v wantState=%s wantRemaining=%v", snapshot, ok, testCase.wantState, testCase.wantRemaining)
			}
			if len(testCase.wantWindows) > 0 {
				if len(snapshot.Windows) != len(testCase.wantWindows) {
					t.Fatalf("windows=%+v want=%v", snapshot.Windows, testCase.wantWindows)
				}
				for index, want := range testCase.wantWindows {
					if snapshot.Windows[index].RemainingPercent == nil || *snapshot.Windows[index].RemainingPercent != want {
						t.Fatalf("window[%d]=%+v wantRemaining=%v", index, snapshot.Windows[index], want)
					}
				}
			}
		})
	}
}

func floatPointer(value float64) *float64 {
	return &value
}

func TestServiceConcurrentReadsAreSingleFlight(t *testing.T) {
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
		requests.Add(1)
		time.Sleep(25 * time.Millisecond)
		_, _ = io.WriteString(writer, `{"rate_limit":{"primary_window":{"used_percent":10}}}`)
	}))
	t.Cleanup(server.Close)
	service := NewService(
		WithEndpoints(Endpoints{OpenAI: server.URL}),
		WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
	)
	credential := &coreauth.Auth{ID: "single-flight-auth", Metadata: map[string]any{"access_token": "single-flight-secret"}}
	start := make(chan struct{})
	errorsChannel := make(chan error, 16)
	var waitGroup sync.WaitGroup
	for range 16 {
		waitGroup.Add(1)
		go func() {
			defer waitGroup.Done()
			<-start
			_, err := service.Read(context.Background(), credential, accountbridge.ConnectorOpenAI, false)
			errorsChannel <- err
		}()
	}
	close(start)
	waitGroup.Wait()
	close(errorsChannel)
	for err := range errorsChannel {
		if err != nil {
			t.Fatalf("concurrent read: %v", err)
		}
	}
	if requests.Load() != 1 {
		t.Fatalf("provider requests=%d want=1", requests.Load())
	}
}

func TestServiceCacheForceRefreshAndExpiry(t *testing.T) {
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
		requests.Add(1)
		_, _ = io.WriteString(writer, `{"rate_limit":{"primary_window":{"used_percent":20}}}`)
	}))
	t.Cleanup(server.Close)
	now := time.Date(2026, 7, 14, 12, 0, 0, 0, time.UTC)
	var clockMu sync.Mutex
	clock := func() time.Time {
		clockMu.Lock()
		defer clockMu.Unlock()
		return now
	}
	advance := func(duration time.Duration) {
		clockMu.Lock()
		now = now.Add(duration)
		clockMu.Unlock()
	}
	service := NewService(
		WithEndpoints(Endpoints{OpenAI: server.URL}),
		WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
		WithClock(clock),
	)
	credential := &coreauth.Auth{ID: "cache-auth", Metadata: map[string]any{"access_token": "cache-secret"}}

	first, err := service.Read(context.Background(), credential, accountbridge.ConnectorOpenAI, false)
	if err != nil {
		t.Fatalf("first read: %v", err)
	}
	second, err := service.Read(context.Background(), credential, accountbridge.ConnectorOpenAI, false)
	if err != nil || requests.Load() != 1 {
		t.Fatalf("cached read err=%v requests=%d", err, requests.Load())
	}
	firstSnapshot, _ := first.ForModel("any")
	secondSnapshot, _ := second.ForModel("any")
	if !firstSnapshot.ObservedAt.Equal(secondSnapshot.ObservedAt) {
		t.Fatalf("cached observedAt changed: first=%s second=%s", firstSnapshot.ObservedAt, secondSnapshot.ObservedAt)
	}

	advance(time.Second)
	forced, err := service.Read(context.Background(), credential, accountbridge.ConnectorOpenAI, true)
	if err != nil || requests.Load() != 2 {
		t.Fatalf("forced read err=%v requests=%d", err, requests.Load())
	}
	forcedSnapshot, _ := forced.ForModel("any")
	if !forcedSnapshot.ObservedAt.After(firstSnapshot.ObservedAt) {
		t.Fatalf("force refresh retained observedAt=%s", forcedSnapshot.ObservedAt)
	}

	advance(DefaultCacheTTL)
	if _, err := service.Read(context.Background(), credential, accountbridge.ConnectorOpenAI, false); err != nil || requests.Load() != 3 {
		t.Fatalf("expired read err=%v requests=%d", err, requests.Load())
	}
}

func TestProviderFailuresAreFailSafeAndDoNotLeakCredentials(t *testing.T) {
	responses := []struct {
		name   string
		status int
		body   string
	}{
		{name: "http failure", status: http.StatusTooManyRequests, body: `{"error":"secret-marker"}`},
		{name: "malformed", status: http.StatusOK, body: `{"rate_limit":`},
		{name: "oversized", status: http.StatusOK, body: strings.Repeat(" ", maxResponseBytes+1)},
	}
	for _, response := range responses {
		t.Run(response.name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
				writer.WriteHeader(response.status)
				_, _ = io.WriteString(writer, response.body)
			}))
			t.Cleanup(server.Close)
			service := NewService(
				WithEndpoints(Endpoints{OpenAI: server.URL}),
				WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
			)
			_, err := service.Read(context.Background(), &coreauth.Auth{ID: "failure-auth", Metadata: map[string]any{"access_token": "credential-secret-marker"}}, accountbridge.ConnectorOpenAI, true)
			if err == nil {
				t.Fatal("expected provider failure")
			}
			if strings.Contains(err.Error(), "secret-marker") || strings.Contains(err.Error(), "credential-secret-marker") {
				t.Fatalf("provider error leaked sensitive material: %v", err)
			}
		})
	}

	service := NewService()
	_, err := service.Read(context.Background(), &coreauth.Auth{ID: "missing-token"}, accountbridge.ConnectorOpenAI, true)
	if !errors.Is(err, ErrMissingCredential) {
		t.Fatalf("missing token err=%v", err)
	}
}

func TestFailedForceRefreshInvalidatesCachedPercentage(t *testing.T) {
	var fail atomic.Bool
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
		requests.Add(1)
		if fail.Load() {
			writer.WriteHeader(http.StatusServiceUnavailable)
			return
		}
		_, _ = io.WriteString(writer, `{"rate_limit":{"primary_window":{"used_percent":10}}}`)
	}))
	t.Cleanup(server.Close)
	service := NewService(
		WithEndpoints(Endpoints{OpenAI: server.URL}),
		WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
	)
	credential := &coreauth.Auth{ID: "force-failure-auth", Metadata: map[string]any{"access_token": "force-failure-secret"}}
	if _, err := service.Read(context.Background(), credential, accountbridge.ConnectorOpenAI, false); err != nil {
		t.Fatalf("prime cache: %v", err)
	}
	fail.Store(true)
	if _, err := service.Read(context.Background(), credential, accountbridge.ConnectorOpenAI, true); err == nil {
		t.Fatal("forced provider failure unexpectedly returned cached quota")
	}
	if _, err := service.Read(context.Background(), credential, accountbridge.ConnectorOpenAI, false); err == nil {
		t.Fatal("stale quota reappeared after failed forced refresh")
	}
	if requests.Load() != 3 {
		t.Fatalf("provider requests=%d want=3", requests.Load())
	}
}

func TestAmbiguousProviderWindowsFailSafe(t *testing.T) {
	mux := http.NewServeMux()
	mux.HandleFunc("/anthropic", func(writer http.ResponseWriter, _ *http.Request) {
		_, _ = io.WriteString(writer, `{"five_hour":{"utilization":10},"seven_day":{"utilization":"invalid"}}`)
	})
	mux.HandleFunc("/google", func(writer http.ResponseWriter, _ *http.Request) {
		_, _ = io.WriteString(writer, `{"buckets":[{"modelId":"model-alpha","remainingFraction":0.8},{"remainingFraction":0.1}]}`)
	})
	server := httptest.NewServer(mux)
	t.Cleanup(server.Close)
	service := NewService(
		WithEndpoints(Endpoints{Anthropic: server.URL + "/anthropic", Google: server.URL + "/google"}),
		WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
	)
	tests := []struct {
		connectorID string
		credential  *coreauth.Auth
	}{
		{connectorID: accountbridge.ConnectorAnthropic, credential: &coreauth.Auth{ID: "ambiguous-anthropic", Metadata: map[string]any{"access_token": "anthropic-secret"}}},
		{connectorID: accountbridge.ConnectorGoogle, credential: &coreauth.Auth{ID: "ambiguous-google", Metadata: map[string]any{"access_token": "google-secret", "project_id": "projects/test"}}},
	}
	for _, testCase := range tests {
		if _, err := service.Read(context.Background(), testCase.credential, testCase.connectorID, true); !errors.Is(err, ErrMalformedResponse) {
			t.Fatalf("connector=%s err=%v want ErrMalformedResponse", testCase.connectorID, err)
		}
	}
}

func TestProviderWireValuesAndShapesFailClosed(t *testing.T) {
	tests := []struct {
		name        string
		connectorID string
		body        string
	}{
		{name: "OpenAI used percent above range", connectorID: accountbridge.ConnectorOpenAI, body: `{"rate_limit":{"primary_window":{"used_percent":101}}}`},
		{name: "OpenAI remaining percent below range", connectorID: accountbridge.ConnectorOpenAI, body: `{"spend_control":{"individual_limit":{"remaining_percent":-1}}}`},
		{name: "Google remaining fraction above range", connectorID: accountbridge.ConnectorGoogle, body: `{"buckets":[{"modelId":"model-alpha","remainingFraction":1.01}]}`},
		{name: "xAI used percent below range", connectorID: accountbridge.ConnectorXAI, body: `{"currentPeriod":{"creditUsagePercent":-1}}`},
		{name: "OpenAI implausible Unix reset", connectorID: accountbridge.ConnectorOpenAI, body: `{"rate_limit":{"primary_window":{"used_percent":10,"reset_at":1}}}`},
		{name: "Anthropic invalid reset", connectorID: accountbridge.ConnectorAnthropic, body: `{"five_hour":{"utilization":10,"resets_at":"tomorrow"}}`},
		{name: "Google implausible reset", connectorID: accountbridge.ConnectorGoogle, body: `{"buckets":[{"modelId":"model-alpha","remainingFraction":0.5,"resetTime":"9999-01-01T00:00:00Z"}]}`},
		{name: "xAI implausible reset", connectorID: accountbridge.ConnectorXAI, body: `{"currentPeriod":{"creditUsagePercent":10,"end":"9999-01-01T00:00:00Z"}}`},
		{name: "OpenAI unknown top-level quota", connectorID: accountbridge.ConnectorOpenAI, body: `{"rate_limit":{"primary_window":{"used_percent":10}},"future_quota":{"remaining_percent":99}}`},
		{name: "OpenAI unknown nested quota", connectorID: accountbridge.ConnectorOpenAI, body: `{"rate_limit":{"primary_window":{"used_percent":10,"tertiary_window":{"used_percent":1}}}}`},
		{name: "Google unknown quota", connectorID: accountbridge.ConnectorGoogle, body: `{"buckets":[],"globalQuota":{"remainingFraction":1}}`},
		{name: "Google unknown bucket limit", connectorID: accountbridge.ConnectorGoogle, body: `{"buckets":[{"modelId":"model-alpha","remainingFraction":0.5,"dailyLimit":100}]}`},
		{name: "xAI unknown period", connectorID: accountbridge.ConnectorXAI, body: `{"currentPeriod":{"creditUsagePercent":10},"nextPeriod":{"creditUsagePercent":0}}`},
		{name: "xAI unknown remaining percent", connectorID: accountbridge.ConnectorXAI, body: `{"currentPeriod":{"creditUsagePercent":10,"remainingPercent":90}}`},
		{name: "OpenAI duplicate key", connectorID: accountbridge.ConnectorOpenAI, body: `{"rate_limit":{"primary_window":{"used_percent":10,"used_percent":20}}}`},
		{name: "Anthropic duplicate key", connectorID: accountbridge.ConnectorAnthropic, body: `{"five_hour":{"utilization":10,"utilization":20}}`},
		{name: "Google duplicate key", connectorID: accountbridge.ConnectorGoogle, body: `{"buckets":[{"modelId":"model-alpha","modelId":"model-beta","remainingFraction":0.5}]}`},
		{name: "xAI duplicate key", connectorID: accountbridge.ConnectorXAI, body: `{"currentPeriod":{"creditUsagePercent":10,"creditUsagePercent":20}}`},
		{name: "OpenAI mixed envelopes", connectorID: accountbridge.ConnectorOpenAI, body: `{"rate_limit":{"primary_window":{"used_percent":10}},"rate_limits":{"rate_limit":{"primary_window":{"used_percent":20}}}}`},
		{name: "Google duplicate semantic bucket", connectorID: accountbridge.ConnectorGoogle, body: `{"buckets":[{"modelId":"model-alpha","tokenType":"Requests","remainingFraction":0.5},{"modelId":"model-alpha","tokenType":"requests","remainingFraction":0.4}]}`},
	}
	for _, testCase := range tests {
		t.Run(testCase.name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
				_, _ = io.WriteString(writer, testCase.body)
			}))
			t.Cleanup(server.Close)
			service := NewService(
				WithEndpoints(Endpoints{OpenAI: server.URL, Anthropic: server.URL, Google: server.URL, XAI: server.URL}),
				WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
			)
			credential := &coreauth.Auth{ID: "strict-" + testCase.name, Metadata: map[string]any{
				"access_token": "secret",
				"project_id":   "projects/test",
			}}
			if _, err := service.Read(context.Background(), credential, testCase.connectorID, true); !errors.Is(err, ErrMalformedResponse) {
				t.Fatalf("err=%v want ErrMalformedResponse", err)
			}
		})
	}
}

func TestProviderNonQuotaMetadataAndEmptyGoogleBucketsRemainCompatible(t *testing.T) {
	tests := []struct {
		name        string
		connectorID string
		body        string
	}{
		{name: "OpenAI metadata", connectorID: accountbridge.ConnectorOpenAI, body: `{"rate_limit":{"primary_window":{"used_percent":10}},"telemetry":{"captured_at":"2026-07-14T00:00:00Z"}}`},
		{name: "Google metadata and empty buckets", connectorID: accountbridge.ConnectorGoogle, body: `{"buckets":[],"telemetry":{"captured_at":"2026-07-14T00:00:00Z"}}`},
		{name: "xAI metadata", connectorID: accountbridge.ConnectorXAI, body: `{"currentPeriod":{"creditUsagePercent":10},"telemetry":{"captured_at":"2026-07-14T00:00:00Z"}}`},
	}
	for _, testCase := range tests {
		t.Run(testCase.name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
				_, _ = io.WriteString(writer, testCase.body)
			}))
			t.Cleanup(server.Close)
			service := NewService(
				WithEndpoints(Endpoints{OpenAI: server.URL, Google: server.URL, XAI: server.URL}),
				WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return server.Client() }),
			)
			report, err := service.Read(context.Background(), &coreauth.Auth{ID: "metadata-" + testCase.name, Metadata: map[string]any{"access_token": "secret", "project_id": "projects/test"}}, testCase.connectorID, true)
			if err != nil {
				t.Fatalf("Read: %v", err)
			}
			if testCase.connectorID == accountbridge.ConnectorGoogle && len(report.Models) != 0 {
				t.Fatalf("empty Google buckets produced models: %+v", report.Models)
			}
		})
	}
}

func TestProviderRedirectIsRejectedWithoutForwardingBearer(t *testing.T) {
	var redirectedAuthorization atomic.Value
	target := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		redirectedAuthorization.Store(request.Header.Get("Authorization"))
		_, _ = io.WriteString(writer, `{}`)
	}))
	t.Cleanup(target.Close)
	source := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
		writer.Header().Set("Location", target.URL)
		writer.WriteHeader(http.StatusFound)
	}))
	t.Cleanup(source.Close)
	service := NewService(
		WithEndpoints(Endpoints{OpenAI: source.URL}),
		WithClientFactory(func(context.Context, *coreauth.Auth, time.Duration) *http.Client { return source.Client() }),
	)
	_, err := service.Read(context.Background(), &coreauth.Auth{ID: "redirect-auth", Metadata: map[string]any{"access_token": "redirect-secret"}}, accountbridge.ConnectorOpenAI, true)
	if err == nil {
		t.Fatal("redirect unexpectedly succeeded")
	}
	if value := redirectedAuthorization.Load(); value != nil {
		t.Fatalf("redirect target received authorization=%q", value)
	}
}

func sanitizedHeaders(headers http.Header) http.Header {
	clone := headers.Clone()
	if clone.Get("Authorization") != "" {
		clone.Set("Authorization", "<redacted>")
	}
	return clone
}
