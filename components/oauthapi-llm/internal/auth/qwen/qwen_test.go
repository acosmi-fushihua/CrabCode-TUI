package qwen

import (
	"context"
	"crypto/sha256"
	"encoding/base64"
	"io"
	"net/http"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"golang.org/x/sync/singleflight"
)

type qwenRoundTripFunc func(*http.Request) (*http.Response, error)

func (f qwenRoundTripFunc) RoundTrip(req *http.Request) (*http.Response, error) {
	return f(req)
}

func jsonResponse(req *http.Request, status int, body string) *http.Response {
	return &http.Response{
		StatusCode: status,
		Body:       io.NopCloser(strings.NewReader(body)),
		Header:     http.Header{"Content-Type": []string{"application/json"}},
		Request:    req,
	}
}

func newTestDeviceFlowClient(transport http.RoundTripper) *DeviceFlowClient {
	return &DeviceFlowClient{httpClient: &http.Client{Transport: transport}}
}

func resetQwenRefreshGroupForTest() {
	qwenRefreshGroup = singleflight.Group{}
}

func TestRequestDeviceCodeSendsPKCEAndBindsVerifier(t *testing.T) {
	var capturedForm map[string][]string
	var capturedRequestID string
	transport := qwenRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		if req.URL.String() != qwenDeviceCodeURL {
			t.Fatalf("device code URL=%q", req.URL.String())
		}
		if got := req.Header.Get("Content-Type"); got != "application/x-www-form-urlencoded" {
			t.Fatalf("content type=%q", got)
		}
		capturedRequestID = req.Header.Get("x-request-id")
		if err := req.ParseForm(); err != nil {
			t.Fatalf("parse form: %v", err)
		}
		capturedForm = req.PostForm
		return jsonResponse(req, http.StatusOK, `{
			"device_code":"device-code-1",
			"user_code":"ABCD-1234",
			"verification_uri":"https://chat.qwen.ai/authorize",
			"verification_uri_complete":"https://chat.qwen.ai/authorize?user_code=ABCD-1234",
			"expires_in":900
		}`), nil
	})
	client := newTestDeviceFlowClient(transport)

	deviceCode, err := client.RequestDeviceCode(context.Background())
	if err != nil {
		t.Fatalf("RequestDeviceCode: %v", err)
	}
	if deviceCode.DeviceCode != "device-code-1" || deviceCode.UserCode != "ABCD-1234" || deviceCode.ExpiresIn != 900 {
		t.Fatalf("unexpected device code response: %+v", deviceCode)
	}
	if capturedRequestID == "" {
		t.Fatal("device code request must carry x-request-id")
	}
	if got := capturedForm["client_id"]; len(got) != 1 || got[0] != qwenClientID {
		t.Fatalf("client_id=%v", got)
	}
	if got := capturedForm["scope"]; len(got) != 1 || got[0] != qwenScope {
		t.Fatalf("scope=%v", got)
	}
	if got := capturedForm["code_challenge_method"]; len(got) != 1 || got[0] != "S256" {
		t.Fatalf("code_challenge_method=%v", got)
	}
	if deviceCode.CodeVerifier == "" {
		t.Fatal("PKCE verifier must be bound to the device authorization")
	}
	hash := sha256.Sum256([]byte(deviceCode.CodeVerifier))
	wantChallenge := base64.RawURLEncoding.EncodeToString(hash[:])
	if got := capturedForm["code_challenge"]; len(got) != 1 || got[0] != wantChallenge {
		t.Fatalf("code_challenge=%v, want S256 of the bound verifier", got)
	}
}

func TestRequestDeviceCodeSurfacesOAuthErrorEnvelope(t *testing.T) {
	transport := qwenRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		return jsonResponse(req, http.StatusOK, `{"error":"invalid_client","error_description":"unknown client"}`), nil
	})
	if _, err := newTestDeviceFlowClient(transport).RequestDeviceCode(context.Background()); err == nil || !strings.Contains(err.Error(), "invalid_client") {
		t.Fatalf("expected oauth error surfaced, got %v", err)
	}
}

