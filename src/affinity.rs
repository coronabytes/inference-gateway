//! Sticky routing: the turns of a conversation go to the deployment (and through an auto router,
//! the route and model) that served its earlier turns, where the provider's prompt cache for it
//! is. Spread over several deployments, every turn would miss that cache and pay for the whole
//! history again (and on Anthropic and newer OpenAI models, pay to write it once more).
//!
//! A conversation is known by how it starts, which every later turn repeats: its system prompt,
//! tools and messages up to the first user message. A client's `prompt_cache_key` replaces that,
//! and a Responses conversation the gateway stores (`previous_response_id`) is known by its root.
//! Only a keyed hash of it is kept, in Valkey, for an hour after the conversation's last request.

use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{api::Inbound, state::AppState};

/// How long a conversation stays with its deployment after its last request. Longer than the
/// providers' default prompt cache lifetimes (5 to 30 minutes), so it outlasts the cache.
pub const TTL: Duration = Duration::from_secs(3600);

/// A conversation's identity for sticky routing.
#[derive(Clone)]
pub struct Affinity([u8; 32]);

impl Affinity {
    /// The conversation `raw` (a request in `inbound`'s format) belongs to; `None` when sticky
    /// routing is off or the request has nothing that identifies a conversation.
    pub fn of(state: &AppState, raw: &Value, inbound: Inbound) -> Option<Self> {
        if !state.settings.get().sticky_routing {
            return None;
        }
        let start = start(raw, inbound)?;
        let mut h = Sha256::new();
        h.update(state.cfg.affinity_salt);
        h.update(start);
        Some(Self(h.finalize().into()))
    }

    /// The Valkey key for this conversation on a model or router: `kind` and `name` are hashed
    /// in, since names may contain anything (also a cluster hash tag).
    fn key(&self, kind: &str, name: &str) -> String {
        let mut h = Sha256::new();
        h.update(self.0);
        h.update(kind.as_bytes());
        h.update([0]);
        h.update(name.as_bytes());
        format!("gw:aff:{}", hex::encode(h.finalize()))
    }

    /// The deployment of `model` that served this conversation last.
    pub async fn deployment(&self, state: &AppState, model: &str) -> Option<Uuid> {
        state.cache.affinity(&self.key("deployment", model), TTL).await?.parse().ok()
    }

    pub async fn set_deployment(&self, state: &AppState, model: &str, id: Uuid) {
        state.cache.set_affinity(&self.key("deployment", model), &id.to_string(), TTL).await;
    }

    /// The route and model (with its reasoning effort) auto router `router` chose for this
    /// conversation.
    pub async fn route(&self, state: &AppState, router: &str) -> Option<StickyRoute> {
        serde_json::from_str(&state.cache.affinity(&self.key("router", router), TTL).await?).ok()
    }

    pub async fn set_route(&self, state: &AppState, router: &str, route: &StickyRoute) {
        if let Ok(v) = serde_json::to_string(route) {
            state.cache.set_affinity(&self.key("router", router), &v, TTL).await;
        }
    }
}

/// Where an auto router sent a conversation.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StickyRoute {
    pub route: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

/// What identifies the conversation of a request: its `prompt_cache_key`, the root of a stored
/// Responses conversation, or how it starts.
fn start(raw: &Value, inbound: Inbound) -> Option<Vec<u8>> {
    if let Some(k) = raw.get("prompt_cache_key").and_then(Value::as_str).filter(|k| !k.is_empty()) {
        return Some(format!("prompt_cache_key\n{k}").into_bytes());
    }
    if inbound == Inbound::Responses
        && let Some(previous) = raw.get("previous_response_id").and_then(Value::as_str)
    {
        // Each turn sends only what is new: the conversation is known by its root. Responses the
        // provider stores are already bound to their deployment.
        return crate::conversations::root_of(previous).map(|root| format!("conversation\n{root}").into_bytes());
    }
    let (system, conversation) = match inbound {
        Inbound::Messages => (raw.get("system"), raw.get("messages")?),
        Inbound::Chat => (None, raw.get("messages")?),
        Inbound::Responses => (raw.get("instructions"), raw.get("input")?),
    };
    let opening = match conversation {
        Value::String(s) if !s.is_empty() => json!([{ "role": "user", "content": s }]),
        Value::Array(items) => {
            let first_user = items.iter().position(|m| m.get("role").and_then(Value::as_str) == Some("user"))?;
            Value::Array(items[..=first_user].to_vec())
        }
        _ => return None,
    };
    serde_json::to_vec(&json!([system, raw.get("tools"), opening])).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &Value, b: &Value, inbound: Inbound) -> bool {
        let state = crate::state::test_state();
        let (a, b) = (Affinity::of(&state, a, inbound).unwrap(), Affinity::of(&state, b, inbound).unwrap());
        a.0 == b.0
    }

    #[tokio::test]
    async fn later_turns_belong_to_the_first() {
        let first = json!({ "model": "m", "system": "Be brief.", "messages": [{ "role": "user", "content": "Fix my SQL" }] });
        let later = json!({ "model": "other", "system": "Be brief.", "stream": true, "messages": [
            { "role": "user", "content": "Fix my SQL" },
            { "role": "assistant", "content": "Which query?" },
            { "role": "user", "content": "SELECT 1" },
        ] });
        assert!(same(&first, &later, Inbound::Messages), "the model and later messages do not count");
        let other_question = json!({ "system": "Be brief.", "messages": [{ "role": "user", "content": "Explain joins" }] });
        assert!(!same(&first, &other_question, Inbound::Messages));
        let other_system = json!({ "system": "Be verbose.", "messages": [{ "role": "user", "content": "Fix my SQL" }] });
        assert!(!same(&first, &other_system, Inbound::Messages));
    }

    #[tokio::test]
    async fn chat_counts_system_messages_before_the_first_user_message() {
        let a = json!({ "messages": [{ "role": "system", "content": "A" }, { "role": "user", "content": "hi" }] });
        let b = json!({ "messages": [{ "role": "system", "content": "B" }, { "role": "user", "content": "hi" }] });
        assert!(!same(&a, &b, Inbound::Chat));
        let tools = json!({ "tools": [{ "type": "function" }], "messages": [{ "role": "system", "content": "A" }, { "role": "user", "content": "hi" }] });
        assert!(!same(&a, &tools, Inbound::Chat));
    }

    #[tokio::test]
    async fn explicit_identities_win() {
        let a = json!({ "prompt_cache_key": "tenant-1", "messages": [{ "role": "user", "content": "x" }] });
        let b = json!({ "prompt_cache_key": "tenant-1", "messages": [{ "role": "user", "content": "y" }] });
        assert!(same(&a, &b, Inbound::Chat));
        let turn2 = json!({ "previous_response_id": "resp_00112233445566778899aabbccddeeff", "input": "and then?" });
        let turn3 = json!({ "previous_response_id": "resp_0011223344556677ffeeddccbbaa9988", "input": "go on" });
        assert!(same(&turn2, &turn3, Inbound::Responses), "same conversation root");
    }

    #[tokio::test]
    async fn nothing_to_identify() {
        let state = crate::state::test_state();
        let no_user = json!({ "messages": [{ "role": "system", "content": "A" }] });
        assert!(Affinity::of(&state, &no_user, Inbound::Chat).is_none());
        // A provider-stored response: its deployment is pinned elsewhere.
        let upstream = json!({ "previous_response_id": "resp_abc", "input": "x" });
        assert!(Affinity::of(&state, &upstream, Inbound::Responses).is_none());
    }
}
