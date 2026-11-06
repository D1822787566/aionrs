//! Regressions for strict argument validation and model-driven correction.

mod common;

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::engine::AgentEngine;
use aion_agent::host_skills::{HostSkill, HostSkillContent, HostSkillReadError, HostSkillSource};
use aion_agent::orchestration::execute_tool_calls;
use aion_agent::output::OutputSink;
use aion_agent::output::terminal::TerminalSink;
use aion_compact::CompactLevel;
use aion_protocol::ToolApprovalManager;
use aion_protocol::events::{ProtocolEvent, ToolCategory};
use aion_protocol::writer::ProtocolEmitter;
use aion_providers::{LlmProvider, ProviderError};
use aion_tools::Tool;
use aion_tools::registry::ToolRegistry;
use aion_tools::tool_search::ToolSearchTool;
use aion_types::llm::{LlmEvent, LlmRequest};
use aion_types::message::{ContentBlock, StopReason, TokenUsage};
use aion_types::tool::ToolResult;
use common::{auto_approve_confirmer, test_config};

fn silent_output() -> Arc<dyn OutputSink> {
    Arc::new(TerminalSink::new(true))
}

struct TaskTool {
    executions: Arc<AtomicUsize>,
    deferred: bool,
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "TaskTool"
    }

    fn description(&self) -> &str {
        "Run independent tasks"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object", "required": ["tasks"], "additionalProperties": false,
            "properties": {"tasks": {
                "type": "array", "minItems": 1,
                "items": {
                    "type": "object", "required": ["name", "prompt"], "additionalProperties": false,
                    "properties": {
                        "name": {"type": "string"}, "prompt": {"type": "string"},
                        "role": {"type": "string", "enum": ["reader", "reviewer"]}
                    }
                }
            }}
        })
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn is_deferred(&self) -> bool {
        self.deferred
    }

    async fn execute(&self, _: Value) -> ToolResult {
        self.executions.fetch_add(1, Ordering::SeqCst);
        ToolResult {
            content: "TASK_OK".to_string(),
            is_error: false,
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Exec
    }
}

#[derive(Default)]
struct RecordingProvider {
    turns: Mutex<VecDeque<Vec<LlmEvent>>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl RecordingProvider {
    fn new(turns: Vec<Vec<LlmEvent>>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            ..Self::default()
        }
    }
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    async fn stream(&self, request: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        let events = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| final_turn("done"));
        let (tx, rx) = mpsc::channel(64);
        for event in events {
            tx.try_send(event).unwrap();
        }
        Ok(rx)
    }
}

#[derive(Default)]
struct EventRecorder(Mutex<Vec<Value>>);

impl ProtocolEmitter for EventRecorder {
    fn emit(&self, event: &ProtocolEvent) -> io::Result<()> {
        self.0.lock().unwrap().push(serde_json::to_value(event)?);
        Ok(())
    }
}

fn final_turn(text: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::TextDelta(text.to_string()),
        LlmEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        },
    ]
}

fn call(id: &str, name: &str, input: Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input,
        extra: None,
    }
}

fn tool_turn(id: &str, name: &str, input: Value) -> Vec<LlmEvent> {
    vec![
        LlmEvent::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
            extra: None,
        },
        LlmEvent::Done {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        },
    ]
}

fn valid_tasks() -> Value {
    json!({"tasks": [{"name": "ping", "prompt": "Reply TASK_OK"}]})
}

fn registry(executions: &Arc<AtomicUsize>, deferred: bool) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(TaskTool {
        executions: Arc::clone(executions),
        deferred,
    }));
    tools.register(Box::new(ToolSearchTool::new(tools.to_tool_defs())));
    tools
}

fn last_error(request: &LlmRequest, id: &str) -> Value {
    request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: true,
            } if tool_use_id == id => serde_json::from_str(content).ok(),
            _ => None,
        })
        .expect("model must receive a structured validation error tied to the original call")
}

