package zai

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"io"
	"net/http"
	"strings"
	"sync/atomic"
	"testing"
)

type zaiRoundTripFunc func(*http.Request) (*http.Response, error)

func (f zaiRoundTripFunc) RoundTrip(req *http.Request) (*http.Response, error) {
	return f(req)
}

func zaiJSONResponse(req *http.Request, status int, body string) *http.Response {
	return &http.Response{
		StatusCode: status,
		Body:       io.NopCloser(strings.NewReader(body)),
		Header:     http.Header{"Content-Type": []string{"application/json"}},
		Request:    req,
	}
}

func newTestZaiAuth(transport http.RoundTripper) *ZaiAuth {
	return &ZaiAuth{httpClient: &http.Client{Transport: transport}}
}

func TestStartFlowSendsProviderAndParsesInitEnvelope(t *testing.T) {
	var capturedBody []byte
	var capturedBearer string
	transport := zaiRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		if req.Method != http.MethodPost || req.URL.String() != zaiOAuthBaseURL+"/oauth/cli/init" {
			t.Fatalf("init request=%s %s", req.Method, req.URL.String())
		}
		capturedBearer = strings.TrimPrefix(req.Header.Get("Authorization"), "Bearer ")
		var err error
		capturedBody, err = io.ReadAll(req.Body)
		if err != nil {
			t.Fatalf("read init body: %v", err)
		}
		return zaiJSONResponse(req, http.StatusOK, `{
			"code":0,
			"msg":"",
			"data":{
				"flow_id":"flow-1",
				"poll_token":"server-poll-token",
				"authorize_url":"https://zcode.z.ai/authorize/flow-1",
				"expires_at":1900000000,
				"poll_interval_sec":3
			}
		}`), nil
	})

	init, err := newTestZaiAuth(transport).StartFlow(context.Background())
	if err != nil {
		t.Fatalf("StartFlow: %v", err)
	}
	var body map[string]string
	if err = json.Unmarshal(capturedBody, &body); err != nil {
		t.Fatalf("decode init body: %v", err)
	}
	if body["provider"] != "zai" {
		t.Fatalf("init provider=%q", body["provider"])
	}
	if raw, errDecode := hex.DecodeString(capturedBearer); errDecode != nil || len(raw) != pollTokenBytes {
		t.Fatalf("client poll token must be %d random bytes hex encoded, got %q", pollTokenBytes, capturedBearer)
	}
	if init.FlowID != "flow-1" || init.AuthorizeURL != "https://zcode.z.ai/authorize/flow-1" || init.PollIntervalSec != 3 {
		t.Fatalf("unexpected init response: %+v", init)
	}
	if init.PollToken != "server-poll-token" {
		t.Fatalf("server poll token must win, got %q", init.PollToken)
	}
}

func TestStartFlowFallsBackToClientPollToken(t *testing.T) {
	transport := zaiRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		return zaiJSONResponse(req, http.StatusOK, `{
			"code":0,
			"data":{"flow_id":"flow-2","authorize_url":"https://zcode.z.ai/authorize/flow-2"}
		}`), nil
	})
	init, err := newTestZaiAuth(transport).StartFlow(context.Background())
	if err != nil {
		t.Fatalf("StartFlow: %v", err)
	}
	if raw, errDecode := hex.DecodeString(init.PollToken); errDecode != nil || len(raw) != pollTokenBytes {
		t.Fatalf("expected client-generated poll token fallback, got %q", init.PollToken)
	}
}

func TestStartFlowFailsClosedOnEnvelopeAndShape(t *testing.T) {
	tests := []struct {
		name      string
		status    int
		body      string
		errSubstr string
	}{
		{name: "business error", status: http.StatusOK, body: `{"code":1001,"msg":"rate limited"}`, errSubstr: "rate limited"},
		{name: "http error", status: http.StatusBadGateway, body: `upstream exploded`, errSubstr: "HTTP 502"},
		{name: "missing flow id", status: http.StatusOK, body: `{"code":0,"data":{"authorize_url":"https://zcode.z.ai/a"}}`, errSubstr: "missing flow_id"},
		{name: "missing authorize url", status: http.StatusOK, body: `{"code":0,"data":{"flow_id":"flow-3"}}`, errSubstr: "missing flow_id or authorize_url"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			transport := zaiRoundTripFunc(func(req *http.Request) (*http.Response, error) {
				return zaiJSONResponse(req, test.status, test.body), nil
			})
			if _, err := newTestZaiAuth(transport).StartFlow(context.Background()); err == nil || !strings.Contains(err.Error(), test.errSubstr) {
				t.Fatalf("err=%v, want substring %q", err, test.errSubstr)
			}
		})
	}
}

