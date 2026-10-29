//! Conversation state behind the OpenAI Responses API's `previous_response_id`.
//!
//! Two cases, both kept in Valkey so every replica sees them, until they go unused for
//! the `response_ttl_minutes` setting (each read and each new turn extends their lifetime):
//!
//! - **Gateway-stored** (requests translated to Anthropic, Bedrock or Chat Completions): each
//!   turn's input and output are stored, encrypted, in one hash per conversation
//!   (`gw:conv:{root}`, field = response id). Our response ids embed the conversation root
//!   (`resp_<16 hex root><16 hex>`), so a continuation is a single read and branching from any
//!   earlier response works. The transcript is replayed to whichever deployment serves the
//!   continuation; the one that produced the previous turn is preferred.
//! - **Upstream-stored** (native Responses passthrough to OpenAI or Azure): the provider keeps the
//!   conversation, so we only remember which deployment holds it (`gw:pin:<id>`). Continuations
//!   must go back there: another Azure resource or account would not know the id.
//!
//! Both are bound to the virtual key that created them; other keys get "not found".

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    error::GatewayError,
    ir::{Message, Part, TextBlock},
    model::ProviderKind,
    state::AppState,
};

/// Guard against corrupt or cyclic parent chains.
const MAX_TURNS: usize = 10_000;

/// One stored turn: what the client sent and what the model answered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub parent: Option<String>,
    pub key_id: Uuid,
    pub deployment_id: Uuid,
    pub provider: ProviderKind,
    /// System/developer messages from `input`. Top-level `instructions` are not stored: like
    /// OpenAI, they apply to one request only.
    #[serde(default)]
    pub system: Vec<TextBlock>,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pin {
    pub key_id: Uuid,
    pub deployment_id: Uuid,
}

/// The turns leading up to (and including) the previous response, oldest first.
pub struct History {
    pub turns: Vec<Turn>,
}

impl History {
    /// Deployment that produced the previous response.
    pub fn last_deployment(&self) -> Option<Uuid> {
        self.turns.last().map(|t| t.deployment_id)
    }

    /// Messages to replay on a deployment of `provider`. Reasoning signatures only validate on
    /// the provider that issued them, so turns from other providers lose their reasoning parts.
    pub fn messages_for(&self, provider: ProviderKind) -> Vec<Message> {
        let mut out = Vec::new();
        for t in &self.turns {
            for m in &t.messages {
                let content: Vec<Part> = if t.provider == provider {
                    m.content.clone()
                } else {
                    m.content.iter().filter(|p| !matches!(p, Part::Reasoning { .. })).cloned().collect()
                };
                if !content.is_empty() {
                    out.push(Message { role: m.role, content });
                }
            }
        }
        out
    }

    pub fn system(&self) -> Vec<TextBlock> {
        self.turns.iter().flat_map(|t| t.system.iter().cloned()).collect()
    }
}

pub enum Previous {
    Gateway(History),
    Upstream(Pin),
}

/// The conversation root embedded in one of our response ids.
pub(crate) fn root_of(response_id: &str) -> Option<&str> {
    let hex = response_id.strip_prefix("resp_")?;
    (hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| &hex[..16])
}

/// A new response id, continuing the conversation of `previous` if it is one of ours.
/// Returns `(root, response_id)`.
pub fn new_response_id(previous: Option<&str>) -> (String, String) {
    let root = previous.and_then(root_of).map(str::to_string).unwrap_or_else(|| crate::util::random_hex(8));
    let id = format!("resp_{root}{}", crate::util::random_hex(8));
    (root, id)
}

fn not_found(id: &str) -> GatewayError {
    GatewayError::not_found(format!(
        "previous response '{id}' not found (unknown, expired, created with store=false, or over this key's conversation storage quota)"
    ))
}

fn store_unavailable(e: &redis::RedisError) -> GatewayError {
    tracing::error!(error = %e, "conversation store unavailable");
    GatewayError::unavailable("conversation state is temporarily unavailable; retry, or send the full input instead")
}

