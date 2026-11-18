mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::output::OutputSink;
use aion_agent::tool_policy::ToolPolicy;
use aion_providers::{LlmProvider, ProviderError};
use aion_types::llm::{LlmEvent, LlmRequest};
use aion_types::message::{ContentBlock, StopReason, TokenUsage};
use aion_types::subagent::{SubagentChange, SubagentEvent, SubagentStatus};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

#[derive(Default)]
struct Observer {
    children: Mutex<Vec<SubagentEvent>>,
    text: Mutex<String>,
    ends: AtomicUsize,
}
impl OutputSink for Observer {
    fn emit_text_delta(&self, text: &str, _: &str) {
        self.text.lock().unwrap().push_str(text);
    }
    fn emit_thinking(&self, _: &str, _: &str) {}
    fn emit_tool_call(&self, _: &str, _: &str, _: &str) {}
    fn emit_tool_result(&self, _: &str, _: &str, _: bool, _: &str) {}
    fn emit_stream_start(&self, _: &str) {}
    fn emit_stream_end(&self, _: &str, _: usize, _: u64, _: u64, _: u64, _: u64) {
        self.ends.fetch_add(1, Ordering::SeqCst);
    }
    fn emit_error(&self, _: &str) {}
    fn emit_info(&self, _: &str) {}
    fn emit_subagent(&self, event: SubagentEvent) {
        self.children.lock().unwrap().push(event);
    }
}
struct Provider {
    parent_turn: AtomicUsize,
    cancel: bool,
}
fn done(reason: StopReason) -> LlmEvent {
    LlmEvent::Done {
        stop_reason: reason,
        usage: TokenUsage::default(),
    }
}
#[async_trait]
impl LlmProvider for Provider {
    async fn stream(&self, request: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        let parent = request.tools.iter().any(|tool| tool.name == "Spawn");
        let mut events = Vec::new();
        if parent {
            let turn = self.parent_turn.fetch_add(1, Ordering::SeqCst);
            if turn < if self.cancel { 1 } else { 2 } {
                let tasks = if self.cancel {
                    json!([{"name":"same","prompt":"cancel"}])
                } else {
                    json!([{"name":"same","prompt":"normal"},{"name":"same","prompt":"limited"},{"name":"same","prompt":"panic"}])
                };
                events.push(LlmEvent::ToolUse {
                    id: format!("batch-{turn}"),
                    name: "Spawn".into(),
                    input: json!({"tasks":tasks}),
                    extra: None,
                });
                events.push(done(StopReason::ToolUse));
            } else {
                events.extend([LlmEvent::TextDelta("parent done".into()), done(StopReason::EndTurn)]);
            }
        } else {
            let prompt = request
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let after_tool = request
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
            if prompt.contains("panic") {
                panic!("intentional child panic");
            }
            if prompt.contains("limited") {
                events.extend([LlmEvent::TextDelta("partial".into()), done(StopReason::MaxTokens)]);
            } else if after_tool {
                events.extend([LlmEvent::TextDelta("child final".into()), done(StopReason::EndTurn)]);
            } else {
                events.extend([
                    LlmEvent::ThinkingDelta("child thinking".into()),
                    LlmEvent::TextDelta("child before tool".into()),
                ]);
                let (name, input) = if self.cancel {
                    (
                        "ExecCommand",
                        json!({"cmd":"while ($true) { [IO.File]::AppendAllText((Join-Path (Get-Location) 'heartbeat.txt'), 'x'); Start-Sleep -Milliseconds 30 }", "shell":"powershell", "timeout":60000}),
                    )
                } else {
                    ("Read", json!({"file_path":"sample.txt"}))
                };
                events.push(LlmEvent::ToolUse {
                    id: "child-tool".into(),
                    name: name.into(),
                    input,
                    extra: None,
                });
                events.push(done(StopReason::ToolUse));
            }
        }
        let (tx, rx) = mpsc::channel(16);
        for event in events {
            tx.try_send(event).unwrap();
        }
        Ok(rx)
    }
}

