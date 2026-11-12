use super::*;

fn source(data: Value) -> ToolResult {
    encode(
        json!({"success":true,"operation":"skills.show","data":data,"code":null,"error":null,"meta":{}}),
        false,
    )
}

#[test]
fn errors_preserve_primary_diagnostic_and_execution_facts_at_each_budget() {
    let output = OperationOutput::default();
    let original = json!({"success":false,"operation":"config.mcp.servers.get","code":"CONFIG_PAYLOAD_INVALID","data":null,
        "error":{"message":"Invalid input","details":{"errors":[{"path":"/server_id","expected":"string","actual":"number"}],"extra":"x".repeat(1200)}},
        "execution":{"status":"not_executed","stage":"preflight"}});
    let raw = encode(original.clone(), true);
    for budget in [512, 1024, 2048, 10000] {
        let result = output.format(&json!({}), &raw, budget);
        let value: Value = serde_json::from_str(&result.content).unwrap();
        assert!(result.content.len() <= budget);
        assert_eq!(value["code"], "CONFIG_PAYLOAD_INVALID");
        assert_eq!(value["delivery"]["execution_status"], "not_executed");
        if value["delivery"]["complete"] == false {
            assert_eq!(value["error"]["field_error"]["path"], "/server_id", "{value}");
            let page = output.read(&json!({"result_id":value["delivery"]["result_id"],"path":""}), 10000);
            let page: Value = serde_json::from_str(&page.content).unwrap();
            // The nested large detail remains a pointer on this page.
            assert_eq!(page["delivery"]["complete"], false);
            let details = output.read(
                &json!({"result_id":value["delivery"]["result_id"],"path":"/error/details/extra"}),
                10000,
            );
            assert_eq!(
                serde_json::from_str::<Value>(&details.content).unwrap()["data"],
                original["error"]["details"]["extra"]
            );
        }
    }
}

#[test]
fn whole_envelope_read_is_complete_when_no_field_was_summarized() {
    let output = OperationOutput::default();
    let original = json!({"success":false,"operation":"config.mcp.servers.get","code":"FIXTURE","data":null,"error":{"message":"x".repeat(350)},"execution":{"status":"unknown"}});
    let first = output.format(&json!({}), &encode(original.clone(), true), 512);
    let first: Value = serde_json::from_str(&first.content).unwrap();
    let page = output.read(&json!({"result_id":first["delivery"]["result_id"],"path":""}), 10000);
    let page: Value = serde_json::from_str(&page.content).unwrap();
    assert_eq!(page["data"], original);
    assert_eq!(page["delivery"]["complete"], true);
    assert_eq!(page["delivery"]["has_more"], false);
}

