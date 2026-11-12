use serde_json::json;
use std::fs;

use super::{AionOperationTool, OPERATION_PATHS};
use crate::Tool;

// Protocol/output fixtures intentionally use permissive inputs. The Core bridge
// suite verifies the production catalog against real helper parsing.
fn fixture_contract() -> crate::aion_operation_contract::OperationContract {
    crate::aion_operation_contract::OperationContract::new(
        json!({"contract_version":1,"operations":OPERATION_PATHS.iter().map(|name|json!({"operation":name,"input_schema":{"type":"object"},"requires_confirmation":AionOperationTool::requires_confirmation(name)})).collect::<Vec<_>>()}),
    )
}

#[test]
fn schema_advertises_only_fixed_operations() {
    let tool = AionOperationTool::from_runtime_env(&[("AIONUI_HELPER_BIN".into(), "aioncore".into())])
        .expect("helper path should create the tool");
    let schema = tool.input_schema();
    assert_eq!(
        schema["properties"]["operation"]["enum"].as_array().unwrap().len(),
        OPERATION_PATHS.len() + 2
    );
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(tool.name(), "AionOperation");
}

#[test]
fn helper_path_is_required_before_advertising_the_tool() {
    assert!(AionOperationTool::from_runtime_env(&[]).is_none());
    assert!(AionOperationTool::from_runtime_env(&[("AIONUI_HELPER_BIN".into(), " ".into())]).is_none());
}

#[tokio::test]
async fn state_changes_require_explicit_confirmation() {
    let tool = AionOperationTool::from_runtime_env(&[("AIONUI_HELPER_BIN".into(), "missing-helper".into())])
        .unwrap()
        .with_contract(Some(fixture_contract()));
    let result = tool
        .execute(json!({
            "operation": "config.assistants.delete",
            "input": { "assistant_id": "assistant-1" }
        }))
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("aion_operation_confirmation_required"));
}

#[tokio::test]
async fn read_operation_returns_structured_spawn_error() {
    let tool = AionOperationTool::from_runtime_env(&[("AIONUI_HELPER_BIN".into(), "missing-helper".into())])
        .unwrap()
        .with_contract(Some(fixture_contract()));
    let result = tool
        .execute(json!({
            "operation": "config.assistants.list",
            "input": {}
        }))
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("aion_operation_spawn_failed"));
    assert!(result.content.contains("config.assistants.list"));
}

