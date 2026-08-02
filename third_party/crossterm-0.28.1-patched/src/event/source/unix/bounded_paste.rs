//! Bounded bracketed-paste payload parser.
//!
//! Upstream 0.28.1 keeps the entire sequence in the generic ANSI buffer until
//! `ESC[201~` arrives. A missing terminator therefore grows that buffer for as
//! long as the terminal keeps sending bytes. This parser recognizes the start
//! marker in the Unix event sources, retains only a fixed payload prefix, and
//! then discards bytes while still matching the exact end marker. Payload
//! bytes can never escape as key events.

pub(crate) const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
pub(crate) const MAX_BRACKETED_PASTE_BYTES: usize = 8 * 1024 * 1024;
const RESERVE_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) struct BoundedPaste {
    retained: Vec<u8>,
    overflowed: bool,
    end_prefix_len: usize,
}

impl Default for BoundedPaste {
    fn default() -> Self {
        Self {
            retained: Vec::with_capacity(RESERVE_CHUNK_BYTES),
            overflowed: false,
            end_prefix_len: 0,
        }
    }
}

impl BoundedPaste {
    /// Consume one payload byte. Returns exactly one paste when the end marker
    /// completes, including when that marker is split across terminal reads.
    pub(crate) fn advance(&mut self, byte: u8) -> Option<String> {
        if byte == BRACKETED_PASTE_END[self.end_prefix_len] {
            self.end_prefix_len += 1;
            if self.end_prefix_len == BRACKETED_PASTE_END.len() {
                self.end_prefix_len = 0;
                return Some(self.finish());
            }
            return None;
        }

        if self.end_prefix_len != 0 {
            for pending in &BRACKETED_PASTE_END[..self.end_prefix_len] {
                self.retain(*pending);
            }
            self.end_prefix_len = 0;
            // The only byte shared by the end marker's prefix and a possible
            // new prefix is ESC. Re-evaluate it instead of treating it as
            // ordinary payload.
            if byte == BRACKETED_PASTE_END[0] {
                self.end_prefix_len = 1;
                return None;
            }
        }
        self.retain(byte);
        None
    }

    fn retain(&mut self, byte: u8) {
        if self.retained.len() == MAX_BRACKETED_PASTE_BYTES {
            self.overflowed = true;
            return;
        }
        if self.retained.len() == self.retained.capacity() {
            let remaining = MAX_BRACKETED_PASTE_BYTES - self.retained.len();
            self.retained
                .reserve_exact(RESERVE_CHUNK_BYTES.min(remaining));
        }
        self.retained.push(byte);
    }

    fn finish(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.retained).into_owned();
        // The CrabCode input layer has the same 8 MiB ceiling and reports
        // truncation when the Event::Paste string is longer than it. Preserve
        // that signal without allowing a discarded byte into user content:
        // the extra ASCII byte is itself removed by the application ceiling.
        if self.overflowed && text.len() <= MAX_BRACKETED_PASTE_BYTES {
            text.push(' ');
        }
        text
    }

    #[cfg(test)]
    fn retained_len(&self) -> usize {
        self.retained.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finish(parser: &mut BoundedPaste) -> String {
        let mut result = None;
        for byte in BRACKETED_PASTE_END {
            result = parser.advance(*byte).or(result);
        }
        result.expect("end marker must finish one paste")
    }

    #[test]
    fn unterminated_input_has_a_fixed_retained_prefix() {
        let mut parser = BoundedPaste::default();
        for _ in 0..(MAX_BRACKETED_PASTE_BYTES * 2) {
            assert!(parser.advance(b'x').is_none());
        }
        assert_eq!(parser.retained_len(), MAX_BRACKETED_PASTE_BYTES);
        assert!(parser.overflowed);
    }

    #[test]
    fn end_marker_can_cross_arbitrary_read_boundaries() {
        let mut parser = BoundedPaste::default();
        for byte in b"payload\x1b[20Xinside" {
            assert!(parser.advance(*byte).is_none());
        }
        let text = finish(&mut parser);
        assert_eq!(text, "payload\x1b[20Xinside");
    }

    #[test]
    fn false_terminator_prefix_can_restart_at_a_second_escape() {
        let mut parser = BoundedPaste::default();
        for byte in b"before\x1b[20\x1b[201~" {
            if let Some(text) = parser.advance(*byte) {
                assert_eq!(text, "before\x1b[20");
                return;
            }
        }
        panic!("the second escape must begin the real end marker");
    }

    #[test]
    fn overflow_emits_one_truncation_signalling_paste() {
        let mut parser = BoundedPaste::default();
        for _ in 0..(MAX_BRACKETED_PASTE_BYTES + 17) {
            assert!(parser.advance(b'a').is_none());
        }
        let text = finish(&mut parser);
        assert_eq!(text.len(), MAX_BRACKETED_PASTE_BYTES + 1);
        assert!(text[..MAX_BRACKETED_PASTE_BYTES]
            .bytes()
            .all(|byte| byte == b'a'));
    }

    #[test]
    fn utf8_cut_at_the_limit_remains_valid_event_text() {
        let mut parser = BoundedPaste::default();
        for _ in 0..(MAX_BRACKETED_PASTE_BYTES - 1) {
            parser.advance(b'a');
        }
        for byte in "界".as_bytes() {
            parser.advance(*byte);
        }
        let text = finish(&mut parser);
        assert!(text.len() > MAX_BRACKETED_PASTE_BYTES);
        assert!(text.is_char_boundary(text.len()));
    }
}
