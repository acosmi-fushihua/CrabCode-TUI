#[cfg(feature = "use-dev-tty")]
pub(crate) mod tty;

#[cfg(feature = "bracketed-paste")]
pub(crate) mod bounded_paste;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) mod mio;

#[cfg(feature = "use-dev-tty")]
pub(crate) use self::tty::UnixInternalEventSource;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) use self::mio::UnixInternalEventSource;
