//! Default port numbers and derivation rules for Crab Claw services.
//!
//! Bridge, browser-control, canvas, and browser CDP ports are derived
//! from the gateway port with fixed offsets, so that moving the gateway
//! port by `K` moves every dependent port by `K` too.

pub const DEFAULT_BRIDGE_PORT: u16 = 18_790;
pub const DEFAULT_BROWSER_CONTROL_PORT: u16 = 18_791;
pub const DEFAULT_CANVAS_HOST_PORT: u16 = 18_793;
pub const DEFAULT_BROWSER_CDP_PORT_RANGE_START: u16 = 18_800;
pub const DEFAULT_BROWSER_CDP_PORT_RANGE_END: u16 = 18_899;

// Compile-time invariant: a future edit that flips end < start would
// make `derive_default_browser_cdp_port_range` underflow on the `END -
// START` width subtraction. Catch that at build time, not at runtime.
const _: () = assert!(DEFAULT_BROWSER_CDP_PORT_RANGE_END >= DEFAULT_BROWSER_CDP_PORT_RANGE_START);

/// Inclusive port range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

fn derive(base: u16, offset: u16, fallback: u16) -> u16 {
    if base == 0 {
        return fallback;
    }
    base.checked_add(offset).unwrap_or(fallback)
}

/// Derive bridge port from gateway port (offset +1).
#[must_use]
pub fn derive_default_bridge_port(gateway_port: u16) -> u16 {
    derive(gateway_port, 1, DEFAULT_BRIDGE_PORT)
}

/// Derive browser-control port from gateway port (offset +2).
#[must_use]
pub fn derive_default_browser_control_port(gateway_port: u16) -> u16 {
    derive(gateway_port, 2, DEFAULT_BROWSER_CONTROL_PORT)
}

/// Derive canvas host port from gateway port (offset +4).
#[must_use]
pub fn derive_default_canvas_host_port(gateway_port: u16) -> u16 {
    derive(gateway_port, 4, DEFAULT_CANVAS_HOST_PORT)
}

/// Derive browser CDP port range from browser-control port (start offset +9).
/// Range width matches the default [18800, 18899] window.
#[must_use]
pub fn derive_default_browser_cdp_port_range(browser_control_port: u16) -> PortRange {
    let start = derive(
        browser_control_port,
        9,
        DEFAULT_BROWSER_CDP_PORT_RANGE_START,
    );
    let width = DEFAULT_BROWSER_CDP_PORT_RANGE_END - DEFAULT_BROWSER_CDP_PORT_RANGE_START;
    let end = start
        .checked_add(width)
        .unwrap_or(DEFAULT_BROWSER_CDP_PORT_RANGE_END);
    PortRange {
        start,
        end: end.max(start),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_from_valid_gateway_port() {
        assert_eq!(derive_default_bridge_port(19_001), 19_002);
        assert_eq!(derive_default_browser_control_port(19_001), 19_003);
        assert_eq!(derive_default_canvas_host_port(19_001), 19_005);
        let r = derive_default_browser_cdp_port_range(19_003);
        assert_eq!(r.start, 19_012);
        assert_eq!(r.end, 19_012 + 99);
    }

    #[test]
    fn falls_back_when_base_is_zero() {
        assert_eq!(derive_default_bridge_port(0), DEFAULT_BRIDGE_PORT);
        assert_eq!(
            derive_default_browser_control_port(0),
            DEFAULT_BROWSER_CONTROL_PORT
        );
        assert_eq!(derive_default_canvas_host_port(0), DEFAULT_CANVAS_HOST_PORT);
        let r = derive_default_browser_cdp_port_range(0);
        assert_eq!(r.start, DEFAULT_BROWSER_CDP_PORT_RANGE_START);
        assert_eq!(r.end, DEFAULT_BROWSER_CDP_PORT_RANGE_END);
    }

    #[test]
    fn overflow_yields_fallback() {
        assert_eq!(derive_default_bridge_port(u16::MAX), DEFAULT_BRIDGE_PORT);
    }

    #[test]
    fn cdp_range_end_never_below_start() {
        let r = derive_default_browser_cdp_port_range(u16::MAX - 5);
        assert!(r.end >= r.start);
    }
}
