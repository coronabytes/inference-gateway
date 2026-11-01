use std::{
    borrow::Borrow,
    collections::HashMap,
    hash::Hash,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

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

/// A map in memory whose entries expire: `ttl` after they are stored (or, made with `idle`, after
/// they were last read), or at the time given to `insert_until`. It holds at most `max` entries:
/// storing past that drops the expired ones, then those expiring soonest. Expired entries are
/// never returned. A lock poisoned by a panic elsewhere is taken over, since every entry is whole.
pub struct TtlCache<K, V> {
    entries: Mutex<HashMap<K, (V, Instant)>>,
    ttl: Duration,
    idle: bool,
    max: usize,
}

impl<K: Eq + Hash + Clone, V: Clone> TtlCache<K, V> {
    pub fn new(ttl: Duration, max: usize) -> Self {
        Self { entries: Mutex::new(HashMap::new()), ttl, idle: false, max }
    }

    /// Entries expire `ttl` after they were last read or stored.
    pub fn idle(ttl: Duration, max: usize) -> Self {
        Self { idle: true, ..Self::new(ttl, max) }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<K, (V, Instant)>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn get<Q: Eq + Hash + ?Sized>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let mut entries = self.lock();
        let now = Instant::now();
        match entries.get_mut(key) {
            Some((v, expires)) if *expires > now => {
                if self.idle {
                    *expires = now + self.ttl;
                }
                Some(v.clone())
            }
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn insert(&self, key: K, value: V) {
        self.insert_until(key, value, Instant::now() + self.ttl);
    }

    /// Stores `value` until `expires`, such as a token's own expiry.
    pub fn insert_until(&self, key: K, value: V, expires: Instant) {
        let mut entries = self.lock();
        if entries.len() >= self.max && !entries.contains_key(&key) {
            let now = Instant::now();
            entries.retain(|_, (_, e)| *e > now);
            while entries.len() >= self.max {
                let Some(soonest) = entries.iter().min_by_key(|(_, (_, e))| *e).map(|(k, _)| k.clone()) else { break };
                entries.remove(&soonest);
            }
        }
        entries.insert(key, (value, expires));
    }

    pub fn remove<Q: Eq + Hash + ?Sized>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        self.lock().remove(key).map(|(v, _)| v)
    }

    /// Keeps only the entries `keep` accepts.
    pub fn retain(&self, mut keep: impl FnMut(&K, &V) -> bool) {
        self.lock().retain(|k, (v, _)| keep(k, v));
    }

    pub fn clear(&self) {
        self.lock().clear();
    }
}

/// An error and its causes, `a: b: c`. A cause whose text the message already includes (as some
/// libraries' errors do) is not repeated.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let text = c.to_string();
        if !s.contains(&text) {
            s.push_str(": ");
            s.push_str(&text);
        }
        cur = c.source();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_chains_skip_repeated_causes() {
        #[derive(Debug)]
        struct E(String, Option<Box<E>>);
        impl std::fmt::Display for E {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for E {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_deref().map(|e| e as _)
            }
        }
        let inner = E("HTTP 400".into(), None);
        let middle = E("refresh failed: HTTP 400".into(), Some(Box::new(inner)));
        let outer = E("token failed".into(), Some(Box::new(middle)));
        assert_eq!(error_chain(&outer), "token failed: refresh failed: HTTP 400");
    }

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

    #[test]
    fn ttl_cache_entries_expire_and_make_room() {
        let c = TtlCache::new(Duration::from_secs(60), 2);
        c.insert("a", 1);
        c.insert_until("gone", 0, Instant::now() - Duration::from_secs(1));
        assert_eq!(c.get("a"), Some(1));
        assert_eq!(c.get("gone"), None, "expired");
        // Full: the expired entry goes first, then the one expiring soonest.
        c.insert_until("soon", 2, Instant::now() + Duration::from_secs(5));
        c.insert("b", 3);
        assert_eq!((c.get("a"), c.get("soon"), c.get("b")), (Some(1), None, Some(3)));
        c.retain(|k, _| *k != "a");
        assert_eq!(c.get("a"), None);
        assert_eq!(c.remove("b"), Some(3));
    }

    #[test]
    fn an_idle_ttl_cache_keeps_what_is_read() {
        let c = TtlCache::idle(Duration::from_millis(50), 10);
        c.insert(1, "x");
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(c.get(&1), Some("x"));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(c.get(&1), Some("x"), "reading kept it");
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(c.get(&1), None);
    }
}
