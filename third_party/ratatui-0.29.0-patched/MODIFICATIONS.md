# CrabCode modifications to Ratatui 0.29.0

Upstream project: <https://github.com/ratatui/ratatui>

Upstream version: `0.29.0`

License: MIT (see the adjacent `LICENSE` file)

This directory starts from the crates.io Ratatui 0.29.0 source and is patched
for CrabCode's native terminal client. The material changes are:

- fix flat-buffer index-to-coordinate conversion so buffers larger than
  `u16::MAX` cells do not wrap before division/modulo;
- add a frame-local OSC 8 hyperlink layer, hyperlink-aware cell diffing and
  balanced open/close emission, including control- and bidi-character
  sanitization of link targets;
- make hyperlink additions, removals and retargeting invalidate otherwise
  unchanged cells, while unchanged link/cell frames emit no output;
- add an explicit inline-viewport height update used when the host terminal
  grows or shrinks, preserving native scrollback behavior;
- reset/resize hyperlink buffers together with Ratatui's cell buffers;
- re-export the added `HyperlinkSpan` API;
- add regression tests for wide glyphs, non-zero viewport origins, OSC 8
  injection resistance, large buffers and inline viewport resize;
- add local workspace/lint metadata so the vendored crate can be tested
  independently inside this repository.

The upstream copyright and MIT license are unchanged. CrabCode's changes are
not represented as an upstream Ratatui release.