func TestExchangeDeviceCodePendingSlowDownAndTerminalStates(t *testing.T) {
	tests := []struct {
		name           string
		status         int
		body           string
		wantContinue   bool
		wantSlowDown   bool
		wantErrSubstr  string
		wantTokenValue string
	}{
		{name: "authorization pending", status: http.StatusBadRequest, body: `{"error":"authorization_pending","error_description":"waiting"}`, wantContinue: true},
		{name: "slow down", status: http.StatusTooManyRequests, body: `{"error":"slow_down","error_description":"poll slower"}`, wantContinue: true, wantSlowDown: true},
		{name: "expired token", status: http.StatusBadRequest, body: `{"error":"expired_token","error_description":"gone"}`, wantErrSubstr: "device code expired"},
		{name: "access denied", status: http.StatusBadRequest, body: `{"error":"access_denied","error_description":"user said no"}`, wantErrSubstr: "access denied"},
		{name: "other oauth error", status: http.StatusBadRequest, body: `{"error":"invalid_grant","error_description":"bad grant"}`, wantErrSubstr: "invalid_grant"},
		{name: "non oauth failure", status: http.StatusBadGateway, body: `upstream exploded`, wantErrSubstr: "status 502"},
		{name: "success", status: http.StatusOK, body: `{"access_token":"token-1","refresh_token":"refresh-1","token_type":"Bearer","expires_in":3600,"resource_url":"portal.qwen.ai"}`, wantTokenValue: "token-1"},
		{name: "empty access token", status: http.StatusOK, body: `{"access_token":"","token_type":"Bearer"}`, wantErrSubstr: "empty access token"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			var capturedForm map[string][]string
			transport := qwenRoundTripFunc(func(req *http.Request) (*http.Response, error) {
				if req.URL.String() != qwenTokenURL {
					t.Fatalf("token URL=%q", req.URL.String())
				}
				if err := req.ParseForm(); err != nil {
					t.Fatalf("parse form: %v", err)
				}
				capturedForm = req.PostForm
				return jsonResponse(req, test.status, test.body), nil
			})
			client := newTestDeviceFlowClient(transport)
			token, slowDown, err, shouldContinue := client.exchangeDeviceCode(context.Background(), "device-code-1", "verifier-1")
			if got := capturedForm["grant_type"]; len(got) != 1 || got[0] != qwenDeviceGrantType {
				t.Fatalf("grant_type=%v", got)
			}
			if got := capturedForm["device_code"]; len(got) != 1 || got[0] != "device-code-1" {
				t.Fatalf("device_code=%v", got)
			}
			if got := capturedForm["code_verifier"]; len(got) != 1 || got[0] != "verifier-1" {
				t.Fatalf("code_verifier=%v", got)
			}
			if shouldContinue != test.wantContinue || slowDown != test.wantSlowDown {
				t.Fatalf("continue=%t slowDown=%t, want %t/%t (err=%v)", shouldContinue, slowDown, test.wantContinue, test.wantSlowDown, err)
			}
			if test.wantErrSubstr != "" {
				if err == nil || !strings.Contains(err.Error(), test.wantErrSubstr) {
					t.Fatalf("err=%v, want substring %q", err, test.wantErrSubstr)
				}
				return
			}
			if test.wantTokenValue != "" {
				if token == nil || token.AccessToken != test.wantTokenValue {
					t.Fatalf("token=%+v", token)
				}
				if token.ResourceURL != "portal.qwen.ai" || token.RefreshToken != "refresh-1" {
					t.Fatalf("token fields not preserved: %+v", token)
				}
				if token.ExpiresAt <= time.Now().Unix() {
					t.Fatalf("expiry not derived from expires_in: %+v", token)
				}
			}
		})
	}
}

