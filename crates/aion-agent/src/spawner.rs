use aion_tools::aion_operation_contract::OperationContract;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;

use async_trait::async_trait;

use aion_config::config::Config;
use aion_providers::LlmProvider;
use aion_tools::aion_operation::AionOperationTool;
use aion_tools::edit::EditTool;
use aion_tools::exec_command::ExecCommandTool;
use aion_tools::glob::GlobTool;
use aion_tools::grep::GrepTool;
use aion_tools::read::ReadTool;
use aion_tools::registry::ToolRegistry;
use aion_tools::write::WriteTool;
use aion_types::message::{StopReason, TokenUsage};
use aion_types::subagent::{SubagentIdentity, SubagentStatus};
use chrono::Utc;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::engine::AgentEngine;
use crate::output::OutputSink;
use crate::output::null_sink::NullSink;
use crate::subagent_output::{ChildExecution, SubagentTasks};
use crate::tool_policy::ToolPolicy;

// Re-export from aion-types — single source of truth
pub use aion_types::spawner::{ForkOverrides, Spawner, SubAgentConfig, SubAgentResult};

/// Runtime tools supplied by an embedding host, with the same scope in children.
pub type HostTools = Arc<RwLock<Vec<Arc<dyn aion_tools::Tool>>>>;

/// Spawns independent child agents that share the parent's LLM provider.
///
/// Results remain one parent tool result. An optional identity-scoped sink
/// observes child execution without writing child text into the parent stream.
///
/// Children inherit the parent's runtime tool policy. Fork overrides can
/// further narrow that policy, but cannot restore tools denied to the parent.
pub struct AgentSpawner {
    provider: Arc<dyn LlmProvider>,
    base_config: Config,
    cwd: PathBuf,
    runtime_env: Vec<(String, String)>,
    operation_contract: Option<OperationContract>,
    tool_policy: ToolPolicy,
    host_tools: HostTools,
    output: Option<Arc<dyn OutputSink>>,
    tasks: Arc<SubagentTasks>,
}

impl AgentSpawner {
    pub fn new(provider: Arc<dyn LlmProvider>, config: Config, cwd: PathBuf, tool_policy: ToolPolicy) -> Self {
        Self::new_with_env(provider, config, cwd, Vec::new(), tool_policy)
    }

    pub fn new_with_env(
        provider: Arc<dyn LlmProvider>,
        config: Config,
        cwd: PathBuf,
        runtime_env: Vec<(String, String)>,
        tool_policy: ToolPolicy,
    ) -> Self {
        Self {
            provider,
            base_config: config,
            cwd,
            runtime_env,
            operation_contract: None,
            tool_policy,
            host_tools: Default::default(),
            output: None,
            tasks: Default::default(),
        }
    }

    pub fn with_operation_contract(mut self, contract: Option<OperationContract>) -> Self {
        self.operation_contract = contract;
        self
    }

    pub fn with_host_tools(mut self, tools: HostTools) -> Self {
        self.host_tools = tools;
        self
    }

    pub(crate) fn with_observer(mut self, output: Arc<dyn OutputSink>, tasks: Arc<SubagentTasks>) -> Self {
        self.output = Some(output);
        self.tasks = tasks;
        self
    }

    /// Spawn a single sub-agent and wait for result.
    pub async fn spawn_one(&self, sub_config: SubAgentConfig) -> SubAgentResult {
        self.spawn_observed(sub_config, None).await
    }

