//! Incremental server-sent events parser for upstream streams.

use bytes::BytesMut;

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Server-sent events parser that tolerates arbitrary chunk boundaries.
#[derive(Default)]
pub struct SseParser {
    buf: BytesMut,
    /// How much of `buf` holds no line break: a long line arriving in many chunks is scanned once.
    scanned: usize,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some(at) = self.buf[self.scanned..].iter().position(|&b| b == b'\n') else {
                self.scanned = self.buf.len();
                break;
            };
            let pos = self.scanned + at;
            self.scanned = 0;
            let mut line = self.buf.split_to(pos + 1);
            line.truncate(pos);
            if line.last() == Some(&b'\r') {
                line.truncate(pos - 1);
            }
            let line = String::from_utf8_lossy(&line);
            self.line(&line, &mut out);
        }
        out
    }

    /// Flush a trailing event that was not terminated by a blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            self.scanned = 0;
            for line in rest.lines() {
                self.line(line, &mut out);
            }
        }
        self.dispatch(&mut out);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            self.event = None;
            return;
        }
        out.push(SseEvent { event: self.event.take(), data: self.data.join("\n") });
        self.data.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_split_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.feed(b"event: a\nda").is_empty());
        let ev = p.feed(b"ta: {\"x\":1}\r\n\r\ndata: [DONE]\n\n");
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].event.as_deref(), Some("a"));
        assert_eq!(ev[0].data, "{\"x\":1}");
        assert_eq!(ev[1].data, "[DONE]");
    }

    #[test]
    fn a_long_line_in_many_chunks() {
        let mut p = SseParser::default();
        assert!(p.feed(b"data: ").is_empty());
        for _ in 0..1000 {
            assert!(p.feed(b"xxxxxxxx").is_empty());
        }
        let ev = p.feed(b"\r\n\ndata: y\n\nda");
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].data.len(), 8000);
        assert_eq!(ev[1].data, "y");
        // The rest of the buffer still makes a line with what comes next.
        let ev = p.feed(b"ta: z\n\n");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "z");
    }
}
