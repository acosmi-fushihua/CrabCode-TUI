package management

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/config"
	"github.com/gin-gonic/gin"
)

func TestOAuthSessionStoreCompleteKeepsShortLivedSession(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	store.Register("completed-state", "codex")

	store.Complete("completed-state")

	if _, ok := store.Get("completed-state"); !ok {
		t.Fatal("completed OAuth session was deleted instead of retained as a tombstone")
	}
	if store.IsPending("completed-state", "codex") {
		t.Fatal("completed OAuth session remained pending")
	}
}

func TestOAuthSessionCompletionRetainsExactDefensiveResultAuthIDs(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	replaceOAuthSessionStoreForTest(t, store)
	store.Register("result-state", "codex")

	CompleteOAuthSessionWithAuthIDs("result-state", " auth-a ", "auth-a", "auth-b", "")
	authIDs, completed, ok := GetOAuthSessionResultAuthIDs("result-state")
	if !ok || !completed {
		t.Fatalf("result session completed/ok = %t/%t, want true/true", completed, ok)
	}
	if got := strings.Join(authIDs, ","); got != "auth-a,auth-b" {
		t.Fatalf("result auth IDs = %q, want deduplicated exact identities", got)
	}

	authIDs[0] = "mutated"
	authIDsAgain, _, _ := GetOAuthSessionResultAuthIDs("result-state")
	if got := strings.Join(authIDsAgain, ","); got != "auth-a,auth-b" {
		t.Fatalf("stored result identities were mutated through returned slice: %q", got)
	}
}

func TestOAuthSessionStoreCompleteDoesNotExtendCompletedSession(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	store.Register("completed-state", "codex")
	store.Complete("completed-state")
	before, ok := store.Get("completed-state")
	if !ok {
		t.Fatal("completed OAuth session tombstone is missing")
	}

	store.completedTTL = 2 * time.Minute
	store.Complete("completed-state")
	after, ok := store.Get("completed-state")
	if !ok {
		t.Fatal("completed OAuth session tombstone is missing after repeated completion")
	}
	if !after.ExpiresAt.Equal(before.ExpiresAt) {
		t.Fatalf("repeated completion extended expiry from %s to %s", before.ExpiresAt, after.ExpiresAt)
	}
}

func TestOAuthSessionStoreCompleteProviderSkipsCompletedSessions(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	store.Register("completed-state", "codex")
	store.Register("pending-state", "codex")
	store.Complete("completed-state")
	completedBefore, ok := store.Get("completed-state")
	if !ok {
		t.Fatal("completed OAuth session tombstone is missing")
	}

	store.completedTTL = 2 * time.Minute
	if got := store.CompleteProvider("codex", oauthSessionSourceBuiltin); got != 1 {
		t.Fatalf("CompleteProvider() = %d, want 1 newly completed session", got)
	}
	completedAfter, ok := store.Get("completed-state")
	if !ok {
		t.Fatal("completed OAuth session tombstone is missing after provider completion")
	}
	if !completedAfter.ExpiresAt.Equal(completedBefore.ExpiresAt) {
		t.Fatalf("provider completion extended existing tombstone from %s to %s", completedBefore.ExpiresAt, completedAfter.ExpiresAt)
	}
	pendingAfter, ok := store.Get("pending-state")
	if !ok || !pendingAfter.Completed {
		t.Fatalf("pending session completed/ok = %t/%t, want true/true", pendingAfter.Completed, ok)
	}
}

func TestGetOAuthSessionHidesCompletedSession(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	replaceOAuthSessionStoreForTest(t, store)
	store.Register("completed-state", "codex")
	store.Complete("completed-state")

	provider, status, ok := GetOAuthSession("completed-state")
	if ok {
		t.Fatalf("GetOAuthSession() = (%q, %q, true), want completed session hidden", provider, status)
	}

	_, _, _, _, completed, detailsOK := GetOAuthSessionDetails("completed-state")
	if !detailsOK || !completed {
		t.Fatalf("GetOAuthSessionDetails() completed/ok = %t/%t, want true/true", completed, detailsOK)
	}
}

