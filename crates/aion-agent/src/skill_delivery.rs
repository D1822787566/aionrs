//! Skill instructions use follow-up context; ordinary tool output stays bounded.
use aion_tools::{ToolExecutionOutput, truncate_utf8};
use aion_types::message::ContentBlock;
use aion_types::tool::ToolResult;
use serde_json::{Value, json};

use crate::context_usage::estimate_content_tokens;

pub(crate) fn deliver_skill(name: &str, input: &Value, result: ToolResult) -> ToolExecutionOutput {
    if result.is_error {
        return result.into();
    }
    let content = result.content;
    let offset = input["offset"].as_u64().unwrap_or(0);
    let limit = input["limit"].as_u64().unwrap_or(content.len() as u64);
    if ["offset", "limit"]
        .iter()
        .any(|key| input.get(key).is_some_and(|v| v.as_u64().is_none()))
        || offset > content.len() as u64
        || !content.is_char_boundary(offset as usize)
        || limit == 0
    {
        return ToolResult {
            content: json!({"code":"invalid_skill_range","skill":name,"total_bytes":content.len(),
                "message":"Use a zero-based UTF-8 byte boundary and a positive limit."})
            .to_string(),
            is_error: true,
        }
        .into();
    }
    let offset = offset as usize;
    let end = offset
        .saturating_add(usize::try_from(limit).unwrap_or(usize::MAX))
        .min(content.len());
    let body = truncate_utf8(&content[offset..], end - offset);
    if body.is_empty() && offset < content.len() {
        return ToolResult {
            content: json!({"code":"invalid_skill_range","skill":name,"suggested_limit":4,
                "message":"The limit must fit at least one complete UTF-8 character."})
            .to_string(),
            is_error: true,
        }
        .into();
    }
    let next = offset + body.len();
    let partial = offset != 0 || next != content.len();
    let label = if partial { "SKILL CHUNK" } else { "LOADED SKILL" };
    ToolExecutionOutput {
        result: ToolResult {
            content: json!({
                "code":if partial {"skill_chunk_loaded"} else {"skill_loaded"},"skill":name,
                "content_bytes":body.len(),"total_bytes":content.len(),"offset":offset,
                "next_offset":next,"complete":next == content.len(),"full_content":!partial,
                "content_delivered":"follow_up_context"
            })
            .to_string(),
            is_error: false,
        },
        follow_up_blocks: vec![ContentBlock::Text {
            text: format!("[BEGIN {label}: {name}]\n{body}\n[END {label}: {name}]"),
        }],
    }
}

/// Share one remaining budget across every successful Skill in a tool round.
pub(crate) fn reserve_skill_context(output: &mut ToolExecutionOutput, remaining: &mut u64) {
    let Ok(info) = serde_json::from_str::<Value>(&output.result.content) else {
        return;
    };
    if !matches!(info["code"].as_str(), Some("skill_loaded" | "skill_chunk_loaded")) {
        return;
    }
    let required = estimate_content_tokens(&output.follow_up_blocks);
    if required <= *remaining {
        *remaining -= required;
        return;
    }
    let suggested_limit = remaining.saturating_mul(4).saturating_sub(512).min(8_000);
    output.result = ToolResult {
        content: json!({
            "code":"skill_requires_chunked_read","skill":info["skill"],
            "total_bytes":info["total_bytes"],"next_offset":info["offset"],
            "suggested_limit":suggested_limit,"required_tokens":required,
            "available_tokens":*remaining,"content_delivered":false,
            "action":if suggested_limit == 0 {"compact_or_start_new_conversation"} else {"call_Skill_with_offset_and_limit"}
        }).to_string(),
        is_error: true,
    };
    output.follow_up_blocks.clear();
    tracing::info!(target:"aion_agent", required_tokens=required, available_tokens=*remaining,
        "skill requires chunked delivery; no instructions delivered");
}

#[cfg(test)]
#[path = "skill_delivery_test.rs"]
mod skill_delivery_test;
