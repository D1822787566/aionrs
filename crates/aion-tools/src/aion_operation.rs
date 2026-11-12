use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use aion_process::CommandRunner;
use aion_protocol::events::ToolCategory;
use aion_types::tool::{JsonSchema, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::Tool;
use crate::aion_operation_contract::{OperationContract, local_descriptor, local_schema, validate_operation_input};
use crate::validation::{ToolInputValidationError, format_tool_input_error, validate_tool_input};

#[path = "aion_operation_output.rs"]
mod output;
use output::{MIN_OUTPUT_BYTES, OperationOutput};

const HELPER_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DIAGNOSTIC_BYTES: usize = 2_000;

/// Fixed AionUi operations that can be called without asking the model to
/// construct a shell command. The strings are CLI paths, but the tool never
/// accepts an arbitrary path: only values in this list are converted to args.
pub const OPERATION_PATHS: &[&str] = &[
    "capabilities",
    "config.capabilities",
    "config.context",
    "config.conversation.rename",
    "config.assistants.list",
    "config.assistants.get",
    "config.assistants.create",
    "config.assistants.update",
    "config.assistants.delete",
    "config.assistants.import",
    "config.assistants.state",
    "config.assistants.rule.read",
    "config.assistants.rule.write",
    "config.assistants.rule.delete",
    "config.assistants.skill.read",
    "config.assistants.skill.write",
    "config.assistants.skill.delete",
    "config.skills.list",
    "config.skills.info",
    "config.skills.paths",
    "config.skills.import",
    "config.skills.delete",
    "config.skills.scan",
    "config.skills.external-paths.list",
    "config.skills.external-paths.add",
    "config.skills.external-paths.remove",
    "config.skills.market.enable",
    "config.skills.market.disable",
    "config.mcp.servers.list",
    "config.mcp.servers.get",
    "config.mcp.servers.create",
    "config.mcp.servers.update",
    "config.mcp.servers.delete",
    "config.mcp.servers.toggle",
    "config.mcp.servers.import",
    "config.mcp.test-connection",
    "config.mcp.agent-configs",
    "config.mcp.oauth.check-status",
    "config.mcp.oauth.login",
    "config.mcp.oauth.logout",
    "config.mcp.oauth.authenticated",
    "config.providers.list",
    "config.providers.create",
    "config.providers.update",
    "config.providers.delete",
    "config.providers.detect-protocol",
    "config.providers.fetch-models",
    "config.providers.models.fetch",
    "config.providers.health-check",
    "config.settings.get",
    "config.settings.patch",
    "config.settings.client.get",
    "config.settings.client.put",
    "config.agents.list",
    "config.agents.enable",
    "config.agents.overrides.get",
    "config.agents.overrides.set",
    "config.agents.custom.create",
    "config.agents.custom.update",
    "config.agents.custom.delete",
    "config.agents.custom.try-connect",
    "config.cron.jobs.list",
    "config.cron.jobs.get",
    "config.cron.jobs.create",
    "config.cron.jobs.update",
    "config.cron.jobs.delete",
    "config.cron.jobs.run",
    "config.cron.jobs.skill.get",
    "config.cron.jobs.skill.save",
    "config.cron.jobs.skill.delete",
    "config.cron.current.list",
    "config.cron.current.create",
    "config.cron.current.update",
    "session.capabilities",
    "session.list",
    "session.send-message",
    "team.capabilities",
    "team.help",
    "team.context",
    "team.members",
    "team.read-messages",
    "team.send-message",
    "team.interrupt-agent",
    "team.task.create",
    "team.task.update",
    "team.task.list",
    "team.list-assistants",
    "team.describe-assistant",
    "team.spawn-agent",
    "team.rename-agent",
    "team.clear-agent-context",
    "team.shutdown-agent",
    "conversation.capabilities",
    "conversation.create",
    "skills.capabilities",
    "skills.list",
    "skills.show",
    "skills.cat",
];

/// An AionUi operation that is safe to advertise only when the host injected
/// the helper binary path and runtime environment.
pub struct AionOperationTool {
    helper: PathBuf,
    runtime_env: Vec<(String, String)>,
    output: OperationOutput,
    contract: Option<OperationContract>,
}

impl AionOperationTool {
    pub fn from_runtime_env(runtime_env: &[(String, String)]) -> Option<Self> {
        let helper = runtime_env
            .iter()
            .find(|(name, _)| name == "AIONUI_HELPER_BIN")
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)?;

        Some(Self {
            helper,
            runtime_env: runtime_env.to_vec(),
            output: OperationOutput::default(),
            contract: None,
        })
    }

    pub fn with_contract(mut self, contract: Option<OperationContract>) -> Self {
        self.contract = contract;
        self
    }

    fn requires_confirmation(operation: &str) -> bool {
        let action = operation.rsplit('.').next().unwrap_or(operation);
        matches!(
            action,
            "create"
                | "update"
                | "delete"
                | "import"
                | "write"
                | "state"
                | "toggle"
                | "login"
                | "logout"
                | "enable"
                | "disable"
                | "add"
                | "remove"
                | "patch"
                | "put"
                | "save"
                | "run"
                | "send-message"
                | "rename"
                | "spawn-agent"
                | "interrupt-agent"
                | "shutdown-agent"
                | "clear-agent-context"
        )
    }

    fn command_args(operation: &str) -> Vec<String> {
        operation.split('.').map(str::to_owned).collect()
    }

    fn error_result(operation: &str, code: &str, message: impl Into<String>) -> ToolResult {
        ToolResult {
            content: serde_json::to_string_pretty(&json!({
                "success": false,
                "operation": operation,
                "data": Value::Null,
                "error": { "message": message.into() },
                "code": code,
                "execution": {"status": if matches!(code, "aion_operation_timeout" | "aion_operation_invalid_output" | "aion_operation_helper_failed" | "aion_operation_process_failed") { "unknown" } else { "not_executed" }, "stage": "helper_or_preflight"},
            }))
            .unwrap_or_else(|_| format!("{{\"success\":false,\"operation\":{operation:?},\"code\":{code:?}}}")),
            is_error: true,
        }
    }

    fn normalize_output(operation: &str, output: &[u8], stderr: &[u8], exit_code: Option<i32>) -> ToolResult {
        let text = String::from_utf8_lossy(output).trim().to_owned();
        let parsed = match serde_json::from_str::<Value>(&text) {
            Ok(value) => value,
            Err(error) => {
                if exit_code != Some(0) && text.is_empty() {
                    return Self::error_result(
                        operation,
                        "aion_operation_helper_failed",
                        format!(
                            "helper exited without a JSON failure envelope (exit_code={exit_code:?}); diagnostic: {}",
                            truncate_diagnostic(stderr)
                        ),
                    );
                }
                let diagnostic = if stderr.is_empty() {
                    format!("helper returned invalid JSON: {error}")
                } else {
                    format!(
                        "helper returned invalid JSON: {error}; stderr: {}",
                        truncate_diagnostic(stderr)
                    )
                };
                return Self::error_result(operation, "aion_operation_invalid_output", diagnostic);
            }
        };

        let helper_success = parsed.get("success").and_then(Value::as_bool).unwrap_or(false);
        if !parsed.is_object()
            || parsed.get("success").and_then(Value::as_bool).is_none()
            || (helper_success && exit_code != Some(0))
        {
            return Self::error_result(
                operation,
                "aion_operation_invalid_output",
                "helper envelope conflicts with its exit status or lacks success",
            );
        }
        let success = exit_code == Some(0) && helper_success;
        let helper_error = parsed.get("error").cloned().unwrap_or(Value::Null);
        let helper_code = helper_error
            .get("code")
            .and_then(Value::as_str)
            .or_else(|| parsed.get("code").and_then(Value::as_str));
        let code = if success {
            Value::Null
        } else {
            Value::String(
                helper_code
                    .unwrap_or(if exit_code.is_none() {
                        "aion_operation_failed"
                    } else {
                        "aion_operation_helper_failed"
                    })
                    .to_owned(),
            )
        };
        let error = if success {
            Value::Null
        } else if !helper_error.is_null() {
            helper_error
        } else {
            json!({
                "message": if exit_code.is_none() { "Aion helper did not return a successful result" } else { "Aion helper exited unsuccessfully" },
                "exit_code": exit_code,
                "stderr": (!stderr.is_empty()).then(|| truncate_diagnostic(stderr)),
            })
        };
        let data = parsed.get("data").cloned().unwrap_or(Value::Null);
        let meta = parsed.get("meta").cloned().unwrap_or(Value::Null);
        let content = serde_json::to_string_pretty(&json!({
            "success": success,
            "operation": operation,
            "data": data,
            "error": error,
            "code": code,
            "meta": meta,
            "execution": parsed.get("execution").cloned().unwrap_or_else(||json!({"status":if success {"succeeded"} else {"unknown"},"stage":"helper_response"})),
        }))
        .unwrap_or_else(|_| {
            Self::error_result(
                operation,
                "aion_operation_invalid_output",
                "failed to serialize helper result",
            )
            .content
        });

        ToolResult {
            content,
            is_error: !success,
        }
    }

    async fn run(&self, operation: &str, input: Value) -> ToolResult {
        let payload = serde_json::to_vec(&input).unwrap_or_else(|_| b"{}".to_vec());
        let mut command = Command::new(&self.helper);
        command.args(Self::command_args(operation));
        command.envs(self.runtime_env.iter().map(|(name, value)| (name, value)));

        let started = Arc::new(AtomicBool::new(false));
        match CommandRunner::new(command)
            .started_flag(started.clone())
            .stdin_bytes(payload)
            .timeout(HELPER_TIMEOUT)
            .run()
            .await
        {
            Ok(result) if result.timed_out => {
                Self::error_result(operation, "aion_operation_timeout", "Aion helper timed out")
            }
            Ok(result) => Self::normalize_output(operation, &result.stdout, &result.stderr, result.exit_code),
            Err(error) => Self::error_result(
                operation,
                if started.load(Ordering::Acquire) {
                    "aion_operation_process_failed"
                } else {
                    "aion_operation_spawn_failed"
                },
                error.to_string(),
            ),
        }
    }
}

