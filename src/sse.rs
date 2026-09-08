use crate::{Result, fail};

pub const MAX_FRAME: usize = 2 * 1024 * 1024;

/// Incremental SSE decoder. UTF-8 is decoded only after complete data lines.
#[derive(Default)]
pub struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    after_cr: bool,
}
impl Decoder {
    pub fn byte(&mut self, b: u8) -> Result<Option<Vec<u8>>> {
        if self.after_cr {
            self.after_cr = false;
            if b == b'\n' {
                return Ok(None);
            }
        }
        if b != b'\n' && b != b'\r' {
            if self.line.len() + self.data.len() >= MAX_FRAME {
                return fail("sse_frame_limit");
            }
            self.line.push(b);
            return Ok(None);
        }
        self.after_cr = b == b'\r';
        if self.line.is_empty() {
            if self.data.is_empty() {
                return Ok(None);
            }
            self.data.pop(); // Remove the final SSE data-line newline.
            return Ok(Some(std::mem::take(&mut self.data)));
        }
        if let Some(value) = self.line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            self.data.extend_from_slice(value);
            self.data.push(b'\n');
        }
        self.line.clear();
        Ok(None)
    }
    pub fn is_empty(&self) -> bool {
        self.line.is_empty() && self.data.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_unicode_crlf_comments_and_multiline_data() {
        let bytes = ": keepalive\r\nevent: text\r\ndata: héllo\r\ndata: world\r\n\r\n".as_bytes();
        let mut decoder = Decoder::default();
        let mut frames = Vec::new();
        for b in bytes {
            if let Some(frame) = decoder.byte(*b).unwrap() {
                frames.push(frame);
            }
        }
        assert_eq!(frames, vec!["héllo\nworld".as_bytes()]);
        assert!(decoder.is_empty());
    }
    #[test]
    fn oversized_unterminated_frame_is_rejected() {
        let mut decoder = Decoder::default();
        for _ in 0..MAX_FRAME {
            decoder.byte(b'x').unwrap();
        }
        assert!(decoder.byte(b'x').is_err());
    }
}
