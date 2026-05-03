//! Microcompact: clear old tool result content without any LLM call.
//!
//! This is the lightest compaction level.  It walks the conversation,
//! identifies tool results from compactable tools, and replaces the
//! content of all but the N most recent with a short placeholder.

use std::collections::{HashMap, HashSet};

use aion_config::compact::CompactConfig;
use aion_types::message::{ContentBlock, Message, Role};
use chrono::Utc;
use serde_json::Value;

use crate::session::ToolExecutionAuditRecord;

/// Placeholder that replaces cleared tool result content.
pub const CLEARED_TOOL_RESULT: &str = "[Tool result cleared]";

/// Statistics returned after a microcompact pass.
#[derive(Debug, Clone, PartialEq)]
pub struct MicrocompactResult {
    /// Number of tool results whose content was cleared.
    pub cleared_count: usize,
    /// Rough estimate of tokens freed (content bytes / 4).
    pub estimated_tokens_freed: usize,
    /// Original tool executions captured before their results were cleared.
    pub cleared_tool_results: Vec<ToolExecutionAuditRecord>,
}

// ── Trigger checks ──────────────────────────────────────────────────────────

/// Decide whether microcompact should run.
///
/// Returns `true` if **either** trigger fires:
/// - **Time**: the most recent assistant message is older than
///   `config.micro_gap_seconds`.
/// - **Count**: total compactable (non-cleared) tool results exceed
///   `config.micro_keep_recent * 2`.
pub fn should_microcompact(messages: &[Message], config: &CompactConfig) -> bool {
    if !config.enabled {
        return false;
    }
    time_trigger(messages, config) || count_trigger(messages, config)
}

/// Time-based trigger: last assistant timestamp older than gap threshold.
fn time_trigger(messages: &[Message], config: &CompactConfig) -> bool {
    let last_assistant_ts = messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .find_map(|m| m.timestamp);

    let Some(ts) = last_assistant_ts else {
        return false;
    };

    let gap = Utc::now().signed_duration_since(ts);
    gap.num_seconds() >= config.micro_gap_seconds as i64
}

/// Count-based trigger: compactable tool results > keep_recent * 2.
fn count_trigger(messages: &[Message], config: &CompactConfig) -> bool {
    let tool_names = build_tool_name_map(messages);
    let compactable_set: HashSet<&str> = config.compactable_tools.iter().map(String::as_str).collect();

    let count = count_compactable_results(messages, &tool_names, &compactable_set);
    count > config.micro_keep_recent * 2
}

// ── Core compaction ─────────────────────────────────────────────────────────

/// Clear old tool result content in-place.
///
/// Keeps the `config.micro_keep_recent` most recent compactable results
/// (minimum 1) and replaces older ones with [`CLEARED_TOOL_RESULT`].
/// Already-cleared results are left untouched and do not count toward
/// the keep budget.
pub fn microcompact(messages: &mut [Message], config: &CompactConfig) -> MicrocompactResult {
    let tool_names = build_tool_name_map(messages);
    let tool_inputs = build_tool_input_map(messages);
    let compactable_set: HashSet<&str> = config.compactable_tools.iter().map(String::as_str).collect();

    // Collect (message_index, block_index) of all compactable, non-cleared
    // tool results, in conversation order.
    let targets = collect_compactable_locations(messages, &tool_names, &compactable_set);

    let keep = config.micro_keep_recent.max(1);
    if targets.len() <= keep {
        return MicrocompactResult {
            cleared_count: 0,
            estimated_tokens_freed: 0,
            cleared_tool_results: Vec::new(),
        };
    }

    let to_clear = &targets[..targets.len() - keep];

    let mut cleared_count = 0usize;
    let mut tokens_freed = 0usize;
    let mut cleared_tool_results = Vec::with_capacity(to_clear.len());

    for &(mi, bi) in to_clear {
        let message = &mut messages[mi];
        let message_timestamp = message.timestamp;
        let turn_id = message.turn_id.clone();
        if let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = &mut message.content[bi]
        {
            // Rough token estimate: ~4 chars per token.
            tokens_freed += content.len() / 4;
            if let Some(tool_name) = tool_names.get(tool_use_id) {
                cleared_tool_results.push(ToolExecutionAuditRecord {
                    tool_use_id: tool_use_id.clone(),
                    tool_name: tool_name.clone(),
                    input: tool_inputs.get(tool_use_id).cloned().unwrap_or(Value::Null),
                    output: content.clone(),
                    is_error: *is_error,
                    message_timestamp,
                    turn_id,
                });
            }
            *content = CLEARED_TOOL_RESULT.to_string();
            cleared_count += 1;
        }
    }

    MicrocompactResult {
        cleared_count,
        estimated_tokens_freed: tokens_freed,
        cleared_tool_results,
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build a map from tool_use_id → tool name by scanning ToolUse blocks
/// across all messages.
fn build_tool_name_map(messages: &[Message]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                map.insert(id.clone(), name.clone());
            }
        }
    }
    map
}

fn build_tool_input_map(messages: &[Message]) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, input, .. } = block {
                map.insert(id.clone(), input.clone());
            }
        }
    }
    map
}

/// Capture every live tool result that autocompact would remove.
pub(crate) fn snapshot_live_tool_results(messages: &[Message]) -> Vec<ToolExecutionAuditRecord> {
    let tool_names = build_tool_name_map(messages);
    let tool_inputs = build_tool_input_map(messages);
    let mut records = Vec::new();

    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = block
                && content != CLEARED_TOOL_RESULT
                && let Some(tool_name) = tool_names.get(tool_use_id)
            {
                records.push(ToolExecutionAuditRecord {
                    tool_use_id: tool_use_id.clone(),
                    tool_name: tool_name.clone(),
                    input: tool_inputs.get(tool_use_id).cloned().unwrap_or(Value::Null),
                    output: content.clone(),
                    is_error: *is_error,
                    message_timestamp: message.timestamp,
                    turn_id: message.turn_id.clone(),
                });
            }
        }
    }

    records
}

/// Count compactable, non-cleared tool results.
fn count_compactable_results(
    messages: &[Message],
    tool_names: &HashMap<String, String>,
    compactable_set: &HashSet<&str>,
) -> usize {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| is_compactable_and_live(b, tool_names, compactable_set))
        .count()
}

/// Collect `(message_index, block_index)` of every compactable, non-cleared
/// tool result in conversation order.
fn collect_compactable_locations(
    messages: &[Message],
    tool_names: &HashMap<String, String>,
    compactable_set: &HashSet<&str>,
) -> Vec<(usize, usize)> {
    let mut locations = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        for (bi, block) in msg.content.iter().enumerate() {
            if is_compactable_and_live(block, tool_names, compactable_set) {
                locations.push((mi, bi));
            }
        }
    }
    locations
}

/// A tool result is "compactable and live" when:
/// 1. It is a `ToolResult` variant.
/// 2. Its corresponding tool name is in the compactable set.
/// 3. Its content has not already been cleared.
fn is_compactable_and_live(
    block: &ContentBlock,
    tool_names: &HashMap<String, String>,
    compactable_set: &HashSet<&str>,
) -> bool {
    if let ContentBlock::ToolResult {
        tool_use_id, content, ..
    } = block
    {
        if content == CLEARED_TOOL_RESULT {
            return false;
        }
        if let Some(name) = tool_names.get(tool_use_id) {
            return compactable_set.contains(name.as_str());
        }
    }
    false
}

#[cfg(test)]
#[path = "micro_test.rs"]
mod micro_test;