#[tokio::test]
async fn invalid_arguments_are_rejected_before_tool_execution() {
    let executions = Arc::new(AtomicUsize::new(0));
    let tools = registry(&executions, false);
    let invalid = [
        json!({"tasks": "[{\"name\":\"ping\",\"prompt\":\"TASK_OK\"}]"}),
        json!({"tasks": [{"name": "missing-prompt"}]}),
        json!({"tasks": [{"name": "a", "prompt": 7}]}),
        json!({"tasks": [{"name": "a", "prompt": "ok", "role": "writer"}]}),
        json!({"tasks": [{"name": "a", "prompt": "ok", "unexpected": true}]}),
        json!({"tasks": []}),
        json!({}),
    ];
    for input in invalid {
        let calls = vec![call("bad", "TaskTool", input.clone())];
        let outcome = execute_tool_calls(
            &tools,
            &calls,
            &auto_approve_confirmer(),
            None,
            CompactLevel::default(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            executions.load(Ordering::SeqCst),
            0,
            "invalid input must never execute: {input}"
        );
        let ContentBlock::ToolResult { content, is_error, .. } = &outcome.results[0] else {
            panic!("missing result")
        };
        assert!(*is_error);
        let error: Value = serde_json::from_str(content).unwrap();
        assert_eq!(error["code"], "invalid_tool_arguments");
        assert_eq!(error["executed"], false);
        assert!(error["errors"].as_array().is_some_and(|e| !e.is_empty()));
    }
}

#[tokio::test]
async fn model_receives_type_error_and_corrected_call_executes_once() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_turn("bad", "TaskTool", json!({"tasks": "[]"})),
        tool_turn("fixed", "TaskTool", valid_tasks()),
        final_turn("done"),
    ]));
    let mut engine = AgentEngine::new_with_provider(
        provider.clone(),
        test_config(),
        registry(&executions, false),
        silent_output(),
        std::env::temp_dir(),
    );
    engine.run("Run tasks", "test").await.unwrap();
    let requests = provider.requests.lock().unwrap();
    let error = last_error(&requests[1], "bad");
    assert_eq!(error["errors"][0]["path"], "/tasks");
    assert_eq!(error["errors"][0]["expected"], "array");
    assert_eq!(error["errors"][0]["actual"], "string");
    assert_eq!(error["executed"], false);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn successful_search_exposes_full_schema_on_the_next_request() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_turn("search", "ToolSearch", json!({"query": "TaskTool"})),
        final_turn("done"),
    ]));
    let mut engine = AgentEngine::new_with_provider(
        provider.clone(),
        test_config(),
        registry(&executions, true),
        silent_output(),
        std::env::temp_dir(),
    );
    engine.run("Find the task tool", "test").await.unwrap();
    let requests = provider.requests.lock().unwrap();
    assert!(
        requests[0]
            .tools
            .iter()
            .find(|t| t.name == "TaskTool")
            .unwrap()
            .deferred
    );
    let loaded = requests[1].tools.iter().find(|t| t.name == "TaskTool").unwrap();
    assert!(
        !loaded.deferred,
        "discovered schema must become a formal tool definition"
    );
    assert_eq!(loaded.input_schema["properties"]["tasks"]["type"], "array");
}

#[tokio::test]
async fn changing_task_text_and_searching_does_not_reset_type_error_budget() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_turn("bad1", "TaskTool", json!({"tasks": "first"})),
        tool_turn("search1", "ToolSearch", json!({"query": "TaskTool"})),
        tool_turn("bad2", "TaskTool", json!({"tasks": "second"})),
        tool_turn("search2", "ToolSearch", json!({"query": "TaskTool"})),
        tool_turn("bad3", "TaskTool", json!({"tasks": "third"})),
        final_turn("blocked"),
    ]));
    let mut engine = AgentEngine::new_with_provider(
        provider.clone(),
        test_config(),
        registry(&executions, true),
        silent_output(),
        std::env::temp_dir(),
    );
    engine.run("Run tasks", "test").await.unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(
        requests[5].tools.is_empty(),
        "third argument failure must enter tool-free finalization"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mixed_calls_preserve_valid_siblings_and_original_result_ids() {
    let executions = Arc::new(AtomicUsize::new(0));
    let tools = registry(&executions, false);
    let calls = vec![
        call("valid", "TaskTool", valid_tasks()),
        call("invalid", "TaskTool", json!({"tasks": "[]"})),
    ];
    let outcome = execute_tool_calls(
        &tools,
        &calls,
        &auto_approve_confirmer(),
        None,
        CompactLevel::default(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(
        outcome.results.iter().any(
            |r| matches!(r, ContentBlock::ToolResult {tool_use_id, is_error: false, ..} if tool_use_id == "valid")
        )
    );
    assert!(
        outcome.results.iter().any(
            |r| matches!(r, ContentBlock::ToolResult {tool_use_id, is_error: true, ..} if tool_use_id == "invalid")
        )
    );
}

#[tokio::test]
async fn invalid_arguments_do_not_request_protocol_approval() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_turn("bad", "TaskTool", json!({"tasks": "[]"})),
        final_turn("done"),
    ]));
    let mut config = test_config();
    config.tools.auto_approve = false;
    let mut engine = AgentEngine::new_with_provider(
        provider,
        config,
        registry(&executions, false),
        silent_output(),
        std::env::temp_dir(),
    );
    let events = Arc::new(EventRecorder::default());
    engine.set_approval_manager(Arc::new(ToolApprovalManager::new()));
    engine.set_protocol_writer(events.clone());
    timeout(Duration::from_secs(1), engine.run("Run tasks", "test"))
        .await
        .expect("invalid input must not wait for approval")
        .unwrap();
    let recorded = events.0.lock().unwrap();
    assert!(
        !recorded
            .iter()
            .any(|e| e["type"] == "tool_request" || e["type"] == "tool_running")
    );
    assert!(
        recorded
            .iter()
            .any(|e| e["type"] == "tool_result" && e["call_id"] == "bad" && e["metadata"]["executed"] == false)
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn builtin_spawn_is_exposed_without_search_and_runs_a_real_child_engine() {
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_turn(
            "spawn",
            "Spawn",
            json!({"tasks": [{"name": "ping", "prompt": "Reply SUBAGENT_OK"}]}),
        ),
        final_turn("SUBAGENT_OK"),
        final_turn("done"),
    ]));
    let dir = tempfile::tempdir().unwrap();
    let mut result = AgentBootstrap::new(
        test_config(),
        dir.path().to_string_lossy().into_owned(),
        silent_output(),
    )
    .provider(provider.clone())
    .isolated_skill_dirs(vec![])
    .build()
    .await
    .unwrap();
    result.engine.run("Use a subagent", "test").await.unwrap();
    let requests = provider.requests.lock().unwrap();
    let spawn = requests[0].tools.iter().find(|t| t.name == "Spawn").unwrap();
    assert!(!spawn.deferred);
    assert_eq!(requests.len(), 3, "parent request, child request, parent continuation");
    assert!(requests[2].messages.iter().flat_map(|m| &m.content).any(|b| matches!(b, ContentBlock::ToolResult {tool_use_id, content, is_error: false} if tool_use_id == "spawn" && content.contains("SUBAGENT_OK"))));
}