func TestPollStates(t *testing.T) {
	init := &InitResponse{FlowID: "flow-4", PollToken: "poll-token-4"}
	tests := []struct {
		name         string
		status       int
		body         string
		wantDone     bool
		wantTerminal bool
		wantErr      string
	}{
		{name: "pending", status: http.StatusOK, body: `{"code":0,"data":{"status":"pending"}}`},
		{name: "empty status is pending", status: http.StatusOK, body: `{"code":0,"data":{}}`},
		{name: "failed", status: http.StatusOK, body: `{"code":0,"data":{"status":"failed"}}`, wantTerminal: true, wantErr: "failed or was denied"},
		{name: "unexpected status", status: http.StatusOK, body: `{"code":0,"data":{"status":"weird"}}`, wantTerminal: true, wantErr: "unexpected poll status"},
		{name: "ready missing token", status: http.StatusOK, body: `{"code":0,"data":{"status":"ready"}}`, wantTerminal: true, wantErr: "missing token"},
		{name: "transient http error", status: http.StatusInternalServerError, body: `boom`, wantErr: "HTTP 500"},
		{name: "ready", status: http.StatusOK, body: `{
			"code":0,
			"data":{
				"status":"ready",
				"token":"coding-plan-token",
				"user":{"user_id":"user-1","email":"dev@example.com","name":"Dev"},
				"zai":{"access_token":"zai-oauth-token"}
			}
		}`, wantDone: true},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			transport := zaiRoundTripFunc(func(req *http.Request) (*http.Response, error) {
				if req.Method != http.MethodGet || req.URL.String() != zaiOAuthBaseURL+"/oauth/cli/poll/flow-4" {
					t.Fatalf("poll request=%s %s", req.Method, req.URL.String())
				}
				if got := req.Header.Get("Authorization"); got != "Bearer poll-token-4" {
					t.Fatalf("poll authorization=%q", got)
				}
				return zaiJSONResponse(req, test.status, test.body), nil
			})
			result, done, terminal, err := newTestZaiAuth(transport).poll(context.Background(), init)
			if done != test.wantDone || terminal != test.wantTerminal {
				t.Fatalf("done=%t terminal=%t, want %t/%t (err=%v)", done, terminal, test.wantDone, test.wantTerminal, err)
			}
			if test.wantErr != "" {
				if err == nil || !strings.Contains(err.Error(), test.wantErr) {
					t.Fatalf("err=%v, want substring %q", err, test.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("poll: %v", err)
			}
			if test.wantDone {
				if result == nil || result.Token != "coding-plan-token" || result.ZaiAccessToken != "zai-oauth-token" || result.Email != "dev@example.com" {
					t.Fatalf("unexpected ready result: %+v", result)
				}
			}
		})
	}
}

func TestWaitForAuthorizationReadyOnFirstPoll(t *testing.T) {
	var calls int32
	transport := zaiRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		atomic.AddInt32(&calls, 1)
		return zaiJSONResponse(req, http.StatusOK, `{
			"code":0,
			"data":{"status":"ready","token":"coding-plan-token","zai":{"access_token":"zai-token"}}
		}`), nil
	})
	init := &InitResponse{FlowID: "flow-5", PollToken: "poll-token-5"}
	result, err := newTestZaiAuth(transport).WaitForAuthorization(context.Background(), init)
	if err != nil {
		t.Fatalf("WaitForAuthorization: %v", err)
	}
	if result.Token != "coding-plan-token" || atomic.LoadInt32(&calls) != 1 {
		t.Fatalf("unexpected result=%+v calls=%d", result, calls)
	}
}

