use sha2::{Digest, Sha256};

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    buf
}

pub fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    hex::encode(buf)
}

/// Random identifier with an OpenAI/Anthropic style prefix, e.g. `msg_3f9a...`.
pub fn prefixed_id(prefix: &str) -> String {
    format!("{prefix}_{}", random_hex(12))
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Whether two URLs share scheme, host and port: a secret sent to one goes to the same place.
pub fn same_origin(a: &str, b: &str) -> bool {
    match (reqwest::Url::parse(a.trim()), reqwest::Url::parse(b.trim())) {
        (Ok(a), Ok(b)) => a.origin().is_tuple() && a.origin() == b.origin(),
        _ => false,
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Truncate a string on a char boundary for logs and error messages.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_origin_compares_scheme_host_and_port() {
        assert!(same_origin("https://api.example.com/v1", "https://api.example.com/other/"));
        assert!(same_origin("https://api.example.com", "https://api.example.com:443/v1"));
        assert!(!same_origin("https://api.example.com", "https://evil.example.com"));
        assert!(!same_origin("https://api.example.com", "http://api.example.com"));
        assert!(!same_origin("https://api.example.com", "https://api.example.com:8443"));
        assert!(!same_origin("not a url", "not a url"));
        assert!(!same_origin("data:text/plain,x", "data:text/plain,x"));
    }
}