func TestGetAuthStatusRejectsUnknownStateAndAcceptsCompletedState(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	replaceOAuthSessionStoreForTest(t, store)

	handler := &Handler{}
	router := gin.New()
	router.GET("/status", handler.GetAuthStatus)

	unknown := performOAuthStatusRequest(t, router, "unknown-state")
	if unknown.Status != "error" || unknown.Error != "unknown or expired state" {
		t.Fatalf("unknown state response = %#v, want unknown/expired error", unknown)
	}

	store.Register("completed-state", "codex")
	store.Complete("completed-state")
	completed := performOAuthStatusRequest(t, router, "completed-state")
	if completed.Status != "ok" || completed.Error != "" {
		t.Fatalf("completed state response = %#v, want success", completed)
	}
}

func TestOAuthCallbackRejectsCompletedSession(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	replaceOAuthSessionStoreForTest(t, store)
	store.Register("completed-state", "codex")
	store.Complete("completed-state")

	handler := NewHandlerWithoutConfigFilePath(&config.Config{AuthDir: t.TempDir()}, nil)
	router := gin.New()
	router.POST("/oauth-callback", handler.PostOAuthCallback)

	req := httptest.NewRequest(
		http.MethodPost,
		"/oauth-callback",
		strings.NewReader(`{"provider":"codex","state":"completed-state","code":"test-code"}`),
	)
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)

	if w.Code != http.StatusConflict {
		t.Fatalf("completed callback status = %d, want %d; body=%s", w.Code, http.StatusConflict, w.Body.String())
	}
}

type oauthStatusResponse struct {
	Status string `json:"status"`
	Error  string `json:"error"`
}

func performOAuthStatusRequest(t *testing.T, router http.Handler, state string) oauthStatusResponse {
	t.Helper()
	req := httptest.NewRequest(http.MethodGet, "/status?state="+state, nil)
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("status request returned %d, want %d; body=%s", w.Code, http.StatusOK, w.Body.String())
	}
	var response oauthStatusResponse
	if errDecode := json.Unmarshal(w.Body.Bytes(), &response); errDecode != nil {
		t.Fatalf("decode status response: %v", errDecode)
	}
	return response
}

func TestOAuthSessionStoreCancelRemovesPendingSession(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	store.Register("pending-state", "xai")

	if outcome := store.Cancel("pending-state"); outcome != oauthCancelPending {
		t.Fatalf("Cancel() = %v, want oauthCancelPending", outcome)
	}
	if store.IsPending("pending-state", "xai") {
		t.Fatal("cancelled session remained pending")
	}
	if _, ok := store.Get("pending-state"); ok {
		t.Fatal("cancelled session still present in store")
	}
	// F5: a second cancel is an idempotent terminal confirmation, not a failure.
	if outcome := store.Cancel("pending-state"); outcome != oauthCancelAlreadyTerminal {
		t.Fatalf("second Cancel() = %v, want oauthCancelAlreadyTerminal", outcome)
	}
}

func TestOAuthSessionStoreCancelIgnoresCompletedAndUnknown(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	store.Register("completed-state", "codex")
	store.Complete("completed-state")

	if outcome := store.Cancel("completed-state"); outcome != oauthCancelAlreadyTerminal {
		t.Fatalf("Cancel() completed session = %v, want oauthCancelAlreadyTerminal", outcome)
	}
	if _, ok := store.Get("completed-state"); !ok {
		t.Fatal("completed tombstone was removed by Cancel")
	}
	if outcome := store.Cancel("missing-state"); outcome != oauthCancelAlreadyTerminal {
		t.Fatalf("Cancel() unknown session = %v, want oauthCancelAlreadyTerminal", outcome)
	}
}

func TestOAuthSessionStoreCancelIgnoresErrorSession(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	store.Register("error-state", "kimi")
	store.SetError("error-state", "Authentication failed")

	if store.IsPending("error-state", "kimi") {
		t.Fatal("error session should not be pending")
	}
	if outcome := store.Cancel("error-state"); outcome != oauthCancelAlreadyTerminal {
		t.Fatalf("Cancel() error session = %v, want oauthCancelAlreadyTerminal", outcome)
	}
}

