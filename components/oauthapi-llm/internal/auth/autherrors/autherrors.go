// Package autherrors carries the classification sentinels shared by the
// connector auth packages (xai/kimi/qwen/zai) and consumed by the account
// bridge login facade to project a bounded poll errorCode enum.
//
// Contract (V2, 2026-08-08 账户接入审计 §7 PR-α-3): the facade may only emit
// the enum pinned by the component-local direct-TUI login contract. The
// waiters in internal/api/handlers/management map these sentinels onto that
// enum with errors.Is; an unclassified error collapses to the residual
// "login_failed", never to a new ad-hoc value.
package autherrors

import "errors"

var (
	// ErrAuthorizationDenied — the user (or the provider on the user's
	// behalf) declined the authorization. Deterministic; retrying the same
	// session cannot succeed.
	ErrAuthorizationDenied = errors.New("authorization denied")
	// ErrAuthorizationTimeout — the device code / callback window expired
	// before the user completed authorization.
	ErrAuthorizationTimeout = errors.New("authorization timed out")
	// ErrUpstreamUnavailable — transport failure, 5xx, or a malformed
	// upstream response. The provider could not be talked to properly.
	ErrUpstreamUnavailable = errors.New("upstream unavailable")
)

// classified attaches a sentinel kind to an error while keeping the exact
// original message text — existing message-level assertions and forensics
// stay byte-identical.
type classified struct {
	msg  string
	kind error
}

func (c classified) Error() string { return c.msg }

func (c classified) Unwrap() error { return c.kind }

// Classify wraps err's message with kind. A nil err returns nil.
func Classify(kind error, err error) error {
	if err == nil {
		return nil
	}
	return classified{msg: err.Error(), kind: kind}
}