/// Look up `previous_response_id` for `key_id`.
pub async fn resolve(state: &AppState, previous: &str, key_id: Uuid) -> Result<Previous, GatewayError> {
    if let Some(root) = root_of(previous) {
        let stored = state.cache.conversation(root).await.map_err(|e| store_unavailable(&e))?;
        let open = |id: &str| -> Option<Turn> {
            let sealed = stored.get(id)?;
            let plain = state.cfg.secret_box.open(sealed).ok()?;
            serde_json::from_slice(&plain).ok()
        };
        let mut turns = Vec::new();
        let mut next = Some(previous.to_string());
        while let Some(id) = next {
            let turn = open(&id).ok_or_else(|| not_found(previous))?;
            if turn.key_id != key_id || turns.len() >= MAX_TURNS {
                return Err(not_found(previous));
            }
            next = turn.parent.clone();
            turns.push(turn);
        }
        turns.reverse();
        touch(state, root.to_string(), key_id);
        return Ok(Previous::Gateway(History { turns }));
    }
    let raw = state.cache.pin(previous, state.settings.get().response_ttl()).await.map_err(|e| store_unavailable(&e))?;
    match raw.and_then(|r| serde_json::from_str::<Pin>(&r).ok()) {
        Some(pin) if pin.key_id == key_id => Ok(Previous::Upstream(pin)),
        _ => Err(not_found(previous)),
    }
}

/// Keep a conversation being continued from expiring, in the background.
fn touch(state: &AppState, root: String, key_id: Uuid) {
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = state.cache.touch_conversation(&root, key_id, state.settings.get().response_ttl()).await {
            tracing::warn!(error = %e, root, "failed to extend conversation");
        }
    });
}

/// Persist a translated turn in the background, within the key's conversation storage quota
/// (the `conversation_quota_mb` setting, over its conversations that have not expired): Valkey also
/// holds the rate limits, budgets and admin sessions, so no key may fill it. A turn over the
/// quota is not stored, and continuing from it answers "not found" like an expired one.
pub fn save_turn(state: &AppState, root: String, response_id: String, turn: Turn) {
    let state = state.clone();
    tokio::spawn(async move {
        let sealed = state.cfg.secret_box.seal(&serde_json::to_vec(&turn).expect("turn serializes"));
        let settings = state.settings.get();
        let (ttl, quota) = (settings.response_ttl(), settings.conversation_quota_bytes());
        match state.cache.reserve_conversation_bytes(turn.key_id, &root, sealed.len() as u64, quota, ttl).await {
            Ok(true) => {}
            Ok(false) => {
                return tracing::warn!(
                    key = %turn.key_id, response_id, bytes = sealed.len(),
                    "conversation storage quota reached; this turn is not stored and cannot be continued"
                );
            }
            // Valkey is unreachable; the write below fails the same way.
            Err(e) => tracing::warn!(error = %e, "could not check the conversation storage quota"),
        }
        if let Err(e) = state.cache.save_turn(&root, &response_id, &sealed, ttl).await {
            tracing::warn!(error = %e, response_id, "failed to store conversation turn");
        }
    });
}

/// Remember the deployment holding an upstream-stored response, in the background.
pub fn save_pin(state: &AppState, response_id: String, pin: Pin) {
    let state = state.clone();
    tokio::spawn(async move {
        let value = serde_json::to_string(&pin).expect("pin serializes");
        if let Err(e) = state.cache.save_pin(&response_id, &value, state.settings.get().response_ttl()).await {
            tracing::warn!(error = %e, response_id, "failed to store response pin");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Role;

    #[test]
    fn response_ids_carry_their_root() {
        let (root, first) = new_response_id(None);
        assert_eq!(root_of(&first), Some(root.as_str()));
        let (root2, second) = new_response_id(Some(&first));
        assert_eq!(root2, root);
        assert_ne!(second, first);
        assert_eq!(root_of("resp_68af3c2e"), None); // upstream (OpenAI) ids are not ours
        assert_eq!(root_of("msg_0123456789abcdef0123456789abcdef"), None);
    }

    #[test]
    fn foreign_reasoning_is_dropped_on_replay() {
        let turn = |provider| Turn {
            parent: None,
            key_id: Uuid::nil(),
            deployment_id: Uuid::nil(),
            provider,
            system: vec![],
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![
                    Part::Reasoning { text: "hmm".into(), signature: Some("sig".into()) },
                    Part::Text { text: "hi".into(), cache: false },
                ],
            }],
        };
        let h = History { turns: vec![turn(ProviderKind::Anthropic)] };
        assert_eq!(h.messages_for(ProviderKind::Anthropic)[0].content.len(), 2);
        assert_eq!(h.messages_for(ProviderKind::Bedrock)[0].content.len(), 1);
        let json = serde_json::to_string(&h.turns[0]).unwrap();
        let back: Turn = serde_json::from_str(&json).unwrap();
        assert_eq!(back.messages[0].content.len(), 2);
    }
}