func TestCancelOAuthSessionAndCallbackRejectAfterCancel(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	replaceOAuthSessionStoreForTest(t, store)
	store.Register("callback-state", "anthropic")

	if !CancelOAuthSession("callback-state") {
		t.Fatal("CancelOAuthSession() = false, want true")
	}
	if IsOAuthSessionPending("callback-state", "anthropic") {
		t.Fatal("session still pending after cancel")
	}

	_, errWrite := WriteOAuthCallbackFileForPendingSession(t.TempDir(), "anthropic", "callback-state", "code", "")
	if errWrite == nil {
		t.Fatal("expected callback write to fail after cancel")
	}
	if !errors.Is(errWrite, errOAuthSessionNotPending) {
		t.Fatalf("callback write error = %v, want %v", errWrite, errOAuthSessionNotPending)
	}
}

func TestOAuthSessionSaveClaimRejectsCancelledCompletedErroredAndWrongProvider(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	replaceOAuthSessionStoreForTest(t, store)

	providers := []string{"anthropic", "codex", "antigravity", "xai", "kimi"}
	for _, provider := range providers {
		state := provider + "-save-cancelled"
		store.Register(state, provider)
		if !CancelOAuthSession(state) {
			t.Fatalf("%s CancelOAuthSession() = false, want true", provider)
		}
		if errClaim := claimOAuthSessionForSave(state, provider); !errors.Is(errClaim, errOAuthSessionNotPending) {
			t.Fatalf("%s after cancel claim error = %v, want %v", provider, errClaim, errOAuthSessionNotPending)
		}
	}

	// Completed and errored sessions must also refuse save.
	store.Register("completed-save", "codex")
	store.Complete("completed-save")
	if errClaim := claimOAuthSessionForSave("completed-save", "codex"); !errors.Is(errClaim, errOAuthSessionNotPending) {
		t.Fatalf("completed claim error = %v, want %v", errClaim, errOAuthSessionNotPending)
	}

	store.Register("error-save", "anthropic")
	store.SetError("error-save", "Authentication failed")
	if errClaim := claimOAuthSessionForSave("error-save", "anthropic"); !errors.Is(errClaim, errOAuthSessionNotPending) {
		t.Fatalf("error claim error = %v, want %v", errClaim, errOAuthSessionNotPending)
	}

	store.Register("wrong-provider-save", "codex")
	if errClaim := claimOAuthSessionForSave("wrong-provider-save", "anthropic"); !errors.Is(errClaim, errOAuthSessionNotPending) {
		t.Fatalf("wrong-provider claim error = %v, want %v", errClaim, errOAuthSessionNotPending)
	}
}

func TestOAuthSessionSaveClaimAndCancelHaveOneAtomicWinner(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	for iteration := range 250 {
		state := fmt.Sprintf("save-race-%d", iteration)
		store.Register(state, "codex")
		start := make(chan struct{})
		var claimWon bool
		var cancelWon bool
		var waitGroup sync.WaitGroup
		waitGroup.Add(2)
		go func() {
			defer waitGroup.Done()
			<-start
			claimWon = store.ClaimSave(state, "codex")
		}()
		go func() {
			defer waitGroup.Done()
			<-start
			cancelWon = store.Cancel(state) == oauthCancelPending
		}()
		close(start)
		waitGroup.Wait()
		if claimWon == cancelWon {
			t.Fatalf("iteration %d claim/cancel winners = %t/%t, want exactly one", iteration, claimWon, cancelWon)
		}
		if claimWon {
			if outcome := store.Cancel(state); outcome != oauthCancelSaveInFlight {
				t.Fatalf("iteration %d cancel after save claim = %v, want oauthCancelSaveInFlight", iteration, outcome)
			}
			if !store.CompleteClaimedSave(state, "codex", "auth-id") {
				t.Fatalf("iteration %d claimed save did not complete", iteration)
			}
		} else if store.ClaimSave(state, "codex") {
			t.Fatalf("iteration %d save claim succeeded after cancellation", iteration)
		}
	}
}

