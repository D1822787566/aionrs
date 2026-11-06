use super::*;

fn body(text: &str) -> ToolResult {
    ToolResult {
        content: text.into(),
        is_error: false,
    }
}

#[test]
fn invalid_ranges_never_report_loaded_or_deliver_instructions() {
    for range in [
        json!({"offset":1}),
        json!({"limit":0}),
        json!({"limit":1}),
        json!({"offset":100}),
        json!({"offset":-1}),
        json!({"limit":"4"}),
    ] {
        let output = deliver_skill("test", &range, body("中文"));
        assert!(output.result.is_error, "{range}");
        assert!(output.follow_up_blocks.is_empty());
    }
}

#[test]
fn batch_budget_is_shared_and_rejection_returns_a_readable_chunk_size() {
    let original = "中文 instructions\n".repeat(1_000);
    let mut first = deliver_skill("test", &json!({}), body(&original));
    let mut budget = estimate_content_tokens(&first.follow_up_blocks) + 500;
    reserve_skill_context(&mut first, &mut budget);
    assert!(!first.result.is_error);
    assert_eq!(budget, 500);
    let mut second = deliver_skill("test", &json!({}), body(&original));
    reserve_skill_context(&mut second, &mut budget);
    assert!(second.result.is_error);
    assert!(second.follow_up_blocks.is_empty());
    let response: Value = serde_json::from_str(&second.result.content).unwrap();
    let mut chunk = deliver_skill(
        "test",
        &json!({"offset":0,"limit":response["suggested_limit"]}),
        body(&original),
    );
    reserve_skill_context(&mut chunk, &mut budget);
    assert!(!chunk.result.is_error);
    assert!(chunk.result.content.contains("skill_chunk_loaded"));
}

#[test]
fn exhausted_budget_requests_compaction_without_a_fake_success() {
    let mut output = deliver_skill("test", &json!({}), body("instructions"));
    reserve_skill_context(&mut output, &mut 0);
    assert!(output.result.is_error);
    assert!(output.result.content.contains("compact_or_start_new_conversation"));
    assert!(output.follow_up_blocks.is_empty());
}
