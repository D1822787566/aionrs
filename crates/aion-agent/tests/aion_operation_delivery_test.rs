mod common;

use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::orchestration::{execute_tool_calls, execute_tool_calls_with_approval};
use aion_agent::output::null_sink::NullSink;
use aion_agent::session::SessionManager;
use aion_compact::CompactLevel;
use aion_protocol::events::ProtocolEvent;
use aion_protocol::writer::ProtocolEmitter;
use aion_protocol::{ToolApprovalManager, ToolApprovalResult};
use aion_providers::{LlmProvider, ProviderError};
use aion_tools::aion_operation::{AionOperationTool, OPERATION_PATHS};
use aion_tools::registry::ToolRegistry;
use aion_types::llm::{LlmEvent, LlmRequest};
use aion_types::message::ContentBlock;
use async_trait::async_trait;
use common::{MockLlmProvider, auto_approve_confirmer, test_config};
use serde_json::{Value, json};
use std::fs;
use std::sync::{Arc, Mutex};
use tempfile::{TempDir, tempdir};
use tokio::sync::mpsc;

// These are output-protocol fixtures; production input semantics are covered by
// the Core suite, which injects the real catalog and invokes the real helper.
fn fixture_contract() -> Value {
    json!({"contract_version":1,"operations":OPERATION_PATHS.iter().map(|name|json!({"operation":name,"input_schema":{"type":"object"},"requires_confirmation":false})).collect::<Vec<_>>()})
}

fn fixture(response: Value) -> (TempDir, ToolRegistry) {
    let dir = tempdir().unwrap();
    let response_path = dir.path().join("response.json");
    let count_path = dir.path().join("calls.txt");
    fs::write(&response_path, serde_json::to_vec(&response).unwrap()).unwrap();
    #[cfg(windows)]
    let helper = {
        let path = dir.path().join("helper.cmd");
        fs::write(
            &path,
            "@echo off\r\necho x>>\"%FIXTURE_COUNT%\"\r\ntype \"%FIXTURE_RESPONSE%\"\r\n",
        )
        .unwrap();
        path
    };
    #[cfg(not(windows))]
    let helper = {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join("helper.sh");
        fs::write(
            &path,
            "#!/bin/sh\ncat >/dev/null\nprintf 'x\\n' >>\"$FIXTURE_COUNT\"\ncat \"$FIXTURE_RESPONSE\"\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let tool = AionOperationTool::from_runtime_env(&[
        ("AIONUI_HELPER_BIN".into(), helper.to_string_lossy().into_owned()),
        ("FIXTURE_RESPONSE".into(), response_path.to_string_lossy().into_owned()),
        ("FIXTURE_COUNT".into(), count_path.to_string_lossy().into_owned()),
    ])
    .unwrap()
    .with_contract(Some(aion_tools::aion_operation_contract::OperationContract::new(
        fixture_contract(),
    )));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(tool));
    (dir, registry)
}

async fn call(registry: &ToolRegistry, operation: &str, input: Value) -> Value {
    call_mode(registry, operation, input, CompactLevel::Safe, false, false).await
}

struct Events;
impl ProtocolEmitter for Events {
    fn emit(&self, _: &ProtocolEvent) -> std::io::Result<()> {
        Ok(())
    }
}

async fn call_mode(
    registry: &ToolRegistry,
    operation: &str,
    input: Value,
    level: CompactLevel,
    toon: bool,
    protocol: bool,
) -> Value {
    let calls = vec![ContentBlock::ToolUse {
        id: "fixture-call".into(),
        name: "AionOperation".into(),
        input: json!({"operation":operation,"input":input,"confirm":true}),
        extra: None,
    }];
    let outcome = if protocol {
        let writer: Arc<dyn ProtocolEmitter> = Arc::new(Events);
        execute_tool_calls_with_approval(
            registry,
            &calls,
            &Arc::new(ToolApprovalManager::new()),
            &writer,
            "test",
            true,
            &[],
            None,
            level,
            toon,
        )
        .await
        .unwrap()
    } else {
        execute_tool_calls(registry, &calls, &auto_approve_confirmer(), None, level, toon)
            .await
            .unwrap()
    };
    let ContentBlock::ToolResult { content, .. } = &outcome.results[0] else {
        panic!("missing tool result")
    };
    assert!(content.len() <= 10_000);
    serde_json::from_str(content).expect("final model-visible AionOperation result must be JSON")
}

