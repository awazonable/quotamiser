//! Incremental Server-Sent Events framing.
//!
//! Derived from tokenmiser, `crates/tokenmiser-proxy/src/sse.rs` at commit
//! 5fe22e826a0fde09b6910b273dc45bed24316f9f. MIT License, Copyright (c) 2026
//! Open Intelligence Labs contributors; see LICENSE. Ported: event boundary
//! detection across every legal line terminator, the handling of a CRLF split
//! across packets, the buffer cap, and their tests. Not ported: the Chat
//! Completions accumulator.
//!
//! One behaviour differs from the original. tokenmiser skipped an event it
//! could not decode and carried on. Here an undecodable event, or a buffer
//! overflow, fails the splitter permanently: the skipped event might have
//! been the one carrying the response id or the final usage, so a caller must
//! know the stream can no longer be relied on for settlement.

/// Cap on an event still being assembled. An upstream that never emits a
/// blank line would otherwise grow the buffer for the life of the stream.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The `event:` field, when present.
    pub event: Option<String>,
    /// Every `data:` line of the event, joined with `\n`.
    pub data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FramingError {
    #[error("an SSE event exceeded {MAX_EVENT_BYTES} bytes without terminating")]
    EventTooLarge,
    #[error("an SSE event was not valid UTF-8")]
    InvalidUtf8,
}

#[derive(Debug, Default)]
pub struct EventSplitter {
    buf: Vec<u8>,
    /// The previous packet ended on a blank line whose last terminator was a
    /// bare `\r`. If the next packet starts with the `\n` completing that CRLF
    /// pair, it is swallowed rather than read as a second terminator.
    swallow_leading_lf: bool,
    failure: Option<FramingError>,
}

impl EventSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one packet and returns the events it completed. A trailing
    /// partial event stays buffered. Events completed before a failure in the
    /// same packet are still returned; nothing is returned after it.
    pub fn push(&mut self, packet: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        if self.failure.is_some() {
            return events;
        }
        let mut packet = packet;
        if std::mem::take(&mut self.swallow_leading_lf)
            && self.buf.is_empty()
            && packet.first() == Some(&b'\n')
        {
            packet = &packet[1..];
        }
        self.buf.extend_from_slice(packet);

        let mut last_event_ended_cr = false;
        while let Some(end) = find_event_end(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..end).collect();
            last_event_ended_cr = raw.last() == Some(&b'\r');
            match std::str::from_utf8(&raw) {
                Ok(text) => events.extend(parse_event(text)),
                Err(_) => {
                    self.fail(FramingError::InvalidUtf8);
                    return events;
                }
            }
        }
        self.swallow_leading_lf = last_event_ended_cr && self.buf.is_empty();
        if self.buf.len() > MAX_EVENT_BYTES {
            self.fail(FramingError::EventTooLarge);
        }
        events
    }

    /// Set once the splitter has lost track of the stream. Permanent.
    pub fn failure(&self) -> Option<FramingError> {
        self.failure
    }

    /// True while the bytes seen so far end inside an unterminated event.
    pub fn is_mid_event(&self) -> bool {
        !self.buf.is_empty()
    }

    fn fail(&mut self, error: FramingError) {
        self.failure = Some(error);
        self.buf = Vec::new();
        self.swallow_leading_lf = false;
    }
}

/// Index one past the end of the first complete event in `buf`, or `None`
/// for a partial event.
///
/// Per WHATWG HTML §9.2.5 a line ends with CRLF, LF, or CR, and an event ends
/// at a blank line: two consecutive terminators in any mix.
fn find_event_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    let mut prev_was_terminator = false;
    while i < buf.len() {
        let term_len = match buf[i] {
            b'\n' => 1,
            b'\r' => {
                if i + 1 >= buf.len() {
                    // A trailing CR after a terminator completes the blank line
                    // whether or not an LF follows in the next packet (push
                    // swallows that LF). Otherwise it only ends a line, and
                    // consuming it here would split one event into two.
                    return if prev_was_terminator {
                        Some(i + 1)
                    } else {
                        None
                    };
                }
                if buf[i + 1] == b'\n' { 2 } else { 1 }
            }
            _ => 0,
        };
        if term_len == 0 {
            prev_was_terminator = false;
            i += 1;
        } else {
            i += term_len;
            if prev_was_terminator {
                return Some(i);
            }
            prev_was_terminator = true;
        }
    }
    None
}

