//! What the model is given, kept in the agent's own form. Each provider
//! format is built from this, so the same context can go to any provider.

use serde_json::Value;

// Only `User` is built outside tests until the tool loop lands.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    System(String),
    User(String),
    Assistant(Reply),
    ToolResult { call_id: String, content: String },
}

/// One model turn: text, reasoning and the tool calls it made.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reply {
    pub text: String,
    pub reasoning: String,
    pub calls: Vec<ToolCall>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments as the model wrote them; usually JSON, not checked here.
    pub args: String,
}

/// A tool as the model sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
}
