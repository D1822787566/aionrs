use serde::{Deserialize, Serialize};

/// Runtime-owned identity; none of these fields are supplied by tool arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentIdentity {
    pub child_run_id: String,
    pub parent_call_id: String,
    pub task_index: usize,
    pub name: String,
    pub prompt: String,
    pub started_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentEvent {
    pub run: SubagentIdentity,
    pub occurred_at: i64,
    pub change: SubagentChange,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentChange {
    Started,
    Text {
        entry_id: String,
        text: String,
        thinking: bool,
    },
    MessageEnd {
        msg_id: String,
    },
    ToolCall {
        tool_call_id: String,
        name: String,
        input: String,
    },
    ToolResult {
        tool_call_id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    Finished {
        status: SubagentStatus,
        text: String,
        elapsed_ms: u64,
    },
}
