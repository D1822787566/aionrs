use aion_types::llm::InvalidToolInput;
use serde_json::{Value, json};

pub(crate) const TEXT_CALL_RETRY_LIMIT: usize = 2;
pub(crate) const TEXT_CALL_RETRY_PROMPT: &str = "The previous tool-call markup was only text and was NOT executed. Reissue the intended operation using a formal tool call matching the advertised schema, or explain the actual completed result in plain text. Do not claim that textual markup executed.";

pub(crate) fn tool_call_examples_requested(text: &str) -> bool {
    let text = text.to_lowercase();
    ["<tool_call>", "工具调用", "tool call", "function call"]
        .iter()
        .any(|term| text.contains(term))
        && ["示例", "格式", "解释", "演示", "example", "show", "explain"]
            .iter()
            .any(|term| text.contains(term))
}

pub(crate) fn invalid_input_feedback(error: &InvalidToolInput) -> String {
    json!({
        "code":"invalid_tool_arguments", "reason":"invalid_json", "executed":false,
        "tool":error.name, "call_id":error.id, "line":error.line, "column":error.column,
        "instruction":"Regenerate this call as a formal tool call with valid JSON arguments matching its schema."
    })
    .to_string()
}

/// Detect a complete invocation shape, never turn its contents into an operation.
pub(crate) fn textual_tool_call(text: &str, allowed: impl Fn(&str) -> bool) -> bool {
    let visible = without_examples(text);
    let mut remaining = visible.as_str();
    while let Some((_, tail)) = remaining.split_once("<tool_call>") {
        let Some((body, rest)) = tail.split_once("</tool_call>") else {
            return false;
        };
        if let Some(function) = body.trim().strip_prefix("<function=") {
            if let Some((name, parameters)) = function.split_once('>')
                && parameters.trim_end().ends_with("</function>")
                && allowed(name.trim())
            {
                return true;
            }
        } else if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
            let name = value.get("name").or_else(|| value.pointer("/function/name"));
            if name.and_then(Value::as_str).is_some_and(&allowed) {
                return true;
            }
        }
        remaining = rest;
    }
    false
}

fn without_examples(text: &str) -> String {
    let mut result = String::new();
    let mut fence: Option<(char, usize)> = None;
    let mut inline_ticks = 0;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        let count = marker.map_or(0, |c| trimmed.chars().take_while(|v| *v == c).count());
        if let Some((kind, width)) = fence {
            if marker == Some(kind) && count >= width {
                fence = None;
            }
            continue;
        }
        if count >= 3 {
            fence = marker.map(|c| (c, count));
            continue;
        }
        if trimmed.starts_with('>') || line.starts_with("    ") || line.starts_with('\t') {
            continue;
        }
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '`' {
                let mut width = 1;
                while chars.peek() == Some(&'`') {
                    chars.next();
                    width += 1;
                }
                if inline_ticks == 0 {
                    inline_ticks = width;
                } else if inline_ticks == width {
                    inline_ticks = 0;
                }
            } else if inline_ticks == 0 {
                result.push(c);
            }
        }
        result.push('\n');
    }
    result
}

#[cfg(test)]
#[path = "tool_recovery_test.rs"]
mod tool_recovery_test;
