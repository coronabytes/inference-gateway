//! `multipart/form-data` bodies held in memory: read from clients (audio transcriptions) and
//! written to upstreams.

use bytes::{BufMut, Bytes, BytesMut};

/// One part of a form: a field's text, or a file with its name and type.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Bytes,
}

impl Part {
    pub fn text(name: &str, value: impl Into<String>) -> Self {
        Self { name: name.into(), filename: None, content_type: None, data: Bytes::from(value.into()) }
    }

    /// The field's value as text, if it is UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.data).ok()
    }
}

/// The boundary of a `multipart/form-data` content type.
pub fn boundary(content_type: &str) -> Option<String> {
    let (kind, params) = content_type.split_once(';')?;
    if !kind.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"').to_string()).filter(|b| !b.is_empty())
    })
}

/// The parts of a body sent with `content_type`.
pub fn parse(content_type: &str, body: &Bytes) -> Result<Vec<Part>, String> {
    let boundary = boundary(content_type).ok_or("the request must be multipart/form-data with a boundary")?;
    let delimiter = format!("--{boundary}");
    let d = delimiter.as_bytes();
    let mut pos = find(body, d, 0).ok_or("the multipart body has no parts")? + d.len();
    let mut parts = Vec::new();
    loop {
        if body[pos..].starts_with(b"--") {
            return Ok(parts);
        }
        pos = skip_line_break(body, pos).ok_or("malformed multipart body")?;
        let head_end = find(body, b"\r\n\r\n", pos).ok_or("malformed multipart part headers")?;
        let head = std::str::from_utf8(&body[pos..head_end]).map_err(|_| "multipart part headers are not UTF-8")?;
        let start = head_end + 4;
        let next = find(body, d, start).ok_or("the multipart body is not terminated")?;
        // The data ends before the line break that precedes the next delimiter.
        let end = if body[..next].ends_with(b"\r\n") { next - 2 } else { next };
        parts.push(part(head, body.slice(start..end.max(start)))?);
        pos = next + d.len();
    }
}

fn part(head: &str, data: Bytes) -> Result<Part, String> {
    let (mut name, mut filename, mut content_type) = (None, None, None);
    for line in head.split("\r\n") {
        let Some((key, value)) = line.split_once(':') else { continue };
        if key.trim().eq_ignore_ascii_case("content-disposition") {
            for param in value.split(';').skip(1) {
                let Some((k, v)) = param.split_once('=') else { continue };
                let v = v.trim().trim_matches('"').to_string();
                match k.trim().to_ascii_lowercase().as_str() {
                    "name" => name = Some(v),
                    "filename" => filename = Some(v),
                    _ => {}
                }
            }
        } else if key.trim().eq_ignore_ascii_case("content-type") {
            content_type = Some(value.trim().to_string());
        }
    }
    let name = name.ok_or("a multipart part has no name")?;
    Ok(Part { name, filename, content_type, data })
}

fn skip_line_break(body: &[u8], pos: usize) -> Option<usize> {
    if body[pos..].starts_with(b"\r\n") { Some(pos + 2) } else { None }
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// A form of `parts`, with its content type.
pub fn build(parts: &[Part]) -> (String, Vec<u8>) {
    let boundary = format!("gateway-{}", uuid::Uuid::new_v4().simple());
    let mut out = BytesMut::new();
    for p in parts {
        out.put_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"", escape(&p.name)).as_bytes());
        if let Some(f) = &p.filename {
            out.put_slice(format!("; filename=\"{}\"", escape(f)).as_bytes());
        }
        out.put_slice(b"\r\n");
        if let Some(t) = &p.content_type {
            out.put_slice(format!("Content-Type: {t}\r\n").as_bytes());
        }
        out.put_slice(b"\r\n");
        out.put_slice(&p.data);
        out.put_slice(b"\r\n");
    }
    out.put_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), out.to_vec())
}

/// Names go in quotes; quotes and line breaks in them would end the header.
fn escape(s: &str) -> String {
    s.replace(['"', '\r', '\n'], "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let parts = vec![
            Part::text("model", "whisper"),
            Part {
                name: "file".into(),
                filename: Some("a.wav".into()),
                content_type: Some("audio/wav".into()),
                data: Bytes::from_static(b"RIFF\r\n--x\0"),
            },
            Part::text("language", ""),
        ];
        let (ct, body) = build(&parts);
        assert_eq!(parse(&ct, &Bytes::from(body)).unwrap(), parts);
    }

    #[test]
    fn reads_what_clients_send() {
        let body = Bytes::from_static(
            b"--XyZ\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"s.mp3\"\r\nContent-Type: audio/mpeg\r\n\r\nID3\r\n--XyZ--\r\n",
        );
        let parts = parse("multipart/form-data; boundary=\"XyZ\"", &body).unwrap();
        assert_eq!(parts[0].as_str(), Some("m"));
        assert_eq!(
            (parts[1].filename.as_deref(), parts[1].content_type.as_deref(), &parts[1].data[..]),
            (Some("s.mp3"), Some("audio/mpeg"), &b"ID3"[..])
        );
        assert!(parse("application/json", &body).is_err());
        assert!(
            parse(
                "multipart/form-data; boundary=XyZ",
                &Bytes::from_static(b"--XyZ\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\nno end")
            )
            .is_err()
        );
        assert_eq!(boundary("Multipart/Form-Data; charset=utf-8; boundary=abc").as_deref(), Some("abc"));
    }
}
