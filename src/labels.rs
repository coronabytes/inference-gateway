//! Request labels: up to five `key=value` pairs a client attaches to a request, such as
//! `x-gateway-labels: tenant=0192…,feature=core`. They are kept with the request in the request
//! log, where the log and the metrics can be filtered by them.

use std::collections::BTreeMap;

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, request::Parts},
    response::Response,
};
use serde_json::Value;

use crate::{error::GatewayError, state::AppState};

pub const HEADER: &str = "x-gateway-labels";
pub const MAX_LABELS: usize = 5;
/// Keys and values are shorter than this many characters.
pub const MAX_LEN: usize = 128;

/// A request's labels, ordered by key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Labels(BTreeMap<String, String>);

impl Labels {
    /// Parses `key=value` pairs separated by commas. Keys are letters, digits, `_`, `-` and `.`;
    /// values are printable ASCII without commas, and may contain `=`. Whitespace around pairs,
    /// keys and values is ignored, and so are empty pairs (a trailing comma).
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut labels = BTreeMap::new();
        for pair in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let Some((key, value)) = pair.split_once('=') else {
                return Err(format!("label '{pair}' needs a value, as key=value"));
            };
            let (key, value) = (key.trim(), value.trim());
            if key.is_empty() || value.is_empty() {
                return Err(format!("label '{pair}' needs both a key and a value"));
            }
            if key.len() >= MAX_LEN || value.len() >= MAX_LEN {
                return Err(format!("label keys and values must be shorter than {MAX_LEN} characters ('{key}')"));
            }
            if !key.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) {
                return Err(format!("label key '{key}' may only contain letters, digits, '_', '-' and '.'"));
            }
            if !value.bytes().all(|b| b == b' ' || b.is_ascii_graphic()) {
                return Err(format!("label '{key}' has a value with characters other than printable ASCII"));
            }
            if labels.insert(key.to_string(), value.to_string()).is_some() {
                return Err(format!("label '{key}' is given more than once"));
            }
        }
        if labels.len() > MAX_LABELS {
            return Err(format!("a request may have at most {MAX_LABELS} labels, not {}", labels.len()));
        }
        Ok(Self(labels))
    }

    /// The labels in the request's `x-gateway-labels` headers (several are read as one).
    pub fn from_headers(headers: &HeaderMap) -> Result<Self, GatewayError> {
        let mut all = String::new();
        for v in headers.get_all(HEADER) {
            let v = v.to_str().map_err(|_| GatewayError::invalid(format!("{HEADER} must be printable ASCII")))?;
            all.push_str(v);
            all.push(',');
        }
        Self::parse(&all).map_err(|e| GatewayError::invalid(format!("{HEADER}: {e}")))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// A JSON object for the request log; `None` without labels.
    pub fn to_json(&self) -> Option<Value> {
        (!self.is_empty()).then(|| Value::Object(self.0.iter().map(|(k, v)| (k.clone(), v.clone().into())).collect()))
    }
}

/// The request's labels, read from its headers before the body like `ClientKey`, so a request
/// with invalid labels is refused before its body is read.
pub struct ClientLabels(pub Labels);

impl FromRequestParts<AppState> for ClientLabels {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _: &AppState) -> Result<Self, Response> {
        Labels::from_headers(&parts.headers)
            .map(ClientLabels)
            .map_err(|e| e.into_response_for(crate::auth::dialect_of(parts)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairs_in_key_order() {
        let l = Labels::parse(" tenant = 0192f0c1-7c3a-7d2e-9b1a-3c4d5e6f7a8b , feature=core,").unwrap();
        assert_eq!(l.iter().collect::<Vec<_>>(), [("feature", "core"), ("tenant", "0192f0c1-7c3a-7d2e-9b1a-3c4d5e6f7a8b")]);
        assert_eq!(l.to_json().unwrap(), serde_json::json!({ "feature": "core", "tenant": "0192f0c1-7c3a-7d2e-9b1a-3c4d5e6f7a8b" }));
        assert_eq!(Labels::parse("").unwrap().to_json(), None);
        assert_eq!(Labels::parse("q=a=b").unwrap().iter().next(), Some(("q", "a=b")));
    }

    #[test]
    fn at_most_five() {
        assert!(Labels::parse("a=1,b=2,c=3,d=4,e=5").is_ok());
        assert!(Labels::parse("a=1,b=2,c=3,d=4,e=5,f=6").unwrap_err().contains("at most 5"));
    }

    #[test]
    fn keys_and_values_are_shorter_than_128() {
        let (k127, k128) = ("k".repeat(127), "k".repeat(128));
        assert!(Labels::parse(&format!("{k127}={k127}")).is_ok());
        assert!(Labels::parse(&format!("{k128}=v")).is_err());
        assert!(Labels::parse(&format!("k={k128}")).is_err());
    }

    #[test]
    fn refuses_malformed_labels() {
        for bad in ["tenant", "=x", "tenant=", "a b=1", "a=1,a=2", "a=tab\there"] {
            assert!(Labels::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn several_headers_are_read_as_one() {
        let mut h = HeaderMap::new();
        h.append(HEADER, "tenant=t1".parse().unwrap());
        h.append(HEADER, "feature=core".parse().unwrap());
        assert_eq!(Labels::from_headers(&h).unwrap().iter().count(), 2);
        h.append(HEADER, "tenant=t2".parse().unwrap());
        assert!(Labels::from_headers(&h).is_err());
        assert!(Labels::from_headers(&HeaderMap::new()).unwrap().is_empty());
    }
}
