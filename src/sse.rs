use crate::{Result, fail};

/// One frame is bounded as the whole response is: a reply's items come whole
/// in one, and again in the terminal frame.
pub const MAX_FRAME: usize = crate::provider::MAX_RESPONSE;

/// Incremental SSE decoder. UTF-8 is decoded only after complete data lines.
#[derive(Default)]
pub struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
    delivered: bool,
}
impl Decoder {
    /// Consume through the next frame, leaving later bytes for the next call.
    /// The returned frame borrows the decoder's reusable buffer. No list of
    /// frames, or additional copy of the input chunk, is held while publishing.
    pub fn next_frame(&mut self, input: &mut &[u8]) -> Result<Option<&[u8]>> {
        if self.delivered {
            self.data.clear();
            self.delivered = false;
        }
        // A single non-delimiter byte needs only the bounded append.
        if input.len() == 1 && input[0] != b'\r' && input[0] != b'\n' {
            self.after_cr = false;
            if self.line.len() + self.data.len() >= MAX_FRAME {
                return fail("sse_frame_limit");
            }
            self.line.push(input[0]);
            *input = &[];
            return Ok(None);
        }
        while !input.is_empty() {
            if self.after_cr {
                self.after_cr = false;
                if input[0] == b'\n' {
                    *input = &input[1..];
                    continue;
                }
            }
            let end = memchr::memchr2(b'\r', b'\n', input).unwrap_or(input.len());
            if end > MAX_FRAME.saturating_sub(self.line.len() + self.data.len()) {
                return fail("sse_frame_limit");
            }
            self.line.extend_from_slice(&input[..end]);
            *input = &input[end..];
            let Some((&ending, rest)) = input.split_first() else {
                break;
            };
            self.after_cr = ending == b'\r';
            *input = rest;
            if self.line.is_empty() {
                if !self.data.is_empty() {
                    self.data.pop(); // Remove the final SSE data-line newline.
                    self.delivered = true;
                    return Ok(Some(&self.data));
                }
            } else {
                if let Some(value) = self.line.strip_prefix(b"data:") {
                    let value = value.strip_prefix(b" ").unwrap_or(value);
                    self.data.reserve(value.len() + 1);
                    self.data.extend_from_slice(value);
                    self.data.push(b'\n');
                }
                self.line.clear();
            }
        }
        Ok(None)
    }
    pub fn is_empty(&self) -> bool {
        self.line.is_empty() && (self.delivered || self.data.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(wire: &[u8], fragment: usize) -> (Vec<Vec<u8>>, bool) {
        let mut decoder = Decoder::default();
        let mut frames = Vec::new();
        for mut bytes in wire.chunks(fragment) {
            while let Some(frame) = decoder.next_frame(&mut bytes).unwrap() {
                frames.push(frame.to_vec());
            }
            assert!(bytes.is_empty());
        }
        (frames, decoder.is_empty())
    }

    #[test]
    fn framing_survives_every_fragment_size_and_line_ending() {
        let wire = ": keepalive\r\nevent: text\r\ndata: héllo\r\ndata: world\r\n\r\n\
                    data:\n\ndata:  space\n\nid: 2\ndata:tail\r\r"
            .as_bytes();
        let expected = vec!["héllo\nworld".as_bytes(), b"", b" space", b"tail"];
        for fragment in 1..=wire.len() {
            let (frames, empty) = decode(wire, fragment);
            assert_eq!(frames, expected, "fragment size {fragment}");
            assert!(empty);
        }
    }

    #[test]
    fn a_call_returns_one_frame_without_reading_ahead() {
        let mut bytes = b"data:a\n\ndata:b\n\n".as_slice();
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder.next_frame(&mut bytes).unwrap(),
            Some(b"a".as_slice())
        );
        assert_eq!(bytes, b"data:b\n\n");
        assert!(decoder.is_empty());
        assert_eq!(
            decoder.next_frame(&mut bytes).unwrap(),
            Some(b"b".as_slice())
        );
        assert!(bytes.is_empty());
        assert!(decoder.is_empty());
        assert_eq!(decoder.next_frame(&mut bytes).unwrap(), None);
    }

    #[test]
    fn comments_are_empty_but_partial_lines_and_data_are_not() {
        for (wire, empty) in [
            (b":ping\n".as_slice(), true),
            (b":ping", false),
            (b"data:value", false),
            (b"data:value\n", false),
            (b"data:value\r\n", false),
        ] {
            let (frames, actual) = decode(wire, 1);
            assert!(frames.is_empty());
            assert_eq!(actual, empty);
        }
    }

    #[test]
    fn limits_count_pending_line_and_multiline_data_together() {
        let mut wire = b"data:".to_vec();
        wire.resize(MAX_FRAME, b'x');
        wire.extend_from_slice(b"\n\n");
        let mut decoder = Decoder::default();
        let mut bytes = wire.as_slice();
        assert_eq!(
            decoder.next_frame(&mut bytes).unwrap().unwrap().len(),
            MAX_FRAME - 5
        );
        assert!(decoder.is_empty());

        wire.insert(MAX_FRAME, b'x');
        let mut bytes = wire.as_slice();
        assert_eq!(
            Decoder::default().next_frame(&mut bytes).unwrap_err().code,
            "sse_frame_limit"
        );

        let mut decoder = Decoder::default();
        for mut bytes in wire[..MAX_FRAME].chunks(1) {
            assert!(decoder.next_frame(&mut bytes).unwrap().is_none());
        }
        let mut bytes = b"x".as_slice();
        assert_eq!(
            decoder.next_frame(&mut bytes).unwrap_err().code,
            "sse_frame_limit"
        );

        let mut wire = b"data:".to_vec();
        wire.resize(MAX_FRAME / 2 + 5, b'x');
        wire.extend_from_slice(b"\ndata:");
        wire.resize(MAX_FRAME + 11, b'x');
        let mut decoder = Decoder::default();
        for mut bytes in wire.chunks(4096) {
            if let Err(error) = decoder.next_frame(&mut bytes) {
                assert_eq!(error.code, "sse_frame_limit");
                return;
            }
        }
        panic!("pending multiline data must remain bounded");
    }
}