#[tokio::test]
async fn successful_write_receipt_continuation_never_calls_helper_again() {
    let body = "state-after-write\n".repeat(3000);
    let (dir, registry) = fixture(
        json!({"success":true,"data":{"id":"created-resource"},"meta":{"before":"x".repeat(15000),"after":body}}),
    );
    let first = call(&registry, "config.mcp.servers.create", json!({"name":"fixture"})).await;
    assert_eq!(first["delivery"]["execution_status"], "succeeded");
    let id = first["delivery"]["result_id"].as_str().unwrap();
    // Changing the source proves the continuation uses the immutable snapshot.
    fs::write(dir.path().join("response.json"), b"not-json-anymore").unwrap();
    let mut offset = 0;
    let mut actual = String::new();
    for _ in 0..100 {
        let page = call(
            &registry,
            "result.read",
            json!({"result_id":id,"path":"/meta/after","offset":offset}),
        )
        .await;
        assert_eq!(page["success"], true);
        actual.push_str(page["data"].as_str().unwrap());
        let Some(next) = page["delivery"]["next_offset"].as_u64() else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(actual, body);
    assert_eq!(
        fs::read_to_string(dir.path().join("calls.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn all_whitelisted_operations_use_the_same_safe_output_path() {
    let (_dir, registry) =
        fixture(json!({"success":true,"data":{"body":"x".repeat(30000)},"meta":{"after":"y".repeat(15000)}}));
    for operation in OPERATION_PATHS {
        let page = call(&registry, operation, json!({})).await;
        assert_eq!(page["success"], true, "{operation}: {page}");
        assert!(page["delivery"]["result_id"].is_string(), "{operation}: {page}");
    }
}

#[tokio::test]
async fn structured_errors_survive_all_compaction_and_approval_modes() {
    let (_dir, registry) = fixture(
        json!({"success":false,"code":"FIXTURE_ERROR","error":{"message":"failure\n".repeat(5000)},"data":null}),
    );
    for level in [CompactLevel::Off, CompactLevel::Safe, CompactLevel::Full] {
        for toon in [false, true] {
            for protocol in [false, true] {
                let result = call_mode(&registry, "config.context", json!({}), level, toon, protocol).await;
                assert_eq!(result["success"], false);
                assert_eq!(result["code"], "FIXTURE_ERROR");
                assert_eq!(result["delivery"]["execution_status"], "unknown");
            }
        }
    }
}

fn runtime_env(dir: &TempDir) -> Vec<(String, String)> {
    let helper = dir.path().join(if cfg!(windows) { "helper.cmd" } else { "helper.sh" });
    vec![
        ("AIONUI_HELPER_BIN".into(), helper.to_string_lossy().into_owned()),
        (
            "FIXTURE_RESPONSE".into(),
            dir.path().join("response.json").to_string_lossy().into_owned(),
        ),
        (
            "FIXTURE_COUNT".into(),
            dir.path().join("calls.txt").to_string_lossy().into_owned(),
        ),
    ]
}

struct Capture {
    provider: MockLlmProvider,
    requests: Arc<Mutex<Vec<LlmRequest>>>,
}
#[async_trait]
impl LlmProvider for Capture {
    async fn stream(&self, request: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        self.provider.stream(request).await
    }
}

#[tokio::test]
async fn next_provider_request_and_saved_history_contain_valid_json() {
    for protocol in [false, true] {
        let (dir, _) = fixture(
            json!({"success":true,"data":[{"name":"fixture","tools":[{"input_schema":{"description":"x".repeat(20000)}}]}]}),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Capture {
            provider: MockLlmProvider::with_tool_use(
                "operation-1",
                "AionOperation",
                json!({"operation":"config.mcp.servers.list"}),
            ),
            requests: requests.clone(),
        };
        let mut config = test_config();
        config.compact.enabled = false;
        config.compact.compaction = CompactLevel::Full;
        config.compact.toon = true;
        config.session.enabled = true;
        config.session.directory = dir.path().join("sessions").to_string_lossy().into_owned();
        let manager = SessionManager::new(dir.path().join("sessions"), 5);
        let mut built = AgentBootstrap::new(config, dir.path().to_string_lossy(), Arc::new(NullSink))
            .runtime_env(runtime_env(&dir))
            .operation_contract(fixture_contract())
            .provider(Arc::new(provider))
            .build()
            .await
            .unwrap();
        built
            .engine
            .init_session("fixture", &dir.path().to_string_lossy(), Some("output-test"))
            .unwrap();
        if protocol {
            built.engine.set_approval_manager(Arc::new(ToolApprovalManager::new()));
            built.engine.set_protocol_writer(Arc::new(Events));
        }
        built.engine.run("List MCP servers", "").await.unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests.len() >= 2);
        let content = requests[1]
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                ContentBlock::ToolResult {
                    content,
                    is_error: false,
                    ..
                } => Some(content),
                _ => None,
            })
            .unwrap();
        let parsed: Value = serde_json::from_str(content).unwrap();
        assert_eq!(parsed["data"][0]["name"], "fixture");
        assert!(content.len() <= 10000);
        let saved = manager.load("output-test").unwrap();
        assert!(
            saved
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|b| matches!(b,ContentBlock::ToolResult {content:old,..} if old==content))
        );
    }
}

#[tokio::test]
async fn impossible_budget_stops_before_helper_execution() {
    let (dir, _) = fixture(json!({"success":true,"data":{}}));
    let mut config = test_config();
    config.compact.tool_output_max_bytes = 1;
    let mut built = AgentBootstrap::new(config, dir.path().to_string_lossy(), Arc::new(NullSink))
        .runtime_env(runtime_env(&dir))
        .operation_contract(fixture_contract())
        .provider(Arc::new(MockLlmProvider::with_tool_use(
            "small",
            "AionOperation",
            json!({"operation":"config.mcp.servers.create","confirm":true,"input":{}}),
        )))
        .build()
        .await
        .unwrap();
    let error = built.engine.run("fixture", "").await.unwrap_err();
    assert!(error.to_string().contains("needs at least"));
    assert!(!dir.path().join("calls.txt").exists());
}

struct ApprovalEvents(mpsc::UnboundedSender<String>);
impl ProtocolEmitter for ApprovalEvents {
    fn emit(&self, event: &ProtocolEvent) -> std::io::Result<()> {
        if let ProtocolEvent::ToolRequest { call_id, .. } = event {
            let _ = self.0.send(call_id.clone());
        }
        Ok(())
    }
}

#[tokio::test]
async fn approval_denial_stays_json_and_does_not_execute_helper() {
    let (dir, registry) = fixture(json!({"success":true,"data":{}}));
    let manager = Arc::new(ToolApprovalManager::new());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let writer: Arc<dyn ProtocolEmitter> = Arc::new(ApprovalEvents(tx));
    let calls = [ContentBlock::ToolUse {
        id: "deny".into(),
        name: "AionOperation".into(),
        input: json!({"operation":"config.mcp.servers.create","confirm":true}),
        extra: None,
    }];
    let execute = execute_tool_calls_with_approval(
        &registry,
        &calls,
        &manager,
        &writer,
        "test",
        false,
        &[],
        None,
        CompactLevel::Full,
        true,
    );
    let deny = async {
        let id = rx.recv().await.unwrap();
        manager.resolve(
            &id,
            ToolApprovalResult::Denied {
                reason: "fixture denied".into(),
            },
        );
    };
    let (result, _) = tokio::time::timeout(std::time::Duration::from_secs(5), async { tokio::join!(execute, deny) })
        .await
        .unwrap();
    let output = result.unwrap();
    let ContentBlock::ToolResult { content, is_error, .. } = &output.results[0] else {
        panic!("missing result")
    };
    assert!(is_error);
    let parsed: Value = serde_json::from_str(content).unwrap();
    assert_eq!(parsed["delivery"]["execution_status"], "not_executed");
    assert!(!dir.path().join("calls.txt").exists());
}

#[tokio::test]
#[ignore = "requires AION_OPERATION_SKILL_ROOT pointing to real built-in skills"]
async fn real_skill_bodies_can_be_reassembled_through_the_tool_chain() {
    let root = std::path::PathBuf::from(std::env::var_os("AION_OPERATION_SKILL_ROOT").unwrap());
    for name in ["officecli-word-form", "officecli-pitch-deck"] {
        let body = fs::read_to_string(root.join(name).join("SKILL.md")).unwrap();
        assert!(body.len() > 50000);
        let (_dir, registry) = fixture(json!({"success":true,"data":{"name":name,"body":body,"path":"fixture"}}));
        let first = call(&registry, "skills.show", json!({"name":name})).await;
        let id = first["delivery"]["result_id"].as_str().unwrap();
        let mut actual = String::new();
        let mut offset = 0;
        for _ in 0..200 {
            let page = call(
                &registry,
                "result.read",
                json!({"result_id":id,"path":"/data/body","offset":offset}),
            )
            .await;
            actual.push_str(page["data"].as_str().unwrap());
            let Some(next) = page["delivery"]["next_offset"].as_u64() else {
                break;
            };
            assert!(next > offset);
            offset = next;
        }
        assert_eq!(actual, body);
        println!("{name}: {} exact UTF-8 bytes reconstructed", body.len());
    }
}

#[tokio::test]
#[ignore = "requires AION_OPERATION_REAL_HELPER pointing to the local AionCore binary"]
async fn real_helper_capabilities_are_json_after_final_delivery() {
    let helper = std::env::var("AION_OPERATION_REAL_HELPER").unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(
        AionOperationTool::from_runtime_env(&[("AIONUI_HELPER_BIN".into(), helper)]).unwrap(),
    ));
    for operation in ["config.capabilities", "team.capabilities"] {
        for level in [CompactLevel::Off, CompactLevel::Safe, CompactLevel::Full] {
            for toon in [false, true] {
                let page = call_mode(&registry, operation, json!({}), level, toon, true).await;
                assert_eq!(page["success"], true);
                assert!(page["delivery"]["result_id"].is_string());
            }
        }
    }
}

#[tokio::test]
async fn large_mcp_list_stays_json_and_all_servers_are_recoverable() {
    let servers: Vec<_> = (0..35)
        .map(|i| {
            json!({
                "id":format!("server-{i}"),"name":format!("服务器-{i}"),"enabled":true,
                "tools":[{"name":"query","input_schema":{"description":"x".repeat(1500)}}]
            })
        })
        .collect();
    let (_dir, registry) = fixture(json!({"success":true,"data":servers}));
    let first = call(&registry, "config.mcp.servers.list", json!({})).await;
    assert_eq!(first["success"], true);
    assert_eq!(first["delivery"]["complete"], false);
    let id = first["delivery"]["result_id"].as_str().unwrap();
    let mut offset = 0;
    let mut names = Vec::new();
    loop {
        let page = call(
            &registry,
            "result.read",
            json!({"result_id":id,"path":"/data","offset":offset}),
        )
        .await;
        names.extend(
            page["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["name"].as_str().unwrap().to_owned()),
        );
        let Some(next) = page["delivery"]["next_offset"].as_u64() else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(names, (0..35).map(|i| format!("服务器-{i}")).collect::<Vec<_>>());
}