func TestMintAPIKeyReusesExistingKeyAndBuildsComposite(t *testing.T) {
	var bizLoginBody []byte
	var customerAuthorization string
	transport := zaiRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		switch {
		case req.Method == http.MethodPost && req.URL.String() == zaiBizBaseURL+"/api/auth/z/login":
			if got := req.Header.Get("Authorization"); got != "" {
				t.Fatalf("business login must not carry an Authorization header, got %q", got)
			}
			var err error
			bizLoginBody, err = io.ReadAll(req.Body)
			if err != nil {
				t.Fatalf("read login body: %v", err)
			}
			return zaiJSONResponse(req, http.StatusOK, `{"code":200,"data":{"access_token":"biz-token"}}`), nil
		case req.Method == http.MethodGet && req.URL.String() == zaiBizBaseURL+"/api/biz/customer/getCustomerInfo":
			customerAuthorization = req.Header.Get("Authorization")
			return zaiJSONResponse(req, http.StatusOK, `{
				"code":200,
				"data":{"organizations":[
					{"organizationId":"org-empty","organizationName":"Empty Org","projects":[]},
					{"organizationId":"org-1","organizationName":"默认机构","projects":[
						{"projectId":"proj-other","projectName":"Other"},
						{"projectId":"proj-1","projectName":"默认项目"}
					]}
				]}
			}`), nil
		case req.Method == http.MethodGet && req.URL.String() == zaiBizBaseURL+"/api/biz/v1/organization/org-1/projects/proj-1/api_keys":
			return zaiJSONResponse(req, http.StatusOK, `{"code":200,"data":[{"name":"other-key","apiKey":"nope"},{"name":"zcode-api-key","apiKey":"ak-123"}]}`), nil
		case req.Method == http.MethodGet && req.URL.String() == zaiBizBaseURL+"/api/biz/v1/organization/org-1/projects/proj-1/api_keys/copy/ak-123":
			return zaiJSONResponse(req, http.StatusOK, `{"code":200,"data":{"secretKey":"sk-456"}}`), nil
		default:
			t.Fatalf("unexpected request: %s %s", req.Method, req.URL.String())
			return nil, nil
		}
	})

	apiKey, baseURL, err := newTestZaiAuth(transport).MintAPIKey(context.Background(), &ReadyResult{ZaiAccessToken: "zai-oauth-token"})
	if err != nil {
		t.Fatalf("MintAPIKey: %v", err)
	}
	var login map[string]string
	if err = json.Unmarshal(bizLoginBody, &login); err != nil {
		t.Fatalf("decode login body: %v", err)
	}
	if login["token"] != "zai-oauth-token" {
		t.Fatalf("business login token=%q", login["token"])
	}
	if customerAuthorization != "Bearer biz-token" {
		t.Fatalf("customer info authorization=%q", customerAuthorization)
	}
	if apiKey != "ak-123.sk-456" {
		t.Fatalf("composite key=%q, want apiKey.secretKey", apiKey)
	}
	if baseURL != ZAIAPIBaseURL {
		t.Fatalf("base URL=%q", baseURL)
	}
}

func TestMintAPIKeyCreatesKeyWhenAbsent(t *testing.T) {
	var createBody []byte
	transport := zaiRoundTripFunc(func(req *http.Request) (*http.Response, error) {
		keysURL := zaiBizBaseURL + "/api/biz/v1/organization/org-1/projects/proj-1/api_keys"
		switch {
		case req.Method == http.MethodPost && req.URL.String() == zaiBizBaseURL+"/api/auth/z/login":
			return zaiJSONResponse(req, http.StatusOK, `{"code":0,"data":{"access_token":"biz-token"}}`), nil
		case req.Method == http.MethodGet && req.URL.String() == zaiBizBaseURL+"/api/biz/customer/getCustomerInfo":
			return zaiJSONResponse(req, http.StatusOK, `{
				"code":0,
				"data":{"organizations":[{"organizationId":"org-1","organizationName":"Org","projects":[{"projectId":"proj-1","projectName":"Proj"}]}]}
			}`), nil
		case req.Method == http.MethodGet && req.URL.String() == keysURL:
			return zaiJSONResponse(req, http.StatusOK, `{"code":0,"data":[]}`), nil
		case req.Method == http.MethodPost && req.URL.String() == keysURL:
			var err error
			createBody, err = io.ReadAll(req.Body)
			if err != nil {
				t.Fatalf("read create body: %v", err)
			}
			return zaiJSONResponse(req, http.StatusOK, `{"code":0,"data":{"apiKey":"ak-new"}}`), nil
		case req.Method == http.MethodGet && req.URL.String() == keysURL+"/copy/ak-new":
			return zaiJSONResponse(req, http.StatusOK, `{"code":0,"data":{"secretKey":"sk-new"}}`), nil
		default:
			t.Fatalf("unexpected request: %s %s", req.Method, req.URL.String())
			return nil, nil
		}
	})

	apiKey, _, err := newTestZaiAuth(transport).MintAPIKey(context.Background(), &ReadyResult{ZaiAccessToken: "zai-oauth-token"})
	if err != nil {
		t.Fatalf("MintAPIKey: %v", err)
	}
	var create map[string]string
	if err = json.Unmarshal(createBody, &create); err != nil {
		t.Fatalf("decode create body: %v", err)
	}
	if create["name"] != mintKeyName {
		t.Fatalf("created key name=%q, want %q", create["name"], mintKeyName)
	}
	if apiKey != "ak-new.sk-new" {
		t.Fatalf("composite key=%q", apiKey)
	}
}