    async fn spawn_observed(
        &self,
        sub_config: SubAgentConfig,
        mut execution: Option<ChildExecution>,
    ) -> SubAgentResult {
        let mut config = self.base_config.clone();
        config.max_turns = Some(sub_config.max_turns);
        config.max_tokens = Some(sub_config.max_tokens);
        if let Some(sp) = sub_config.system_prompt.clone() {
            config.system_prompt = Some(sp);
        }
        config.session.enabled = false;
        config.tools.auto_approve = true;

        tracing::info!(target: "aion_agent", cwd = %self.cwd.display(), "sub-agent spawned with workspace cwd");

        let child_policy = effective_child_tool_policy(&self.tool_policy, &[]);
        let mut tools = build_tool_registry_with_contract(
            &child_policy,
            &self.cwd,
            &self.runtime_env,
            self.operation_contract.clone(),
        );
        install_host_tools(&mut tools, &self.host_tools, &child_policy);
        let output: Arc<dyn OutputSink> = execution
            .as_ref()
            .map(|execution| execution.output.clone() as Arc<dyn OutputSink>)
            .unwrap_or_else(|| Arc::new(NullSink));
        let mut engine = AgentEngine::new_with_provider_and_env(
            self.provider.clone(),
            config,
            tools,
            output,
            self.cwd.clone(),
            self.runtime_env.clone(),
        );
        engine.set_tool_policy(child_policy);

        let (result, status, observation) = match engine.run(&sub_config.prompt, "").await {
            Ok(result) => {
                let status = if result.stop_reason == StopReason::EndTurn {
                    SubagentStatus::Completed
                } else {
                    SubagentStatus::Failed
                };
                let observation = if status == SubagentStatus::Completed {
                    result.text.clone()
                } else {
                    format!("Stopped: {:?}\n{}", result.stop_reason, result.text)
                };
                (
                    SubAgentResult {
                        name: sub_config.name,
                        text: result.text,
                        usage: result.usage,
                        turns: result.turns,
                        is_error: false,
                    },
                    status,
                    observation,
                )
            }
            Err(e) => {
                let observation = format!("Sub-agent error: {e}");
                (
                    SubAgentResult {
                        name: sub_config.name,
                        text: format!("Sub-agent error: {}", e),
                        usage: TokenUsage::default(),
                        turns: 0,
                        is_error: true,
                    },
                    SubagentStatus::Failed,
                    observation,
                )
            }
        };
        if let Some(execution) = &mut execution {
            execution.finish(status, observation);
        }
        result
    }

    /// Spawn multiple sub-agents in parallel.
    pub async fn spawn_parallel(&self, sub_configs: Vec<SubAgentConfig>) -> Vec<SubAgentResult> {
        self.spawn_parallel_for_call(sub_configs, "").await
    }

    pub(crate) async fn spawn_parallel_for_call(
        &self,
        sub_configs: Vec<SubAgentConfig>,
        call_id: &str,
    ) -> Vec<SubAgentResult> {
        let mut pending = JoinSet::new();
        let mut identities = std::collections::HashMap::new();
        let mut results: Vec<Option<SubAgentResult>> = (0..sub_configs.len()).map(|_| None).collect();
        for (index, config) in sub_configs.into_iter().enumerate() {
            let spawner = self.clone_for_spawn();
            let execution = self.output.as_ref().filter(|_| !call_id.is_empty()).map(|output| {
                ChildExecution::new(
                    SubagentIdentity {
                        child_run_id: Uuid::now_v7().to_string(),
                        parent_call_id: call_id.into(),
                        task_index: index,
                        name: config.name.clone(),
                        prompt: config.prompt.clone(),
                        started_at: Utc::now().timestamp_millis(),
                    },
                    output.clone(),
                    self.tasks.clone(),
                )
            });
            let name = config.name.clone();
            let handle = pending.spawn(async move { spawner.spawn_observed(config, execution).await });
            identities.insert(handle.id(), (index, name));
        }
        // JoinSet aborts every remaining child when the parent tool future is dropped.
        while let Some(joined) = pending.join_next_with_id().await {
            let (id, result) = match joined {
                Ok((id, result)) => (id, result),
                Err(e) => {
                    let id = e.id();
                    let name = identities.get(&id).map(|(_, name)| name.clone()).unwrap_or_default();
                    (
                        id,
                        SubAgentResult {
                            name,
                            text: format!("Task join error: {}", e),
                            usage: TokenUsage::default(),
                            turns: 0,
                            is_error: true,
                        },
                    )
                }
            };
            if let Some((index, _)) = identities.remove(&id) {
                results[index] = Some(result);
            }
        }
        results.into_iter().flatten().collect()
    }

    fn clone_for_spawn(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            base_config: self.base_config.clone(),
            cwd: self.cwd.clone(),
            runtime_env: self.runtime_env.clone(),
            operation_contract: self.operation_contract.clone(),
            tool_policy: self.tool_policy.clone(),
            host_tools: self.host_tools.clone(),
            output: self.output.clone(),
            tasks: self.tasks.clone(),
        }
    }
}

