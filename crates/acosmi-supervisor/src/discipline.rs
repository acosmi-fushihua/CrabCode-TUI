//! Discard-with-discipline macros.
//!
//! Closes Step 2 Phase D.1 — root cause R1 (Step 1 §六): bare
//! `let _ = expr;` in this codebase mixes two semantically distinct
//! cases:
//!
//! 1. **Intentional silent discard** — the value (typically a `Result`
//!    from a fire-and-forget operation) genuinely doesn't need
//!    inspection because the caller has decided silence is correct.
//! 2. **Lost critical state** — a `Result::Err` we don't want to
//!    bother with right now, but operationally it should *surface*
//!    (e.g., a channel send failing because the receiver was dropped
//!    is a useful signal that supervisor shutdown is racing).
//!
//! Bare `let _ =` cannot tell these apart at the call site, and so
//! every reviewer asks the same question every time. These macros
//! force the *spelling* to match the intent:
//!
//! * [`silent_drop!`] declares case (1) with a one-line reason. The
//!   reason is documentation visible at the call site; the macro emits
//!   no runtime trace.
//! * [`must_log_err!`] declares case (2): if the expression yields
//!   `Err`, log it at `warn` level under a tracing target the caller
//!   chooses, then discard.
//!
//! Both are zero-cost in the success path. The discipline rule is
//! social: **`let _ = …;` should be replaced wherever it appears in
//! `acosmi-supervisor`**. We start with the highest-traffic offenders
//! in D.2 / D.5 / D.6 / D.8 and tighten over time.
//!
//! ## Example — `silent_drop!`
//!
//! Best-effort post-fork chdir, where the next call (`execve`) will
//! surface any real error anyway:
//!
//! ```ignore
//! silent_drop!(libc::chdir(root), "post-fork best-effort: errno surfaces via execve next");
//! ```
//!
//! ## Example — `must_log_err!`
//!
//! Channel send to a watchdog event sink — receiver may have been
//! dropped during shutdown, but if the channel has been closed *unexpectedly*
//! we want it in the log:
//!
//! ```ignore
//! must_log_err!(event_tx.send(report).await, "supervisor.watchdog.health_report");
//! ```

/// Declare an *intentional* discard of a value with a one-line reason.
///
/// The macro evaluates `$expr`, drops the resulting value, and emits
/// no trace. The `$reason` literal is *only* documentation visible at
/// the call site — the compiler does nothing with it. This is by
/// design: the reason is for the human reader; runtime trace would
/// undermine the "intentional silence" semantics.
///
/// Use [`must_log_err!`] instead when you actually want the `Err`
/// branch to be logged.
#[macro_export]
macro_rules! silent_drop {
    ($expr:expr, $reason:literal) => {{
        // The reason is at the call site for reviewers.
        let _ = $expr;
    }};
}

/// Discard a `Result`, but log its `Err` at `warn` level under
/// `$target` first.
///
/// Use this when:
/// * The caller does not want execution to abort on failure (so `?`
///   is wrong).
/// * Silently dropping the error would lose useful operational
///   signal (so `let _ = expr;` is wrong).
///
/// `$target` is a `tracing` target string, conventionally
/// `"<crate>.<subsystem>.<op>"`. Pick one that an operator can grep.
#[macro_export]
macro_rules! must_log_err {
    ($expr:expr, $target:literal) => {{
        if let ::std::result::Result::Err(__err) = $expr {
            ::tracing::warn!(
                target: $target,
                error = %__err,
                "operation failed but execution continues",
            );
        }
    }};
}

#[cfg(test)]
mod tests {
    // Macros are exported at crate root via `#[macro_export]`. Inside
    // the same crate, `use crate::silent_drop;` would compile but
    // emit `unused_imports` warnings (a known rustc quirk for
    // re-importing one's own #[macro_export] macros). Use the
    // fully-qualified `crate::silent_drop!` invocation to dodge it.

    /// `silent_drop!` accepts arbitrary expressions and returns `()`.
    #[test]
    fn silent_drop_compiles_for_various_types() {
        // Result
        crate::silent_drop!(Ok::<(), &str>(()), "test: Ok variant");
        crate::silent_drop!(Err::<(), &str>("boom"), "test: Err variant must not panic");
        // Option
        crate::silent_drop!(Some(42_i32), "test: Some");
        crate::silent_drop!(None::<i32>, "test: None");
        // Plain value
        crate::silent_drop!(123_i32, "test: i32");
    }

    /// `must_log_err!` evaluates the expression exactly once and only
    /// runs the `tracing::warn!` branch on `Err`. The macro returns
    /// `()` either way.
    #[test]
    fn must_log_err_evaluates_once_and_returns_unit() {
        use std::cell::Cell;

        // Ok branch — no log, no panic.
        let counter = Cell::new(0_u32);
        crate::must_log_err!(
            {
                counter.set(counter.get() + 1);
                Ok::<(), &str>(())
            },
            "test.must_log_err.ok"
        );
        assert_eq!(counter.get(), 1, "expression evaluated exactly once on Ok");

        // Err branch — must not panic. The actual log emission isn't
        // observable from this test (we don't install a subscriber);
        // the contract verified here is "no panic, no double-eval".
        let err_counter = Cell::new(0_u32);
        crate::must_log_err!(
            {
                err_counter.set(err_counter.get() + 1);
                Err::<(), &str>("intentional error")
            },
            "test.must_log_err.err"
        );
        assert_eq!(
            err_counter.get(),
            1,
            "expression evaluated exactly once on Err"
        );
    }
}