func TestPluginOAuthPollClaimIsSingleFlightCancellableUntilSavePromotion(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	if err := store.RegisterPlugin("plugin-cancel", "gemini-cli", map[string]any{"nonce": "sanitized"}); err != nil {
		t.Fatalf("RegisterPlugin: %v", err)
	}

	start := make(chan struct{})
	results := make(chan bool, 16)
	var waitGroup sync.WaitGroup
	for range 16 {
		waitGroup.Add(1)
		go func() {
			defer waitGroup.Done()
			<-start
			results <- store.ClaimPoll("plugin-cancel", "gemini-cli")
		}()
	}
	close(start)
	waitGroup.Wait()
	close(results)
	winners := 0
	for won := range results {
		if won {
			winners++
		}
	}
	if winners != 1 {
		t.Fatalf("concurrent plugin poll winners = %d, want 1", winners)
	}
	if outcome := store.Cancel("plugin-cancel"); outcome != oauthCancelPending {
		t.Fatalf("polling plugin session cancel = %v, want oauthCancelPending", outcome)
	}
	if store.PromotePollToSave("plugin-cancel", "gemini-cli") {
		t.Fatal("cancelled plugin poll promoted to credential save")
	}

	if err := store.RegisterPlugin("plugin-success", "gemini-cli", nil); err != nil {
		t.Fatalf("RegisterPlugin success path: %v", err)
	}
	if !store.ClaimPoll("plugin-success", "gemini-cli") || store.ClaimPoll("plugin-success", "gemini-cli") {
		t.Fatal("plugin poll claim was not exclusive")
	}
	if !store.PromotePollToSave("plugin-success", "gemini-cli") {
		t.Fatal("plugin poll did not promote to save")
	}
	if outcome := store.Cancel("plugin-success"); outcome != oauthCancelSaveInFlight {
		t.Fatalf("cancel after plugin save promotion = %v, want oauthCancelSaveInFlight", outcome)
	}
	if !store.CompleteClaimedSave("plugin-success", "gemini-cli", "plugin-auth") {
		t.Fatal("promoted plugin save did not complete")
	}
	session, ok := store.Get("plugin-success")
	if !ok || !session.Completed || strings.Join(session.ResultAuthIDs, ",") != "plugin-auth" {
		t.Fatalf("completed plugin session = %+v ok=%t", session, ok)
	}
}

func TestOAuthSessionClaimedSaveFailurePublishesErrorAndReleasesClaim(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	store.Register("save-failure", "xai")
	if !store.ClaimSave("save-failure", "xai") {
		t.Fatal("save claim failed")
	}
	store.SetError("save-failure", "racing error must not win")
	if !store.FailClaimedSave("save-failure", "xai", "persist failed") {
		t.Fatal("save owner could not publish failure")
	}
	session, ok := store.Get("save-failure")
	if !ok || session.Operation != oauthSessionOperationNone || session.Status != "persist failed" || session.Completed {
		t.Fatalf("failed save session = %+v ok=%t", session, ok)
	}
	if session.ErrorCode != OAuthSessionErrCredentialSaveFailed {
		t.Fatalf("failed save errorCode = %q, want %q", session.ErrorCode, OAuthSessionErrCredentialSaveFailed)
	}
	if outcome := store.Cancel("save-failure"); outcome != oauthCancelAlreadyTerminal {
		t.Fatalf("cancel of terminal save error = %v, want oauthCancelAlreadyTerminal", outcome)
	}
	if store.ClaimSave("save-failure", "xai") {
		t.Fatal("terminal save error became retryable")
	}
}

func TestPanickedOAuthWaiterPublishesBoundedTerminalFailure(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	replaceOAuthSessionStoreForTest(t, store)
	store.Register("panic-state", "xai")
	if !store.ClaimSave("panic-state", "xai") {
		t.Fatal("save claim failed")
	}

	func() {
		defer recoverOAuthWaiterGoroutine("test oauth waiter", "panic-state", "xai")
		panic("synthetic waiter panic")
	}()

	session, ok := store.Get("panic-state")
	if !ok {
		t.Fatal("panicked waiter session disappeared")
	}
	if session.Completed || session.Operation != oauthSessionOperationNone {
		t.Fatalf("panicked waiter session retained an in-flight operation: %+v", session)
	}
	if session.Status != "Authentication failed" {
		t.Fatalf("panicked waiter status = %q, want bounded generic failure", session.Status)
	}
	if session.ErrorCode != OAuthSessionErrLoginFailed {
		t.Fatalf("panicked waiter errorCode = %q, want %q", session.ErrorCode, OAuthSessionErrLoginFailed)
	}
	if outcome := store.Cancel("panic-state"); outcome != oauthCancelAlreadyTerminal {
		t.Fatalf("panicked waiter cancel outcome = %v, want terminal", outcome)
	}
}

