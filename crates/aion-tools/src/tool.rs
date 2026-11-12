use async_trait::async_trait;
use serde_json::Value;

use aion_config::hooks::HooksConfig;
use aion_protocol::events::ToolCategory;
use aion_types::message::ContentBlock;
use aion_types::skill_types::ContextModifier;
use aion_types::tool::{JsonSchema, ToolResult};

use crate::validation::{ToolInputValidationError, validate_tool_input};

/// Complete output from one tool execution.
///
/// Most tools only return a textual [`ToolResult`]. Tools that load provider
/// input, such as `ViewImage`, can additionally supply content blocks that the
/// engine appends as a separate user message after the tool result.
#[derive(Debug, Clone)]
pub struct ToolExecutionOutput {
    pub result: ToolResult,
    pub follow_up_blocks: Vec<ContentBlock>,
}

impl From<ToolResult> for ToolExecutionOutput {
    fn from(result: ToolResult) -> Self {
        Self {
            result,
            follow_up_blocks: Vec::new(),
        }
    }
}

/// Truncate a string to at most `max_bytes`, snapping to a char boundary.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A tool that the agent can invoke
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (must match API schema)
    fn name(&self) -> &str;

    /// Human-readable description for the LLM
    fn description(&self) -> &str;

    /// JSON Schema for input parameters
    fn input_schema(&self) -> JsonSchema;

    /// Validate arguments before approval, hooks, or execution.
    fn validate_input(&self, input: &Value) -> Result<(), Vec<ToolInputValidationError>> {
        validate_tool_input(&self.input_schema(), input)
    }

    /// Include host failures without presenting them as malformed model arguments.
    fn preflight(&self, input: &Value) -> Result<(), ToolResult> {
        self.validate_input(input).map_err(|errors| ToolResult {
            content: crate::validation::format_tool_input_error(self.name(), &errors),
            is_error: true,
        })
    }

    /// Whether this tool is safe to run concurrently
    fn is_concurrency_safe(&self, input: &Value) -> bool;

    /// Execute the tool
    async fn execute(&self, input: Value) -> ToolResult;

    /// Execute the tool and optionally provide content for the next model turn.
    ///
    /// The default preserves the existing text-only tool contract. Multimodal
    /// loaders override this method so their binary payload never travels in
    /// the textual tool-result channel.
    async fn execute_with_follow_up(&self, input: Value) -> ToolExecutionOutput {
        self.execute(input).await.into()
    }

    /// Whether advertising and executing this tool requires image-input
    /// support from the currently selected model.
    fn requires_image_input(&self) -> bool {
        false
    }

    /// Return an optional context modifier based on the tool input.
    /// Called after execute() to collect any engine-level overrides.
    /// Only SkillTool overrides this; all other tools return None.
    fn context_modifier_for(&self, _input: &Value) -> Option<ContextModifier> {
        None
    }

    /// Return any hooks declared in the skill's frontmatter for dynamic registration.
    /// Called after a successful execute() so the orchestration layer can merge
    /// the returned hooks into the active HookEngine.
    /// Only SkillTool overrides this; all other tools return None.
    fn skill_hooks_for(&self, _input: &Value) -> Option<HooksConfig> {
        None
    }

    /// Per-tool maximum result size in UTF-8 bytes before truncation.
    ///
    /// The agent-level output limit may impose a smaller bound.
    fn max_result_size(&self) -> usize {
        50_000
    }

    /// Minimum budget required to deliver a meaningful structured result.
    /// The orchestrator checks this before executing any calls in the batch.
    fn minimum_result_size(&self) -> usize {
        0
    }

    /// Opt into budget-aware structured output instead of text compaction.
    /// Called again at final delivery for validation and approval errors; must
    /// be idempotent and must never re-execute the underlying operation.
    fn format_result(&self, _input: &Value, _result: &ToolResult, _max_bytes: usize) -> Option<ToolResult> {
        None
    }

    /// MCP server and original tool identity, used for scoped runtime recovery.
    fn mcp_origin(&self) -> Option<(&str, &str)> {
        None
    }

    /// Tool category for protocol classification
    fn category(&self) -> ToolCategory;

    /// Whether this tool's schema should be deferred (sent as name-only stub).
    /// Override to `true` for tools with large schemas or infrequent use.
    fn is_deferred(&self) -> bool {
        false
    }

    /// Human-readable description of what the tool will do with the given input
    fn describe(&self, input: &Value) -> String {
        format!("{}: {}", self.name(), serde_json::to_string(input).unwrap_or_default())
    }
}

#[cfg(test)]
#[path = "tool_test.rs"]
mod tool_test;
