//! Provider-neutral request/stream representation used whenever the inbound API
//! differs from the upstream wire format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub system: Vec<TextBlock>,
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop: Vec<String>,
    pub tools: Vec<Tool>,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning: Option<Reasoning>,
    /// Client asked to see reasoning summaries.
    pub reasoning_summary: bool,
    /// JSON schema structured output (`{name, schema, strict}`), only for OpenAI upstreams.
    pub json_schema: Option<Value>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
    pub cache: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text { text: String, cache: bool },
    Image { source: ImageSource, cache: bool },
    /// A document such as a PDF, inline as base64. The gateway does not download or upload files.
    File { media_type: String, data: String, filename: Option<String>, cache: bool },
    ToolCall { id: String, name: String, arguments: String },
    ToolResult { call_id: String, content: Vec<Part>, is_error: bool, cache: bool },
    Reasoning { text: String, signature: Option<String> },
}

// Externally tagged: an internal tag cannot wrap the plain string in `Url`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url(String),
}

/// Splits `data:<mime>;base64,<data>` into the media type and the data.
fn split_data_url(url: &str) -> Option<(&str, &str)> {
    let (meta, data) = url.strip_prefix("data:")?.split_once(',')?;
    Some((meta.strip_suffix(";base64")?, data))
}

impl ImageSource {
    /// Parse `data:<mime>;base64,<data>` URLs into inline images.
    pub fn from_url(url: &str) -> Self {
        match split_data_url(url) {
            Some((media_type, data)) => Self::Base64 { media_type: media_type.to_string(), data: data.to_string() },
            None => Self::Url(url.to_string()),
        }
    }