#[async_trait]
impl Spawner for AgentSpawner {
    async fn spawn_fork(&self, sub_config: SubAgentConfig, overrides: ForkOverrides) -> SubAgentResult {
        let mut config = self.base_config.clone();
        config.max_turns = Some(sub_config.max_turns);
        config.max_tokens = Some(sub_config.max_tokens);
        if let Some(sp) = sub_config.system_prompt.clone() {
            config.system_prompt = Some(sp);
        }
        config.session.enabled = false;
        config.tools.auto_approve = true;
        if let Some(model) = overrides.model.clone() {
            config.model = model;
        }

        let child_policy = effective_child_tool_policy(&self.tool_policy, &overrides.allowed_tools);
        let mut tools = build_tool_registry_with_contract(
            &child_policy,
            &self.cwd,
            &self.runtime_env,
            self.operation_contract.clone(),
        );
        install_host_tools(&mut tools, &self.host_tools, &child_policy);
        let output: Arc<dyn OutputSink> = Arc::new(NullSink);
        let mut engine = AgentEngine::new_with_provider_and_env(
            self.provider.clone(),
            config,
            tools,
            output,
            self.cwd.clone(),
            self.runtime_env.clone(),
        );
        engine.set_initial_reasoning_effort(overrides.effort.clone());
        engine.set_tool_policy(child_policy);

        match engine.run(&sub_config.prompt, "").await {
            Ok(result) => SubAgentResult {
                name: sub_config.name,
                text: result.text,
                usage: result.usage,
                turns: result.turns,
                is_error: false,
            },
            Err(e) => SubAgentResult {
                name: sub_config.name,
                text: format!("Sub-agent error: {}", e),
                usage: TokenUsage::default(),
                turns: 0,
                is_error: true,
            },
        }
    }
}

fn install_host_tools(tools: &mut ToolRegistry, host_tools: &HostTools, policy: &ToolPolicy) {
    for tool in host_tools.read().unwrap_or_else(|e| e.into_inner()).iter() {
        if policy.allows(tool.name()) {
            tools.replace_shared(tool.clone());
        }
    }
}

fn effective_child_tool_policy(parent: &ToolPolicy, allowed_tools: &[String]) -> ToolPolicy {
    if allowed_tools.is_empty() {
        return parent.clone();
    }

    ToolPolicy::allow_only(
        allowed_tools
            .iter()
            .filter(|tool_name| parent.allows(tool_name))
            .cloned(),
    )
}

#[cfg(test)]
fn build_tool_registry(policy: &ToolPolicy, cwd: &Path, runtime_env: &[(String, String)]) -> ToolRegistry {
    build_tool_registry_with_contract(policy, cwd, runtime_env, None)
}

fn build_tool_registry_with_contract(
    policy: &ToolPolicy,
    cwd: &Path,
    runtime_env: &[(String, String)],
    contract: Option<OperationContract>,
) -> ToolRegistry {
    let all_tools: Vec<(&str, Box<dyn aion_tools::Tool>)> = vec![
        ("Read", Box::new(ReadTool::new(None))),
        ("Write", Box::new(WriteTool::new(None))),
        ("Edit", Box::new(EditTool::new(None))),
        (
            "ExecCommand",
            Box::new(ExecCommandTool::new_with_env(cwd.to_path_buf(), runtime_env.to_vec())),
        ),
        ("Grep", Box::new(GrepTool::new(cwd.to_path_buf()))),
        ("Glob", Box::new(GlobTool::new(cwd.to_path_buf()))),
    ];

    let mut registry = ToolRegistry::new();
    for (name, tool) in all_tools {
        if policy.allows(name) {
            registry.register(tool);
        }
    }
    if policy.allows("AionOperation")
        && let Some(tool) = AionOperationTool::from_runtime_env(runtime_env)
    {
        registry.register(Box::new(tool.with_contract(contract)));
    }
    registry
}

#[cfg(test)]
#[path = "spawner_test.rs"]
mod spawner_test;
