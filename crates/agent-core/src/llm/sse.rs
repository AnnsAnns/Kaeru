//! Incremental SSE parser for provider streams (ADR-005): byte-fed (chunks
//! may split a line mid-UTF-8), multi-`data:` events join with `\n`, comments
//! and unknown fields are ignored, `\n`/`\r\n`/unterminated final lines are
//! accepted, a BOM is stripped. The `[DONE]` sentinel is protocol knowledge
//! and stays with the caller (`llm::client`).

/// Push-based SSE parser: feed raw bytes, receive complete `data:` payloads.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    /// Undecoded bytes of the (possibly partial) current line.
    pending: Vec<u8>,
    /// `data:` values of the event currently being assembled.
    data_lines: Vec<String>,
    /// Whether the stream start has been seen (for BOM stripping).
    started: bool,
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

impl SseParser {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk; returns every complete event's `data` payload.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        if !self.started {
            self.pending.extend_from_slice(bytes);
            // Decide on the BOM only once enough bytes have arrived: a chunk
            // boundary can split it, and deciding early would leak BOM bytes
            // into the first line.
            if self.pending.len() < BOM.len() && !self.pending.contains(&b'\n') {
                return Vec::new();
            }
            self.started = true;
            if self.pending.starts_with(BOM) {
                self.pending.drain(..BOM.len());
            }
        } else {
            self.pending.extend_from_slice(bytes);
        }

        let mut payloads = Vec::new();
        while let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(payload) = self.process_line(&line) {
                payloads.push(payload);
            }
        }
        payloads
    }

    /// Flush at end of stream: processes a trailing line without terminator
    /// and dispatches any pending event (also when only its `data:` lines
    /// arrived, e.g. a stream ending in `data: x\n`).
    pub(crate) fn finish(&mut self) -> Vec<String> {
        let mut payloads = Vec::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            if let Some(payload) = self.process_line(&line) {
                payloads.push(payload);
            }
        }
        if let Some(payload) = self.dispatch() {
            payloads.push(payload);
        }
        payloads
    }

    fn process_line(&mut self, line: &[u8]) -> Option<String> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line[0] == b':' {
            return None;
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line, [].as_slice()),
        };
        if field == b"data" {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            self.data_lines
                .push(String::from_utf8_lossy(value).into_owned());
        }
        // Every other field is deliberately ignored.
        None
    }

    fn dispatch(&mut self) -> Option<String> {
        if self.data_lines.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.data_lines).join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_shot(text: &str) -> Vec<String> {
        let mut parser = SseParser::new();
        let mut out = parser.push(text.as_bytes());
        out.extend(parser.finish());
        out
    }

    fn chunked(chunks: &[&str]) -> Vec<String> {
        let mut parser = SseParser::new();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(parser.push(chunk.as_bytes()));
        }
        out.extend(parser.finish());
        out
    }

    #[test]
    fn parses_basic_event() {
        assert_eq!(one_shot("data: hello\n\n"), vec!["hello"]);
    }

    #[test]
    fn parses_multiple_events_in_one_chunk() {
        assert_eq!(one_shot("data: a\n\ndata: b\n\n"), vec!["a", "b"]);
    }

    #[test]
    fn joins_multi_data_lines_with_newline() {
        assert_eq!(
            one_shot("data: line1\ndata: line2\n\n"),
            vec!["line1\nline2"]
        );
    }

    #[test]
    fn handles_crlf_line_endings() {
        assert_eq!(one_shot("data: a\r\n\r\ndata: b\r\n\r\n"), vec!["a", "b"]);
    }

    #[test]
    fn strips_at_most_one_space_after_colon() {
        assert_eq!(one_shot("data:  two spaces\n\n"), vec![" two spaces"]);
        assert_eq!(one_shot("data:no-space\n\n"), vec!["no-space"]);
    }

    #[test]
    fn ignores_comments_and_unknown_fields() {
        let input = ": keep-alive\nevent: delta\nid: 7\nretry: 1000\ndata: payload\n\n";
        assert_eq!(one_shot(input), vec!["payload"]);
    }

    #[test]
    fn bare_data_field_without_colon_is_an_empty_value() {
        assert_eq!(one_shot("data\n\n"), vec![""]);
    }

    #[test]
    fn splits_events_across_chunks() {
        assert_eq!(
            chunked(&["data: hel", "lo\n", "\ndata: ", "world\n\n"]),
            vec!["hello", "world"]
        );
    }

    #[test]
    fn survives_multibyte_char_split_across_chunks() {
        let text = "data: hé\n\n";
        let bytes = text.as_bytes();
        let split = bytes.len() - 2;
        assert_eq!(
            chunked(&[
                std::str::from_utf8(&bytes[..split]).unwrap(),
                std::str::from_utf8(&bytes[split..]).unwrap()
            ]),
            vec!["hé"]
        );
    }

    #[test]
    fn done_sentinel_is_just_a_payload() {
        assert_eq!(one_shot("data: [DONE]\n\n"), vec!["[DONE]"]);
    }

    #[test]
    fn finish_flushes_event_without_trailing_newline() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: tail").is_empty());
        assert_eq!(parser.finish(), vec!["tail"]);
    }

    #[test]
    fn finish_flushes_event_with_newline_but_no_blank_line() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: tail\n").is_empty());
        assert_eq!(parser.finish(), vec!["tail"]);
    }

    #[test]
    fn finish_on_empty_stream_is_quiet() {
        let mut parser = SseParser::new();
        assert!(parser.finish().is_empty());
    }

    #[test]
    fn strips_leading_bom() {
        let mut parser = SseParser::new();
        let out = parser.push("\u{FEFF}data: b\n\n".as_bytes());
        assert_eq!(out, vec!["b"]);
    }

    #[test]
    fn strips_a_bom_split_across_chunks() {
        // A 1-2 byte first chunk must not leak BOM bytes into the first line.
        let bytes = "\u{FEFF}data: b\n\n".as_bytes();
        for split in 1..BOM.len() {
            let mut parser = SseParser::new();
            let mut out = parser.push(&bytes[..split]);
            out.extend(parser.push(&bytes[split..]));
            out.extend(parser.finish());
            assert_eq!(out, vec!["b"], "split after {split} byte(s)");
        }
    }

    #[test]
    fn a_short_first_chunk_is_held_until_it_can_be_decided() {
        let mut parser = SseParser::new();
        // Two bytes without a newline cannot form an event; hold them.
        assert!(parser.push(b"da").is_empty());
        assert_eq!(parser.push(b"ta: x\n\n"), vec!["x"]);
    }

    #[test]
    fn ignores_garbage_between_events() {
        let input = "data: ok\n\nsome random noise\ndata: more\n\n";
        assert_eq!(one_shot(input), vec!["ok", "more"]);
    }
}