    pub fn to_url(&self) -> String {
        match self {
            Self::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
            Self::Url(u) => u.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Value,
    pub cache: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool(String),
}

#[derive(Debug, Clone, Copy)]
pub enum Reasoning {
    Effort(Effort),
    Budget(u32),
}

/// A reasoning effort, in order: every level OpenAI's and Anthropic's APIs take. Which a model
/// accepts comes from its price catalog entry (see `Deployment::efforts`); translated requests get
/// the nearest of those.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
#[schema(as = ReasoningEffort)]
pub enum Effort {
    /// No reasoning: OpenAI's `none`; for Anthropic models, no thinking.
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const ALL: [Self; 7] = [Self::None, Self::Minimal, Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The closest of `supported`, or this one when `supported` is empty. Between two equally
    /// close levels, the higher: a model without `minimal` gets `low`, not `none`.
    pub fn nearest(self, supported: &[Self]) -> Self {
        let distance = |e: &Self| (*e as i32 - self as i32).abs();
        supported.iter().copied().min_by_key(|e| (distance(e), std::cmp::Reverse(*e))).unwrap_or(self)
    }
}

impl Reasoning {
    pub fn budget_tokens(self) -> u32 {
        match self {
            Self::Budget(b) => b,
            Self::Effort(Effort::None | Effort::Minimal) => 1024,
            Self::Effort(Effort::Low) => 2048,
            Self::Effort(Effort::Medium) => 8192,
            Self::Effort(Effort::High) => 24576,
            Self::Effort(Effort::Xhigh) => 32768,
            Self::Effort(Effort::Max) => 65536,
        }
    }

    pub fn effort(self) -> Effort {
        match self {
            Self::Effort(e) => e,
            Self::Budget(b) if b <= 1024 => Effort::Minimal,
            Self::Budget(b) if b <= 4096 => Effort::Low,
            Self::Budget(b) if b <= 16384 => Effort::Medium,
            Self::Budget(_) => Effort::High,
        }
    }
}

impl ChatRequest {
    /// Merge consecutive messages with the same role; Anthropic and Bedrock require alternation.
    pub fn normalize(&mut self) {
        let mut out: Vec<Message> = Vec::with_capacity(self.messages.len());
        for m in self.messages.drain(..) {
            if m.content.is_empty() {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.role == m.role => last.content.extend(m.content),
                _ => out.push(m),
            }
        }
        self.messages = out;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// Rerank searches billed (a query over up to 100 documents is one).
    pub search_units: u64,
}

impl Usage {
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn total(&self) -> u64 {
        self.total_input() + self.output_tokens
    }

    /// Fold in a partial report; providers send usage in several pieces.
    pub fn merge_max(&mut self, o: &Usage) {
        self.input_tokens = self.input_tokens.max(o.input_tokens);
        self.output_tokens = self.output_tokens.max(o.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(o.cache_read_tokens);
        self.cache_write_tokens = self.cache_write_tokens.max(o.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.max(o.reasoning_tokens);
        self.search_units = self.search_units.max(o.search_units);
    }

    /// Add another request's usage (a routed request sums its calls).
    pub fn add(&mut self, o: &Usage) {
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.cache_read_tokens += o.cache_read_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
        self.search_units += o.search_units;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopReason {
    #[default]
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    ContentFilter,
}

#[derive(Debug, Clone)]
pub enum BlockKind {
    Text,
    ToolCall { id: String, name: String },
    Reasoning,
}

/// Normalized streaming event. Block indices are sequential from 0 and every
/// started block is stopped before `Finish`.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    BlockStart { index: usize, kind: BlockKind },
    TextDelta { index: usize, text: String },
    ToolArgsDelta { index: usize, json: String },
    ReasoningDelta { index: usize, text: String },
    SignatureDelta { index: usize, signature: String },
    BlockStop { index: usize },
    Finish { stop_reason: StopReason, usage: Usage },
}

impl StreamEvent {
    pub fn is_content(&self) -> bool {
        matches!(
            self,
            Self::TextDelta { .. } | Self::ToolArgsDelta { .. } | Self::ReasoningDelta { .. } | Self::BlockStart { .. }
        )
    }
}

/// A finished, non-streamed completion assembled from stream events.
#[derive(Debug, Clone, Default)]
pub struct ChatResponse {
    pub content: Vec<Part>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// Collects stream events into a [`ChatResponse`].
#[derive(Default)]
pub struct Aggregator {
    blocks: Vec<Option<Part>>,
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub finished: bool,
}

impl Aggregator {
    pub fn push(&mut self, ev: &StreamEvent) {
        match ev {
            StreamEvent::BlockStart { index, kind } => {
                if self.blocks.len() <= *index {
                    self.blocks.resize(*index + 1, None);
                }
                self.blocks[*index] = Some(match kind {
                    BlockKind::Text => Part::Text { text: String::new(), cache: false },
                    BlockKind::ToolCall { id, name } => {
                        Part::ToolCall { id: id.clone(), name: name.clone(), arguments: String::new() }
                    }
                    BlockKind::Reasoning => Part::Reasoning { text: String::new(), signature: None },
                });
            }
            StreamEvent::TextDelta { index, text } => {
                if let Some(Some(Part::Text { text: t, .. })) = self.blocks.get_mut(*index) {
                    t.push_str(text);
                }
            }
            StreamEvent::ToolArgsDelta { index, json } => {
                if let Some(Some(Part::ToolCall { arguments, .. })) = self.blocks.get_mut(*index) {
                    arguments.push_str(json);
                }
            }
            StreamEvent::ReasoningDelta { index, text } => {
                if let Some(Some(Part::Reasoning { text: t, .. })) = self.blocks.get_mut(*index) {
                    t.push_str(text);
                }
            }
            StreamEvent::SignatureDelta { index, signature } => {
                if let Some(Some(Part::Reasoning { signature: s, .. })) = self.blocks.get_mut(*index) {
                    s.get_or_insert_with(String::new).push_str(signature);
                }
            }
            StreamEvent::BlockStop { .. } => {}
            StreamEvent::Finish { stop_reason, usage } => {
                self.stop_reason = *stop_reason;
                self.usage = *usage;
                self.finished = true;
            }
        }
    }

    pub fn finish(self) -> ChatResponse {
        let content = self
            .blocks
            .into_iter()
            .flatten()
            .map(|p| match p {
                Part::ToolCall { id, name, arguments } if arguments.trim().is_empty() => {
                    Part::ToolCall { id, name, arguments: "{}".into() }
                }
                p => p,
            })
            .collect();
        ChatResponse { content, stop_reason: self.stop_reason, usage: self.usage }
    }
}

/// Helper for decoders that open blocks lazily and must close them in order.
#[derive(Default)]
pub struct BlockTracker {
    next_index: usize,
    open: Vec<usize>,
}

impl BlockTracker {
    pub fn start(&mut self, out: &mut Vec<StreamEvent>, kind: BlockKind) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        self.open.push(index);
        out.push(StreamEvent::BlockStart { index, kind });
        index
    }

    pub fn stop(&mut self, out: &mut Vec<StreamEvent>, index: usize) {
        if let Some(pos) = self.open.iter().position(|&i| i == index) {
            self.open.remove(pos);
            out.push(StreamEvent::BlockStop { index });
        }
    }

    pub fn stop_all(&mut self, out: &mut Vec<StreamEvent>) {
        for index in self.open.drain(..) {
            out.push(StreamEvent::BlockStop { index });
        }
    }
}

/// Parse tool arguments into a JSON object for upstreams that need structured input.
pub fn parse_args(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return Value::Object(Default::default());
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(v @ Value::Object(_)) => v,
        Ok(other) => serde_json::json!({ "value": other }),
        Err(_) => serde_json::json!({ "_raw": arguments }),
    }
}

/// Plain text of tool result content, for upstreams that only accept strings.
impl Part {
    /// A file part from a `data:<mime>;base64,<data>` URL, as OpenAI's APIs send them; `None` for
    /// anything else.
    pub fn file_from_data_url(url: &str, filename: Option<&str>) -> Option<Self> {
        let (media_type, data) = split_data_url(url)?;
        Some(Self::File {
            media_type: media_type.to_string(),
            data: data.to_string(),
            filename: filename.filter(|f| !f.is_empty()).map(str::to_string),
            cache: false,
        })
    }
}

/// The file's name, or one made up from its type for providers that require a name.
pub fn file_name(filename: &Option<String>, media_type: &str) -> String {
    filename.clone().unwrap_or_else(|| match media_type {
        "application/pdf" => "document.pdf".into(),
        _ => "document".into(),
    })
}

pub fn parts_text(parts: &[Part]) -> String {
    let mut s = String::new();
    for p in parts {
        if let Part::Text { text, .. } = p {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(text);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn efforts_fit_the_nearest_supported_level() {
        use Effort::*;
        let luna = [None, Low, Medium, High, Xhigh, Max];
        assert_eq!(Minimal.nearest(&luna), Low, "a tie goes up");
        assert_eq!(None.nearest(&luna), None);
        assert_eq!(None.nearest(&[Minimal, Low, Medium, High]), Minimal);
        assert_eq!(Max.nearest(&[Low, Medium, High]), High);
        assert_eq!(Xhigh.nearest(&[None, High]), High);
        assert_eq!(Low.nearest(&[]), Low);
        assert_eq!(Effort::parse("xhigh"), Some(Xhigh));
        assert_eq!(Effort::parse("extreme"), Option::None);
        assert_eq!(serde_json::to_value(Max).unwrap(), "max");
    }
}