#[tokio::test]
async fn actual_spawn_dispatch_isolates_segments_batches_and_panic() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.txt"), "fixture content").unwrap();
    let observer = Arc::new(Observer::default());
    let mut engine = AgentBootstrap::new(common::test_config(), dir.path().to_string_lossy(), observer.clone())
        .provider(Arc::new(Provider {
            parent_turn: AtomicUsize::new(0),
            cancel: false,
        }))
        .tool_policy(ToolPolicy::allow_only(["Spawn", "Read"]))
        .build()
        .await
        .unwrap()
        .engine;
    let result = engine.run("start", "parent").await.unwrap();
    engine.drain_subagents().await;
    assert_eq!(result.text, "parent done");
    assert_eq!(*observer.text.lock().unwrap(), "parent done");
    let events = observer.children.lock().unwrap();
    let starts: Vec<_> = events
        .iter()
        .filter(|event| matches!(event.change, SubagentChange::Started))
        .collect();
    assert_eq!(starts.len(), 6);
    let ids: std::collections::HashSet<_> = starts.iter().map(|event| &event.run.child_run_id).collect();
    assert_eq!(ids.len(), 6);
    for batch in ["batch-0", "batch-1"] {
        for index in 0..3 {
            let start = starts
                .iter()
                .find(|e| e.run.parent_call_id == batch && e.run.task_index == index)
                .unwrap();
            let own: Vec<_> = events
                .iter()
                .filter(|e| e.run.child_run_id == start.run.child_run_id)
                .collect();
            let end = own
                .iter()
                .filter(|e| matches!(e.change, SubagentChange::Finished { .. }))
                .collect::<Vec<_>>();
            assert_eq!(end.len(), 1);
            if index == 0 {
                assert!(matches!(
                    end[0].change,
                    SubagentChange::Finished {
                        status: SubagentStatus::Completed,
                        ..
                    }
                ));
                assert_eq!(
                    own.iter()
                        .filter(|e| matches!(e.change, SubagentChange::Text { .. }))
                        .count(),
                    3
                );
                assert!(
                    own.iter()
                        .any(|e| matches!(&e.change, SubagentChange::ToolResult { name, .. } if name == "Read"))
                );
                let entries: std::collections::HashSet<_> = own
                    .iter()
                    .filter_map(|e| match &e.change {
                        SubagentChange::Text { entry_id, .. } => Some(entry_id),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    entries.len(),
                    3,
                    "thinking and both model turns must have distinct segments"
                );
            } else {
                assert!(matches!(
                    end[0].change,
                    SubagentChange::Finished {
                        status: SubagentStatus::Failed,
                        ..
                    }
                ));
                let reason = match &end[0].change {
                    SubagentChange::Finished { text, .. } => text,
                    _ => unreachable!(),
                };
                assert!(
                    reason.contains(if index == 1 { "MaxTokens" } else { "panicked" }),
                    "{reason}"
                );
            }
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn cancelling_parent_drains_child_and_stops_actual_command() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("heartbeat.txt");
    let observer = Arc::new(Observer::default());
    let mut engine = AgentBootstrap::new(common::test_config(), dir.path().to_string_lossy(), observer.clone())
        .provider(Arc::new(Provider {
            parent_turn: AtomicUsize::new(0),
            cancel: true,
        }))
        .tool_policy(ToolPolicy::allow_only(["Spawn", "ExecCommand"]))
        .build()
        .await
        .unwrap()
        .engine;
    {
        let running = engine.run("start", "parent");
        tokio::pin!(running);
        tokio::select! {
            _ = &mut running => panic!("long child unexpectedly finished"),
            _ = async {
                tokio::time::timeout(Duration::from_secs(15), async {
                    while !marker.exists() { tokio::time::sleep(Duration::from_millis(30)).await; }
                }).await.expect("real command must write before cancellation");
            } => {}
        }
    }
    tokio::time::timeout(Duration::from_secs(5), engine.drain_subagents())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let length = std::fs::metadata(&marker).unwrap().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        std::fs::metadata(&marker).unwrap().len(),
        length,
        "the external command must stop writing"
    );
    let events = observer.children.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e.change,
                SubagentChange::Finished {
                    status: SubagentStatus::Cancelled,
                    ..
                }
            ))
            .count(),
        1
    );
}