#[tokio::test]
async fn helper_receives_stdin_and_returns_a_normalized_envelope() {
    let temp = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let helper = {
        let path = temp.path().join("helper.cmd");
        fs::write(&path, "@echo off\r\necho {\"success\":true,\"data\":{\"ok\":true}}\r\n").unwrap();
        path
    };
    #[cfg(not(windows))]
    let helper = {
        use std::os::unix::fs::PermissionsExt;
        let path = temp.path().join("helper.sh");
        fs::write(
            &path,
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"success\":true,\"data\":{\"ok\":true}}'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    };

    let tool =
        AionOperationTool::from_runtime_env(&[("AIONUI_HELPER_BIN".into(), helper.to_string_lossy().into_owned())])
            .unwrap()
            .with_contract(Some(fixture_contract()));
    let result = tool
        .execute(json!({
            "operation": "config.context",
            "input": {"requested": true}
        }))
        .await;

    assert!(!result.is_error, "unexpected helper failure: {}", result.content);
    assert!(result.content.contains("\"success\": true"));
    assert!(result.content.contains("\"ok\": true"));
}

#[test]
fn helper_envelopes_are_normalized_without_dropping_data_or_error_codes() {
    let success = AionOperationTool::normalize_output(
        "config.context",
        br#"{"success":true,"data":{"conversation_id":"conv-1"},"meta":{"schema_version":1}}"#,
        &[],
        Some(0),
    );
    assert!(!success.is_error);
    assert!(success.content.contains("conversation_id"));
    assert!(success.content.contains("config.context"));

    let failure = AionOperationTool::normalize_output(
        "config.assistants.delete",
        br#"{"success":false,"error":{"code":"NOT_FOUND","message":"missing"}}"#,
        &[],
        Some(2),
    );
    assert!(failure.is_error);
    assert!(failure.content.contains("NOT_FOUND"));
    assert!(failure.content.contains("config.assistants.delete"));
}

#[test]
fn invalid_helper_output_has_a_stable_error_code() {
    let result = AionOperationTool::normalize_output("config.context", b"not-json", b"stderr", Some(0));
    assert!(result.is_error);
    assert!(result.content.contains("aion_operation_invalid_output"));
}

#[tokio::test]
async fn missing_or_unsupported_contract_is_a_host_failure_without_spawning() {
    let tool = AionOperationTool::from_runtime_env(&[("AIONUI_HELPER_BIN".into(), "must-not-run".into())]).unwrap();
    let input = json!({"operation":"config.mcp.servers.get","input":{"server_id":"fixture"}});
    let result = tool.preflight(&input).unwrap_err();
    let value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(value["code"], "aion_operation_contract_unavailable");
    assert_eq!(value["execution"]["status"], "not_executed");
    assert!(!result.content.contains("Regenerate"));
    let tool = tool.with_contract(Some(crate::aion_operation_contract::OperationContract::new(
        json!({"contract_version":99}),
    )));
    let result = tool.execute(input).await;
    assert!(result.content.contains("aion_operation_contract_version_unsupported"));
    let result = tool
        .execute(json!({"operation":"result.read","input":{"result_id":"expired"}}))
        .await;
    assert!(result.content.contains("aion_operation_result_expired"));
}

#[test]
fn incomplete_or_unsupported_schema_contract_is_rejected_as_host_configuration() {
    for schema in [
        json!({"type":"object","patternProperties":{}}),
        json!({"type":"unrecognized"}),
    ] {
        let mut contract = json!({"contract_version":1,"operations":OPERATION_PATHS.iter().map(|name|json!({"operation":name,"input_schema":{"type":"object"},"requires_confirmation":false})).collect::<Vec<_>>()});
        contract["operations"][0]["input_schema"] = schema;
        let tool = AionOperationTool::from_runtime_env(&[("AIONUI_HELPER_BIN".into(), "never-run".into())])
            .unwrap()
            .with_contract(Some(crate::aion_operation_contract::OperationContract::new(contract)));
        let result = tool.preflight(&json!({"operation":"config.context"})).unwrap_err();
        assert!(result.content.contains("aion_operation_contract_invalid"));
        assert!(!result.content.contains("invalid_tool_arguments"));
    }
}

#[test]
fn stderr_only_legacy_failures_and_conflicting_success_are_not_business_successes() {
    let result = AionOperationTool::normalize_output(
        "config.mcp.servers.get",
        b"",
        b"CONFIG_PAYLOAD_INVALID field=server_id",
        Some(2),
    );
    let value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(value["code"], "aion_operation_helper_failed");
    assert_eq!(value["execution"]["status"], "unknown");
    let result = AionOperationTool::normalize_output("config.context", br#"{"success":true,"data":{}}"#, b"", Some(2));
    assert!(result.is_error);
    assert!(result.content.contains("aion_operation_invalid_output"));
}

#[test]
fn short_errors_are_complete_and_do_not_page_null_data() {
    let tool = AionOperationTool::from_runtime_env(&[("AIONUI_HELPER_BIN".into(), "fixture".into())]).unwrap();
    let raw=AionOperationTool::normalize_output("config.mcp.servers.get",br#"{"success":false,"error":{"code":"CONFIG_PAYLOAD_INVALID","details":{"path":"/server_id"}},"execution":{"status":"not_executed","stage":"preflight"}}"#,b"",Some(2));
    for budget in [512, 1024, 2048, 10000] {
        let result = tool
            .format_result(&json!({"operation":"config.mcp.servers.get"}), &raw, budget)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(result.content.len() <= budget);
        assert_eq!(value["delivery"]["complete"], true);
        assert_eq!(value["delivery"]["execution_status"], "not_executed");
        assert!(value["delivery"].get("result_id").is_none());
        assert!(value["delivery"].get("state_basis").is_none());
    }
}

#[tokio::test]
async fn describes_local_meta_operations_without_starting_helper_or_requiring_host_contract() {
    let tool = AionOperationTool::from_runtime_env(&[("AIONUI_HELPER_BIN".into(), "never-run".into())]).unwrap();
    for operation in ["result.read", "operation.describe"] {
        let result = tool
            .execute(json!({"operation":"operation.describe","input":{"operation":operation}}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        let value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(value["data"]["operation"], operation);
        assert_eq!(value["data"]["requires_confirmation"], false);
        assert!(value["data"]["input_schema"]["required"].is_array());
    }
}