// Recovery must cross the engine boundary, not only classify a string.
#[tokio::test]
async fn textual_call_is_reissued_formally_and_executes_once() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(RecordingProvider::new(vec![
        final_turn("<tool_call><function=TaskTool></function></tool_call>"),
        tool_turn("fixed", "TaskTool", valid_tasks()),
        final_turn("done"),
    ]));
    let mut engine = AgentEngine::new_with_provider(
        provider.clone(),
        test_config(),
        registry(&executions, false),
        silent_output(),
        std::env::temp_dir(),
    );
    engine.run("Run the tasks", "test").await.unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b,ContentBlock::Text {text} if text.contains("NOT executed")))
    );
}

#[tokio::test]
async fn repeated_textual_calls_stop_after_two_corrections() {
    let executions = Arc::new(AtomicUsize::new(0));
    let markup = "<tool_call><function=TaskTool></function></tool_call>";
    let provider = Arc::new(RecordingProvider::new(vec![
        final_turn(markup),
        final_turn(markup),
        final_turn(markup),
        final_turn("should not be requested"),
    ]));
    let mut engine = AgentEngine::new_with_provider(
        provider.clone(),
        test_config(),
        registry(&executions, false),
        silent_output(),
        std::env::temp_dir(),
    );
    let result = engine.run("Run tasks", "test").await.unwrap();
    assert!(result.text.contains("not completed"));
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn decoding_error_is_feedback_text_without_orphan_results_and_valid_calls_continue() {
    use aion_types::llm::InvalidToolInput;
    let executions = Arc::new(AtomicUsize::new(0));
    let mut mixed = tool_turn("valid", "TaskTool", valid_tasks());
    mixed.insert(
        0,
        LlmEvent::InvalidToolInput(InvalidToolInput {
            id: "bad-json".into(),
            name: "TaskTool".into(),
            line: 1,
            column: 2,
        }),
    );
    let provider = Arc::new(RecordingProvider::new(vec![mixed, final_turn("done")]));
    let recorder = Arc::new(EventRecorder::default());
    let mut engine = AgentEngine::new_with_provider(
        provider.clone(),
        test_config(),
        registry(&executions, false),
        silent_output(),
        std::env::temp_dir(),
    );
    engine.set_protocol_writer(recorder.clone());
    engine.run("Run tasks", "test").await.unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let requests = provider.requests.lock().unwrap();
    let blocks: Vec<_> = requests[1].messages.iter().flat_map(|m| &m.content).collect();
    assert!(
        blocks.iter().any(
            |b| matches!(b,ContentBlock::Text {text} if text.contains("invalid_json") && text.contains("bad-json"))
        )
    );
    assert!(
        !blocks
            .iter()
            .any(|b| matches!(b,ContentBlock::ToolResult {tool_use_id,..} if tool_use_id=="bad-json"))
    );
    assert!(
        !blocks
            .iter()
            .any(|b| matches!(b,ContentBlock::ToolUse {id,..} if id=="bad-json"))
    );
    assert!(
        recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|e| e["call_id"] == "bad-json" && e["metadata"]["executed"] == false)
    );
}

