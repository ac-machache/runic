//! Plain data types for the dev console — the UI model the event stream and
//! persisted log fold into. No Leptos, no I/O.

/// A thread in the sidebar list — its id plus the optional label (title).
#[derive(Clone, PartialEq)]
pub struct ThreadInfo {
    pub id: String,
    pub label: Option<String>,
}

/// A file the user attached to the next message. Uploaded to the artifact
/// store on pick; the message carries the `id` as an `artifact_ref` block, not
/// the bytes.
#[derive(Clone, PartialEq)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub media_type: String,
    pub size: u64,
}

/// The actively-streaming tail. Tokens append here (one reactive text node)
/// instead of mutating the `items` list, so per-token cost is O(1) DOM. On a
/// boundary (a non-text event) or run end it flushes into `items` as a
/// finalized, markdown-rendered message.
#[derive(Clone, Default, PartialEq)]
pub struct LiveBuf {
    pub kind: LiveKind,
    pub text: String,
}

#[derive(Clone, Copy, Default, PartialEq)]
pub enum LiveKind {
    #[default]
    None,
    Text,
    Thinking,
}

/// One rendered entry in the chat transcript.
#[derive(Clone, Debug)]
pub enum Item {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool(ToolView),
    Warning(String),
}

#[derive(Clone, Debug, Default)]
pub struct ToolView {
    pub id: String,
    pub name: String,
    pub input: String,
    pub status: String,
    pub result: String,
    pub duration_ms: u64,
    pub sources: Vec<Source>,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub title: String,
    pub url: String,
}

/// A HITL `ask_user` waiting for the operator's answer (the new `HumanInterface`
/// model: a free-text question, not an editable tool-call draft).
#[derive(Clone)]
pub struct PendingAsk {
    pub ask_id: String,
    pub question: String,
    pub context: Option<String>,
}

// ── Events tab clustering (Run → Turn → details) ─────────────────────────

/// A run = one user message and the agent's answer to it. Holds the model
/// turns that happened in between.
#[derive(Clone, Default)]
pub struct RunCluster {
    pub id: String,
    pub prompt: String,
    pub running: bool,
    pub ended: bool,
    pub errored: bool,
    pub turns: Vec<TurnCluster>,
    pub stop_reason: Option<String>,
    pub usage: Option<(u64, u64)>,
}

#[derive(Clone, Default)]
pub struct TurnCluster {
    pub text: String,
    pub thinking: String,
    pub tools: Vec<ToolView>,
    pub stop_reason: Option<String>,
    pub tool_calls: u32,
    pub closed: bool,
}
