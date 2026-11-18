use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use aion_types::subagent::{SubagentChange, SubagentEvent, SubagentIdentity, SubagentStatus};
use chrono::Utc;
use tokio::sync::Notify;

use crate::output::OutputSink;

/// Shared with the host so dropping a Spawn future can be followed by a drain.
#[derive(Default)]
pub(crate) struct SubagentTasks {
    active: AtomicUsize,
    finished: Notify,
}

impl SubagentTasks {
    pub(crate) async fn drain(&self) {
        loop {
            let notified = self.finished.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) struct ChildOutput {
    pub(crate) identity: SubagentIdentity,
    parent: Arc<dyn OutputSink>,
    segment: Mutex<(usize, Option<bool>)>,
}

impl ChildOutput {
    fn text(&self, text: &str, thinking: bool) {
        let mut segment = self.segment.lock().unwrap_or_else(|e| e.into_inner());
        if segment.1 != Some(thinking) {
            if segment.1.is_some() {
                self.event(SubagentChange::MessageEnd {
                    msg_id: segment.0.to_string(),
                });
            }
            segment.0 += 1;
            segment.1 = Some(thinking);
        }
        let prefix = if thinking { "thinking" } else { "text" };
        self.event(SubagentChange::Text {
            entry_id: format!("{prefix}-{}", segment.0),
            text: text.into(),
            thinking,
        });
    }

    fn close_segment(&self) {
        let mut segment = self.segment.lock().unwrap_or_else(|e| e.into_inner());
        if segment.1.take().is_some() {
            self.event(SubagentChange::MessageEnd {
                msg_id: segment.0.to_string(),
            });
        }
    }

    pub(crate) fn event(&self, change: SubagentChange) {
        self.parent.emit_subagent(SubagentEvent {
            run: self.identity.clone(),
            occurred_at: Utc::now().timestamp_millis(),
            change,
        });
    }
}

pub(crate) struct ChildExecution {
    pub(crate) output: Arc<ChildOutput>,
    tasks: Arc<SubagentTasks>,
    started: Instant,
    finished: bool,
}

impl ChildExecution {
    pub(crate) fn new(identity: SubagentIdentity, parent: Arc<dyn OutputSink>, tasks: Arc<SubagentTasks>) -> Self {
        tasks.active.fetch_add(1, Ordering::AcqRel);
        let output = Arc::new(ChildOutput {
            identity,
            parent,
            segment: Mutex::new((0, None)),
        });
        output.event(SubagentChange::Started);
        Self {
            output,
            tasks,
            started: Instant::now(),
            finished: false,
        }
    }

    pub(crate) fn finish(&mut self, status: SubagentStatus, text: String) {
        if !self.finished {
            self.finished = true;
            self.output.event(SubagentChange::Finished {
                status,
                text,
                elapsed_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            });
        }
    }
}

impl Drop for ChildExecution {
    fn drop(&mut self) {
        let status = if std::thread::panicking() {
            SubagentStatus::Failed
        } else {
            SubagentStatus::Cancelled
        };
        let reason = if std::thread::panicking() {
            "Sub-agent task panicked".into()
        } else {
            String::new()
        };
        self.finish(status, reason);
        self.tasks.active.fetch_sub(1, Ordering::AcqRel);
        self.tasks.finished.notify_one();
    }
}

impl OutputSink for ChildOutput {
    fn emit_text_delta(&self, text: &str, _msg_id: &str) {
        self.text(text, false);
    }
    fn emit_thinking(&self, text: &str, _msg_id: &str) {
        self.text(text, true);
    }
    fn emit_tool_call(&self, tool_use_id: &str, name: &str, input: &str) {
        self.close_segment();
        self.event(SubagentChange::ToolCall {
            tool_call_id: tool_use_id.into(),
            name: name.into(),
            input: input.into(),
        });
    }
    fn emit_tool_result(&self, tool_use_id: &str, name: &str, is_error: bool, content: &str) {
        self.event(SubagentChange::ToolResult {
            tool_call_id: tool_use_id.into(),
            name: name.into(),
            output: content.into(),
            is_error,
        });
    }
    fn emit_stream_start(&self, _msg_id: &str) {}
    fn emit_stream_end(&self, _msg_id: &str, _: usize, _: u64, _: u64, _: u64, _: u64) {
        self.close_segment();
    }
    fn emit_error(&self, _msg: &str) {
        // The enclosing engine result owns failure; a message error is not a task terminal.
    }
    fn emit_info(&self, _msg: &str) {}
}