func TestPollForTokenReturnsTokenAfterPending(t *testing.T) {
	var calls int32
	transport := qwenRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		switch atomic.AddInt32(&calls, 1) {
		case 1:
			return jsonResponse(req, http.StatusBadRequest, `{"error":"authorization_pending","error_description":"waiting"}`), nil
		default:
			return jsonResponse(req, http.StatusOK, `{"access_token":"token-2","refresh_token":"refresh-2","token_type":"Bearer","expires_in":3600,"resource_url":"portal.qwen.ai"}`), nil
		}
	})
	client := newTestDeviceFlowClient(transport)
	deviceCode := &DeviceCodeResponse{DeviceCode: "device-code-2", ExpiresIn: 600, CodeVerifier: "verifier-2"}

	token, err := client.PollForToken(context.Background(), deviceCode)
	if err != nil {
		t.Fatalf("PollForToken: %v", err)
	}
	if token.AccessToken != "token-2" || token.ResourceURL != "portal.qwen.ai" {
		t.Fatalf("unexpected token: %+v", token)
	}
	if got := atomic.LoadInt32(&calls); got < 2 {
		t.Fatalf("expected pending then success, got %d calls", got)
	}
}

func TestPollForTokenRequiresBoundVerifier(t *testing.T) {
	client := newTestDeviceFlowClient(qwenRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		t.Fatal("no request expected without a PKCE verifier")
		return nil, nil
	}))
	if _, err := client.PollForToken(context.Background(), &DeviceCodeResponse{DeviceCode: "x"}); err == nil || !strings.Contains(err.Error(), "PKCE verifier") {
		t.Fatalf("expected missing verifier rejection, got %v", err)
	}
}

func TestRefreshTokenRotatesCredentialsAndResourceURL(t *testing.T) {
	resetQwenRefreshGroupForTest()
	t.Cleanup(resetQwenRefreshGroupForTest)

	var capturedForm map[string][]string
	transport := qwenRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		if err := req.ParseForm(); err != nil {
			t.Fatalf("parse form: %v", err)
		}
		capturedForm = req.PostForm
		return jsonResponse(req, http.StatusOK, `{
			"access_token":"new-access",
			"refresh_token":"new-refresh",
			"token_type":"Bearer",
			"expires_in":3600,
			"resource_url":"portal-eu.qwen.ai"
		}`), nil
	})
	client := newTestDeviceFlowClient(transport)

	token, err := client.RefreshToken(context.Background(), "old-refresh")
	if err != nil {
		t.Fatalf("RefreshToken: %v", err)
	}
	if got := capturedForm["grant_type"]; len(got) != 1 || got[0] != "refresh_token" {
		t.Fatalf("grant_type=%v", got)
	}
	if got := capturedForm["refresh_token"]; len(got) != 1 || got[0] != "old-refresh" {
		t.Fatalf("refresh_token=%v", got)
	}
	if got := capturedForm["client_id"]; len(got) != 1 || got[0] != qwenClientID {
		t.Fatalf("client_id=%v", got)
	}
	if token.AccessToken != "new-access" || token.RefreshToken != "new-refresh" || token.ResourceURL != "portal-eu.qwen.ai" {
		t.Fatalf("unexpected refresh result: %+v", token)
	}
}

func TestRefreshTokenRejectedStatusesRequireReauthentication(t *testing.T) {
	resetQwenRefreshGroupForTest()
	t.Cleanup(resetQwenRefreshGroupForTest)

	for _, status := range []int{http.StatusBadRequest, http.StatusUnauthorized} {
		transport := qwenRoundTripFunc(func(req *http.Request) (*http.Response, error) {
			return jsonResponse(req, status, `{"error":"invalid_grant"}`), nil
		})
		_, err := newTestDeviceFlowClient(transport).RefreshToken(context.Background(), "expired-refresh-"+time.Now().String())
		if err == nil || !strings.Contains(err.Error(), "re-authentication required") {
			t.Fatalf("status %d: err=%v", status, err)
		}
		resetQwenRefreshGroupForTest()
	}
}