func TestMintAPIKeyFailsClosed(t *testing.T) {
	tests := []struct {
		name      string
		ready     *ReadyResult
		responses map[string]string
		errSubstr string
	}{
		{name: "nil ready", ready: nil, errSubstr: "ready result is nil"},
		{name: "missing oauth token", ready: &ReadyResult{}, errSubstr: "missing access token"},
		{
			name:  "no organization",
			ready: &ReadyResult{ZaiAccessToken: "zai-token"},
			responses: map[string]string{
				"/api/auth/z/login":                 `{"code":0,"data":{"access_token":"biz-token"}}`,
				"/api/biz/customer/getCustomerInfo": `{"code":0,"data":{"organizations":[]}}`,
			},
			errSubstr: "no organization",
		},
		{
			name:  "no project",
			ready: &ReadyResult{ZaiAccessToken: "zai-token"},
			responses: map[string]string{
				"/api/auth/z/login":                 `{"code":0,"data":{"access_token":"biz-token"}}`,
				"/api/biz/customer/getCustomerInfo": `{"code":0,"data":{"organizations":[{"organizationId":"org-1","organizationName":"Org","projects":[]}]}}`,
			},
			errSubstr: "no project",
		},
		{
			name:  "missing secret",
			ready: &ReadyResult{ZaiAccessToken: "zai-token"},
			responses: map[string]string{
				"/api/auth/z/login":                                                 `{"code":0,"data":{"access_token":"biz-token"}}`,
				"/api/biz/customer/getCustomerInfo":                                 `{"code":0,"data":{"organizations":[{"organizationId":"org-1","organizationName":"Org","projects":[{"projectId":"proj-1","projectName":"Proj"}]}]}}`,
				"/api/biz/v1/organization/org-1/projects/proj-1/api_keys":           `{"code":0,"data":[{"name":"zcode-api-key","apiKey":"ak-1"}]}`,
				"/api/biz/v1/organization/org-1/projects/proj-1/api_keys/copy/ak-1": `{"code":0,"data":{}}`,
			},
			errSubstr: "missing secretKey",
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			transport := zaiRoundTripFunc(func(req *http.Request) (*http.Response, error) {
				body, ok := test.responses[req.URL.Path]
				if !ok {
					t.Fatalf("unexpected request: %s %s", req.Method, req.URL.String())
				}
				return zaiJSONResponse(req, http.StatusOK, body), nil
			})
			_, _, err := newTestZaiAuth(transport).MintAPIKey(context.Background(), test.ready)
			if err == nil || !strings.Contains(err.Error(), test.errSubstr) {
				t.Fatalf("err=%v, want substring %q", err, test.errSubstr)
			}
		})
	}
}

func TestCredentialFileNamePrefersEmailThenUserID(t *testing.T) {
	if got := CredentialFileName("user-1", "dev@example.com"); got != "zai-dev@example.com.json" {
		t.Fatalf("email file name=%q", got)
	}
	if got := CredentialFileName("user/1", ""); got != "zai-user-1.json" {
		t.Fatalf("user id file name=%q", got)
	}
	if got := CredentialFileName("", ""); !strings.HasPrefix(got, "zai-") || !strings.HasSuffix(got, ".json") {
		t.Fatalf("fallback file name=%q", got)
	}
}