fn truncate_diagnostic(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut end = text.len().min(MAX_DIAGNOSTIC_BYTES);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[async_trait]
impl Tool for AionOperationTool {
    fn name(&self) -> &str {
        "AionOperation"
    }

    fn description(&self) -> &str {
        "Calls a fixed AionUi operation through the injected AionCore helper. Before an unfamiliar operation, call operation.describe with input={operation:target_operation} for exact fields, examples and confirmation requirements. Incorrect fields are rejected before execution; regenerate the call using those fields. Use ExecCommand for ordinary PowerShell, Git, Python, Node, OfficeCLI, or filesystem commands. \
         Lists return summaries. Inspect delivery.complete, has_more, next_offset and $details JSON pointers. \
         Continue with operation=result.read and input={result_id,path,offset,limit}; path is a JSON pointer into the original envelope (default /data, empty string for the whole envelope). \
         Offsets count array items, object fields, or UTF-8 bytes for text. Text chunks are exact; concatenate data strings using next_offset. \
         result.read only reads the same immutable result and never executes the original operation. Results expire after 30 minutes, runtime restart, or eviction (8 results, 32 MiB serialized total, 8 MiB each). \
         After a successful write, never repeat the write to retrieve output. If its result expired, query resource state instead. \
         Configuration enabled flags and historical connection tests do not prove a tool is currently callable in this session."
    }

    fn input_schema(&self) -> JsonSchema {
        let mut operations = OPERATION_PATHS.to_vec();
        operations.push("result.read");
        operations.push("operation.describe");
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["operation"],
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": operations,
                    "description": "A whitelisted AionUi operation or local read-only result.read"
                },
                "input": {
                    "type": ["object", "array"],
                    "description": "Operation-specific JSON input; use operation.describe for exact fields. MCP import also accepts legacy arrays."
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Must be true for operations that create, update, delete, write, or otherwise change state"
                }
            }
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    fn validate_input(&self, input: &Value) -> Result<(), Vec<ToolInputValidationError>> {
        validate_tool_input(&self.input_schema(), input)?;
        let operation = input["operation"].as_str().unwrap_or_default();
        let schema = local_schema(operation).or_else(|| {
            self.contract
                .as_ref()?
                .descriptor(operation)
                .ok()
                .map(|d| d["input_schema"].clone())
        });
        if let Some(schema) = schema {
            validate_operation_input(&schema, input.get("input").unwrap_or(&json!({})))?;
        }
        Ok(())
    }

    fn preflight(&self, input: &Value) -> Result<(), ToolResult> {
        if let Err(errors) = self.validate_input(input) {
            let mut value: Value =
                serde_json::from_str(&format_tool_input_error(self.name(), &errors)).expect("validation JSON");
            value["operation"] = input["operation"].clone();
            value["execution"] = json!({"status":"not_executed","stage":"input_validation"});
            return Err(ToolResult {
                content: value.to_string(),
                is_error: true,
            });
        }
        let operation = input["operation"].as_str().unwrap_or_default();
        if operation == "operation.describe"
            && local_schema(input["input"]["operation"].as_str().unwrap_or_default()).is_some()
        {
            return Ok(());
        }
        if operation != "result.read" {
            let contract = self.contract.as_ref().ok_or("aion_operation_contract_unavailable");
            if let Err(code) = contract.and_then(|c| {
                c.descriptor(if operation == "operation.describe" {
                    input["input"]["operation"].as_str().unwrap_or_default()
                } else {
                    operation
                })
            }) {
                return Err(Self::error_result(
                    operation,
                    code,
                    "The host must provide a supported operation contract. This call was not executed.",
                ));
            }
        }
        Ok(())
    }

    fn minimum_result_size(&self) -> usize {
        MIN_OUTPUT_BYTES
    }

    fn format_result(&self, input: &Value, result: &ToolResult, max_bytes: usize) -> Option<ToolResult> {
        Some(self.output.format(input, result, max_bytes))
    }

    async fn execute(&self, input: Value) -> ToolResult {
        if let Err(result) = self.preflight(&input) {
            return result;
        }
        let Some(operation) = input.get("operation").and_then(Value::as_str) else {
            return Self::error_result("", "aion_operation_invalid_input", "operation must be a string");
        };
        if operation == "result.read" {
            return self.output.read(input.get("input").unwrap_or(&json!({})), 10_000);
        }
        if operation == "operation.describe" {
            let target = input["input"]["operation"].as_str().unwrap_or_default();
            let descriptor = local_descriptor(target)
                .or_else(|| self.contract.as_ref().and_then(|c| c.descriptor(target).ok()).cloned());
            return ToolResult{content:json!({"success":true,"operation":operation,"data":descriptor,"execution":{"status":"succeeded","stage":"local_description"}}).to_string(),is_error:false};
        }
        if !OPERATION_PATHS.contains(&operation) {
            return Self::error_result(operation, "aion_operation_unknown", "operation is not whitelisted");
        }
        if Self::requires_confirmation(operation) && input.get("confirm").and_then(Value::as_bool) != Some(true) {
            return Self::error_result(
                operation,
                "aion_operation_confirmation_required",
                "set confirm=true after reviewing the requested state change",
            );
        }
        let operation_input = input.get("input").cloned().unwrap_or_else(|| json!({}));
        self.run(operation, operation_input).await
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Exec
    }

    fn describe(&self, input: &Value) -> String {
        input
            .get("operation")
            .and_then(Value::as_str)
            .map(|operation| format!("Aion operation {operation}"))
            .unwrap_or_else(|| "Aion operation".to_owned())
    }
}

#[cfg(test)]
#[path = "aion_operation_test.rs"]
mod aion_operation_test;
