//! Incremental Server-Sent Events line decoding.
//!
//! Bytes arrive in arbitrary chunks, so a line (or a multi-byte UTF-8
//! character) can be split across reads. The decoder buffers until a line
//! terminator and yields the payload of each `data:` line.

/// One decoded `data:` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    Data(String),
    Done,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    /// Feed bytes and return every complete event they finish.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut start = 0;
        let mut index = 0;
        while index < self.buffer.len() {
            let byte = self.buffer[index];
            if byte == b'\n' || byte == b'\r' {
                // A `\r` at the very end may be the first half of `\r\n`; wait for more.
                if byte == b'\r' && index + 1 == self.buffer.len() {
                    break;
                }
                if let Some(event) = parse_line(&self.buffer[start..index]) {
                    events.push(event);
                }
                if byte == b'\r' && self.buffer.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
                start = index + 1;
            }
            index += 1;
        }
        self.buffer.drain(..start);
        events
    }

    /// Flush a final line that arrived without a terminator.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let line = std::mem::take(&mut self.buffer);
        let line = line.strip_suffix(b"\r").unwrap_or(&line);
        parse_line(line)
    }
}

fn parse_line(line: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(line);
    let data = text.strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        Some(SseEvent::Done)
    } else {
        Some(SseEvent::Data(data.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_across_chunks_are_reassembled() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.feed(b"data: {\"a\"").is_empty());
        assert_eq!(
            decoder.feed(b": 1}\n\ndata: [DONE]\n"),
            vec![SseEvent::Data("{\"a\": 1}".into()), SseEvent::Done]
        );
    }

    #[test]
    fn crlf_and_cr_terminators_are_accepted() {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            decoder.feed(b"data: one\r"),
            Vec::<SseEvent>::new(),
            "a trailing CR must wait for a possible LF"
        );
        assert_eq!(
            decoder.feed(b"\ndata: two\rdata: three\r\n"),
            vec![
                SseEvent::Data("one".into()),
                SseEvent::Data("two".into()),
                SseEvent::Data("three".into()),
            ]
        );
    }

    #[test]
    fn non_data_lines_are_ignored() {
        let mut decoder = SseDecoder::default();
        assert_eq!(
            decoder.feed(b": keep-alive\nevent: message\nid: 7\n\ndata:x\n"),
            vec![SseEvent::Data("x".into())]
        );
    }

    #[test]
    fn multibyte_characters_split_across_chunks_survive() {
        let mut decoder = SseDecoder::default();
        let bytes = "data: héllo\n".as_bytes();
        let (first, second) = bytes.split_at(8);
        assert!(decoder.feed(first).is_empty());
        assert_eq!(decoder.feed(second), vec![SseEvent::Data("héllo".into())]);
    }

    #[test]
    fn an_unterminated_final_line_is_flushed() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.feed(b"data: {\"tail\"").is_empty());
        assert_eq!(decoder.finish(), Some(SseEvent::Data("{\"tail\"".into())));
        assert_eq!(decoder.finish(), None);
    }
}