func TestRefreshTokenDeduplicatesConcurrentRefresh(t *testing.T) {
	resetQwenRefreshGroupForTest()
	t.Cleanup(resetQwenRefreshGroupForTest)

	var calls int32
	started := make(chan struct{})
	release := make(chan struct{})
	var once sync.Once

	transport := qwenRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		atomic.AddInt32(&calls, 1)
		once.Do(func() { close(started) })
		<-release
		return jsonResponse(req, http.StatusOK, `{
			"access_token":"new-access",
			"refresh_token":"new-refresh",
			"token_type":"Bearer",
			"expires_in":3600
		}`), nil
	})
	clientA := newTestDeviceFlowClient(transport)
	clientB := newTestDeviceFlowClient(transport)

	results := make(chan *QwenTokenData, 2)
	errs := make(chan error, 2)
	runRefresh := func(client *DeviceFlowClient, launched chan<- struct{}) {
		if launched != nil {
			close(launched)
		}
		tokenData, errRefresh := client.RefreshToken(context.Background(), "shared-refresh-token")
		results <- tokenData
		errs <- errRefresh
	}

	go runRefresh(clientA, nil)
	<-started

	secondLaunched := make(chan struct{})
	go runRefresh(clientB, secondLaunched)
	<-secondLaunched
	time.Sleep(20 * time.Millisecond)
	if got := atomic.LoadInt32(&calls); got != 1 {
		t.Fatalf("expected concurrent refresh to share a single upstream call, got %d", got)
	}
	close(release)

	for i := 0; i < 2; i++ {
		if errRefresh := <-errs; errRefresh != nil {
			t.Fatalf("expected refresh to succeed, got %v", errRefresh)
		}
		tokenData := <-results
		if tokenData == nil || tokenData.AccessToken != "new-access" {
			t.Fatalf("unexpected token data: %#v", tokenData)
		}
	}
	if got := atomic.LoadInt32(&calls); got != 1 {
		t.Fatalf("expected both refresh callers to share a single upstream call, got %d", got)
	}
}

func TestAPIBaseURLFromResourceURLNormalizationMatrix(t *testing.T) {
	tests := []struct {
		name string
		in   string
		want string
	}{
		{name: "empty falls back to default", in: "", want: DefaultAPIBaseURL},
		{name: "whitespace falls back to default", in: "   ", want: DefaultAPIBaseURL},
		{name: "bare host", in: "portal.qwen.ai", want: "https://portal.qwen.ai/v1"},
		{name: "bare host trailing slash", in: "portal.qwen.ai/", want: "https://portal.qwen.ai/v1"},
		{name: "bare host multiple trailing slashes", in: "portal.qwen.ai//", want: "https://portal.qwen.ai/v1"},
		{name: "https scheme", in: "https://portal.qwen.ai", want: "https://portal.qwen.ai/v1"},
		{name: "http scheme preserved", in: "http://portal.qwen.ai", want: "http://portal.qwen.ai/v1"},
		{name: "uppercase scheme detected", in: "HTTPS://portal.qwen.ai", want: "HTTPS://portal.qwen.ai/v1"},
		{name: "existing v1 suffix", in: "https://portal.qwen.ai/v1", want: "https://portal.qwen.ai/v1"},
		{name: "existing v1 suffix trailing slash", in: "https://portal.qwen.ai/v1/", want: "https://portal.qwen.ai/v1"},
		{name: "default base round trips", in: DefaultAPIBaseURL, want: DefaultAPIBaseURL},
		{name: "only slashes falls back to default", in: "///", want: DefaultAPIBaseURL},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := APIBaseURLFromResourceURL(test.in); got != test.want {
				t.Fatalf("APIBaseURLFromResourceURL(%q)=%q, want %q", test.in, got, test.want)
			}
		})
	}
}