func TestPanickedOAuthWaiterCannotOverwriteAnotherProviderOrTerminalState(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	store.Register("provider-state", "codex")
	if store.FailPanickedWaiter("provider-state", "xai") {
		t.Fatal("wrong-provider waiter overwrote session")
	}
	if !store.IsPending("provider-state", "codex") {
		t.Fatal("wrong-provider panic changed pending session")
	}

	store.SetErrorCoded("provider-state", OAuthSessionErrAuthorizationDenied, "denied")
	if store.FailPanickedWaiter("provider-state", "codex") {
		t.Fatal("late waiter panic overwrote terminal session")
	}
	session, ok := store.Get("provider-state")
	if !ok || session.Status != "denied" || session.ErrorCode != OAuthSessionErrAuthorizationDenied {
		t.Fatalf("terminal session changed after late panic: %+v ok=%t", session, ok)
	}
}

func TestCallbackForwarderClosesDoneAfterRecoveredPanic(t *testing.T) {
	done := make(chan struct{})
	runCallbackForwarder("test", done, func() error {
		panic("synthetic forwarder panic")
	})

	select {
	case <-done:
	default:
		t.Fatal("callback forwarder did not close its done channel after panic")
	}
}

func TestCancelAuthSessionHandler(t *testing.T) {
	store := newOAuthSessionStore(time.Minute)
	replaceOAuthSessionStoreForTest(t, store)
	store.Register("device-state", "xai")

	handler := &Handler{}
	router := gin.New()
	router.DELETE("/oauth-session", handler.CancelAuthSession)

	missing := performOAuthCancelRequest(t, router, "")
	if missing.status != http.StatusBadRequest {
		t.Fatalf("missing state status = %d, want %d", missing.status, http.StatusBadRequest)
	}

	invalid := performOAuthCancelRequest(t, router, "bad/state")
	if invalid.status != http.StatusBadRequest {
		t.Fatalf("invalid state status = %d, want %d", invalid.status, http.StatusBadRequest)
	}

	cancelled := performOAuthCancelRequest(t, router, "device-state")
	if cancelled.status != http.StatusOK || !cancelled.cancelled || cancelled.bodyStatus != "ok" {
		t.Fatalf("cancel pending response = %#v, want ok/cancelled", cancelled)
	}
	if IsOAuthSessionPending("device-state", "xai") {
		t.Fatal("device session still pending after cancel API")
	}

	// F5: a repeat cancel is an idempotent terminal confirmation — the direct
	// TUI caller cleans up instead of retrying a dead session forever.
	repeat := performOAuthCancelRequest(t, router, "device-state")
	if repeat.status != http.StatusOK || !repeat.cancelled {
		t.Fatalf("repeat cancel response = %#v, want ok with cancelled=true", repeat)
	}

	// Status after cancel should not report success.
	statusRouter := gin.New()
	statusRouter.GET("/status", handler.GetAuthStatus)
	unknown := performOAuthStatusRequest(t, statusRouter, "device-state")
	if unknown.Status != "error" || unknown.Error != "unknown or expired state" {
		t.Fatalf("status after cancel = %#v, want unknown/expired error", unknown)
	}
}

type oauthCancelResponse struct {
	status     int
	bodyStatus string
	cancelled  bool
}

func performOAuthCancelRequest(t *testing.T, router http.Handler, state string) oauthCancelResponse {
	t.Helper()
	path := "/oauth-session"
	if state != "" {
		path += "?state=" + state
	}
	req := httptest.NewRequest(http.MethodDelete, path, nil)
	w := httptest.NewRecorder()
	router.ServeHTTP(w, req)

	var body struct {
		Status    string `json:"status"`
		Cancelled bool   `json:"cancelled"`
		Error     string `json:"error"`
	}
	if w.Body.Len() > 0 {
		if errDecode := json.Unmarshal(w.Body.Bytes(), &body); errDecode != nil {
			t.Fatalf("decode cancel response: %v body=%s", errDecode, w.Body.String())
		}
	}
	return oauthCancelResponse{
		status:     w.Code,
		bodyStatus: body.Status,
		cancelled:  body.Cancelled,
	}
}

func replaceOAuthSessionStoreForTest(t *testing.T, store *oauthSessionStore) {
	t.Helper()
	original := oauthSessions
	oauthSessions = store
	t.Cleanup(func() {
		oauthSessions = original
	})
}