/// An event is dispatched only when it has at least one `data` line.
fn parse_event(text: &str) -> Option<Event> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in text.split(['\r', '\n']) {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_string()),
            "data" => data.push(value),
            _ => {}
        }
    }
    (!data.is_empty()).then(|| Event {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datas(events: &[Event]) -> Vec<&str> {
        events.iter().map(|e| e.data.as_str()).collect()
    }

    #[test]
    fn events_split_across_packets_mid_utf8() {
        let mut s = EventSplitter::new();
        let event = "data: héllo\n\n".as_bytes();
        let split = "data: h".len() + 1; // inside the two-byte é
        assert!(
            s.push(&event[..split]).is_empty(),
            "a partial event must stay buffered"
        );
        assert_eq!(datas(&s.push(&event[split..])), ["héllo"]);
    }

    #[test]
    fn comments_are_ignored() {
        let mut s = EventSplitter::new();
        assert!(s.push(b": keepalive\n\n").is_empty());
        assert_eq!(datas(&s.push(b"data: x\n\n")), ["x"]);
    }

    #[test]
    fn crlf_terminated_events_are_parsed() {
        let mut s = EventSplitter::new();
        assert_eq!(datas(&s.push(b"data: hi\r\n\r\n")), ["hi"]);
        assert_eq!(
            datas(&s.push(b"data: stop\r\n\r\ndata: [DONE]\r\n\r\n")),
            ["stop", "[DONE]"]
        );
    }

    #[test]
    fn cr_only_terminated_events_are_parsed() {
        let mut s = EventSplitter::new();
        assert_eq!(
            datas(&s.push(b"data: x\r\rdata: [DONE]\r\r")),
            ["x", "[DONE]"]
        );
    }

    #[test]
    fn crlf_split_across_packets_terminates_exactly_once() {
        let mut s = EventSplitter::new();
        assert_eq!(
            datas(&s.push(b"data: y\r\n\r")),
            ["y"],
            "CRLF then CR is already a blank line"
        );
        assert!(!s.is_mid_event());
        assert_eq!(
            datas(&s.push(b"\ndata: z\r\n\r\n")),
            ["z"],
            "the split pair's LF must not terminate again"
        );
        assert!(!s.is_mid_event());
    }

    #[test]
    fn a_lone_cr_split_across_packets_stays_buffered() {
        let mut s = EventSplitter::new();
        assert!(
            s.push(b"data: q\r").is_empty(),
            "one terminator is not an event end"
        );
        assert!(s.is_mid_event());
        assert_eq!(datas(&s.push(b"\rdata: [DONE]\r\r")), ["q", "[DONE]"]);
    }

    #[test]
    fn mixed_terminators_do_not_stall_the_parser() {
        let mut s = EventSplitter::new();
        assert_eq!(
            datas(&s.push(b"data: a\n\rdata: b\r\n\rdata: [DONE]\n\n")),
            ["a", "b", "[DONE]"]
        );
        assert!(!s.is_mid_event());
    }

    #[test]
    fn the_mid_event_boundary_is_tracked() {
        let mut s = EventSplitter::new();
        assert!(!s.is_mid_event());
        s.push(b"data: {\"del");
        assert!(s.is_mid_event());
        s.push(b"ta\":1}\n\n");
        assert!(!s.is_mid_event());
        s.push(b"data: x\r\n");
        assert!(s.is_mid_event());
        s.push(b"\r\n");
        assert!(!s.is_mid_event());
    }

    #[test]
    fn an_unterminated_event_trips_the_cap_and_stays_failed() {
        let mut s = EventSplitter::new();
        let junk = format!("data: {}", "A".repeat(2 * MAX_EVENT_BYTES));
        assert!(s.push(junk.as_bytes()).is_empty());
        assert_eq!(s.failure(), Some(FramingError::EventTooLarge));
        assert!(!s.is_mid_event(), "the partial buffer must be released");
        assert!(
            s.push(b"data: late\n\n").is_empty(),
            "nothing after a lost boundary can be trusted"
        );
    }

    #[test]
    fn garbage_without_newlines_does_not_grow_the_buffer() {
        let mut s = EventSplitter::new();
        let junk = vec![b'a'; 64 * 1024];
        for _ in 0..64 {
            s.push(&junk);
        }
        assert!(s.buf.len() <= MAX_EVENT_BYTES);
        assert_eq!(s.failure(), Some(FramingError::EventTooLarge));
    }

    #[test]
    fn event_names_and_multiline_data() {
        let mut s = EventSplitter::new();
        let events = s.push(b"event: response.completed\ndata: line1\ndata: line2\n\n");
        assert_eq!(
            events,
            [Event {
                event: Some("response.completed".into()),
                data: "line1\nline2".into()
            }]
        );
    }

    #[test]
    fn an_event_without_data_is_not_dispatched() {
        let mut s = EventSplitter::new();
        assert!(s.push(b"event: ping\n\n").is_empty());
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        let mut s = EventSplitter::new();
        assert_eq!(datas(&s.push(b"data:  two\n\n")), [" two"]);
        assert_eq!(datas(&s.push(b"data:none\n\n")), ["none"]);
    }

    #[test]
    fn invalid_utf8_fails_permanently_but_keeps_earlier_events() {
        let mut s = EventSplitter::new();
        let events = s.push(b"data: good\n\ndata: \xff\n\ndata: after\n\n");
        assert_eq!(datas(&events), ["good"]);
        assert_eq!(s.failure(), Some(FramingError::InvalidUtf8));
        assert!(s.push(b"data: later\n\n").is_empty());
    }
}