#[test]
fn unicode_and_json_escapes_reconstruct_with_forward_progress() {
    let output = OperationOutput::default();
    let body = "中文😀\n\r\\\"\t".repeat(3000);
    let first = output.format(&json!({}), &source(json!({"body":body})), 2048);
    let v: Value = serde_json::from_str(&first.content).unwrap();
    let id = v["delivery"]["result_id"].as_str().unwrap();
    let mut offset = 0;
    let mut actual = String::new();
    loop {
        let page = output.read(
            &json!({"result_id":id,"path":"/data/body","offset":offset,"limit":1700}),
            2048,
        );
        assert!(page.content.len() <= 2048);
        let v: Value = serde_json::from_str(&page.content).unwrap();
        actual.push_str(v["data"].as_str().expect(&page.content));
        let Some(next) = v["delivery"]["next_offset"].as_u64() else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(body, actual);
    let invalid = output.read(&json!({"result_id":id,"path":"/data/body","offset":1}), 2048);
    assert!(invalid.is_error);
    assert!(invalid.content.contains("invalid_offset"));
}

#[test]
fn eviction_and_tool_instances_do_not_reexecute_or_share_results() {
    let output = OperationOutput::default();
    let first = output.format(&json!({}), &source(json!({"body":"x".repeat(20000)})), 2048);
    let v: Value = serde_json::from_str(&first.content).unwrap();
    let input = json!({"result_id":v["delivery"]["result_id"]});
    assert!(
        OperationOutput::default()
            .read(&input, 2048)
            .content
            .contains("result_expired")
    );
    for _ in 0..MAX_RESULTS {
        output.format(&json!({}), &source(json!({"body":"x".repeat(20000)})), 2048);
    }
    assert!(output.read(&input, 2048).content.contains("result_expired"));
    assert!(output.read(&input, 2048).content.contains("Do not repeat a write"));
}

#[test]
fn huge_write_receipt_preserves_success_and_unknown_errors_stay_unknown() {
    let output = OperationOutput::default();
    for success in [true, false] {
        let raw = encode(
            json!({"operation":"config.mcp.servers.create","success":success,"data":{"id":"created"},"meta":{"before":"x".repeat(20000)}}),
            !success,
        );
        let page = output.format(&json!({}), &raw, 2048);
        let v: Value = serde_json::from_str(&page.content).unwrap();
        assert_eq!(v["success"], success);
        assert_eq!(page.is_error, !success);
        assert_eq!(
            v["delivery"]["execution_status"],
            if success { "succeeded" } else { "unknown" }
        );
        assert!(v["delivery"]["result_id"].is_string());
    }
}

#[test]
fn expired_time_and_oversized_snapshots_are_explicit() {
    let mut store = Store::default();
    let snapshot = Snapshot {
        id: "old".into(),
        value: json!({}),
        bytes: 1,
        created: Instant::now() - SNAPSHOT_TTL,
    };
    store.bytes = 1;
    store.entries.push_back(Arc::new(snapshot));
    assert!(store.get("old").is_none());
    assert_eq!(store.bytes, 0);
    assert!(store.insert(json!({}), MAX_SNAPSHOT_BYTES + 1).is_none());
}

#[test]
fn fitted_results_are_idempotent_at_nondefault_budgets() {
    for budget in [512, 1024, 2048, 8192, 10000] {
        let output = OperationOutput::default();
        for raw in [
            source(json!({"body":"\\\"中文\n".repeat(4000)})),
            encode(
                json!({"success":false,"operation":"config.context","code":"FAIL","error":{"message":"x".repeat(20000)}}),
                true,
            ),
        ] {
            let page = output.format(&json!({}), &raw, budget);
            assert!(page.content.len() <= budget, "{budget}: {}", page.content.len());
            let value: Value = serde_json::from_str(&page.content).unwrap();
            assert_eq!(value["success"], !raw.is_error);
            if raw.is_error {
                assert_eq!(value["code"], "FAIL");
            }
            assert_eq!(output.format(&json!({}), &page, budget).content, page.content);
        }
    }
}

#[test]
fn json_pointer_escaping_and_missing_paths_do_not_fetch_other_data() {
    let output = OperationOutput::default();
    let raw = source(json!({"a/b~c":"z".repeat(12000)}));
    let first = output.format(&json!({}), &raw, 2048);
    let value: Value = serde_json::from_str(&first.content).unwrap();
    let id = &value["delivery"]["result_id"];
    let page = output.read(&json!({"result_id":id,"path":"/data/a~1b~0c"}), 2048);
    assert!(!page.is_error);
    assert!(serde_json::from_str::<Value>(&page.content).unwrap()["data"].is_string());
    assert!(
        output
            .read(&json!({"result_id":id,"path":"/data/missing"}), 2048)
            .is_error
    );
    assert!(output.read(&json!({"result_id":id,"offset":-1}), 2048).is_error);
    assert!(output.read(&json!({"result_id":id,"limit":0}), 2048).is_error);
}

#[test]
fn minimum_budget_can_return_pointers_and_make_text_progress() {
    let output = OperationOutput::default();
    let first = output.format(&json!({}), &source(json!({"body":"中".repeat(3000)})), MIN_OUTPUT_BYTES);
    let value: Value = serde_json::from_str(&first.content).unwrap();
    let id = value["delivery"]["result_id"].as_str().expect(&first.content);
    let page = output.read(&json!({"result_id":id,"path":"/data/body"}), MIN_OUTPUT_BYTES);
    let v: Value = serde_json::from_str(&page.content).unwrap();
    assert!(page.content.len() <= MIN_OUTPUT_BYTES);
    assert!(!v["data"].as_str().expect(&page.content).is_empty());
    assert!(v["delivery"]["next_offset"].as_u64().unwrap() > 0);
}

#[test]
fn a_write_result_above_storage_limit_preserves_success_and_resource_identity() {
    let output = OperationOutput::default();
    let raw = encode(
        json!({"success":true,"operation":"config.mcp.servers.create","data":{"id":"created-resource"},"meta":{"after":"x".repeat(MAX_SNAPSHOT_BYTES+1)}}),
        false,
    );
    let result = output.format(&json!({}), &raw, MIN_OUTPUT_BYTES);
    assert!(result.content.len() <= MIN_OUTPUT_BYTES);
    assert!(!result.is_error);
    let value: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(value["resource_id"], "created-resource");
    assert_eq!(value["delivery"]["execution_status"], "succeeded");
    assert_eq!(value["delivery"]["reason"], "result_exceeds_snapshot_limit");
    assert_eq!(value["delivery"]["retry_write"], false);
}