#[derive(Debug)]
struct TestHostSource;

#[async_trait]
impl HostSkillSource for TestHostSource {
    fn available(&self) -> Vec<HostSkill> {
        vec![HostSkill {
            name: "selected-form".into(),
            description: "Selected form".into(),
        }]
    }
    async fn read(&self, name: &str) -> Result<HostSkillContent, HostSkillReadError> {
        if name != "selected-form" {
            return Err(HostSkillReadError::NotEnabled);
        }
        Ok(HostSkillContent {
            root: std::path::PathBuf::from("selected-root"),
            body: "REAL_FORM_INSTRUCTIONS $ARGUMENTS".into(),
        })
    }
}

#[tokio::test]
async fn bootstrap_uses_one_hosted_skill_and_actual_body_instead_of_native_discovery() {
    let provider = Arc::new(RecordingProvider::new(vec![
        tool_turn("skill", "Skill", json!({"skill":"selected-form"})),
        tool_turn(
            "spawn-hosted",
            "Spawn",
            json!({"tasks":[{"name":"child","prompt":"Read skills only if needed"}]}),
        ),
        final_turn("child done"),
        final_turn("done"),
    ]));
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join(".aionrs/skills/unselected");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("SKILL.md"),
        "---\nname: unselected\ndescription: local\n---\nNOT_SELECTED",
    )
    .unwrap();
    let mut result = AgentBootstrap::new(
        test_config(),
        dir.path().to_string_lossy().into_owned(),
        silent_output(),
    )
    .provider(provider.clone())
    .host_skills(Arc::new(TestHostSource))
    .build()
    .await
    .unwrap();
    assert_eq!(result.engine.skill_names(), &["selected-form".to_string()]);
    result
        .engine
        .run("Read selected form instructions", "test")
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests[0].tools.iter().filter(|t| t.name == "Skill").count(), 1);
    assert!(!requests[0].system.contains("NOT_SELECTED"));
    assert_eq!(requests.len(), 4);
    assert!(!requests[2].tools.iter().any(|tool| tool.name == "Skill"));
    assert!(requests[2].system.contains("This basic child has no Skill tool"));
    assert!(
        !requests[2]
            .system
            .contains("Use Skill to read these conversation-selected instructions")
    );
    assert!(requests[1].messages.iter().flat_map(|m| &m.content).any(|b|matches!(b,ContentBlock::Text {text} if text.contains("REAL_FORM_INSTRUCTIONS $ARGUMENTS") && text.contains("selected-root"))));
}

#[tokio::test]
async fn explicit_protocol_example_is_not_retried_or_executed() {
    let executions = Arc::new(AtomicUsize::new(0));
    let markup = "<tool_call><function=TaskTool></function></tool_call>";
    let provider = Arc::new(RecordingProvider::new(vec![final_turn(markup)]));
    let mut engine = AgentEngine::new_with_provider(
        provider.clone(),
        test_config(),
        registry(&executions, false),
        silent_output(),
        std::env::temp_dir(),
    );
    let result = engine.run("请展示工具调用格式的示例", "test").await.unwrap();
    assert_eq!(result.text, markup);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn changing_decode_error_ids_and_positions_does_not_reset_failure_budget() {
    use aion_types::llm::InvalidToolInput;
    let executions = Arc::new(AtomicUsize::new(0));
    let mut turns = Vec::new();
    for column in 1..=3 {
        turns.push(vec![
            LlmEvent::InvalidToolInput(InvalidToolInput {
                id: format!("bad-{column}"),
                name: "TaskTool".into(),
                line: column,
                column,
            }),
            LlmEvent::Done {
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            },
        ]);
        if column < 3 {
            turns.push(tool_turn("search", "ToolSearch", json!({"query":"TaskTool"})));
        }
    }
    turns.push(final_turn("The requested operation was not completed."));
    let provider = Arc::new(RecordingProvider::new(turns));
    let mut engine = AgentEngine::new_with_provider(
        provider.clone(),
        test_config(),
        registry(&executions, false),
        silent_output(),
        std::env::temp_dir(),
    );
    engine.run("Run tasks", "test").await.unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(
        requests.last().unwrap().tools.is_empty(),
        "budget must force finalization after three errors despite search successes"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}
