use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;

use crate::skill_delivery::deliver_skill;
use aion_protocol::events::ToolCategory;
use aion_skills::permissions::PermissionRule;
use aion_tools::{Tool, ToolExecutionOutput};
use aion_types::tool::ToolResult;
use async_trait::async_trait;
use serde_json::{Value, json};

/// A skill made available by the embedding host's conversation snapshot.
#[derive(Debug, Clone)]
pub struct HostSkill {
    pub name: String,
    pub description: String,
}

/// Plain instructions; loading never activates native hooks or shell expansion.
pub struct HostSkillContent {
    pub root: PathBuf,
    pub body: String,
}

#[derive(Debug, Clone, Copy)]
pub enum HostSkillReadError {
    NotEnabled,
    Unavailable,
}

/// Host-owned resolution preserves user and conversation scoping.
#[async_trait]
pub trait HostSkillSource: Debug + Send + Sync {
    fn available(&self) -> Vec<HostSkill>;
    fn unavailable(&self) -> Vec<String> {
        Vec::new()
    }
    async fn read(&self, name: &str) -> Result<HostSkillContent, HostSkillReadError>;
}

pub(crate) struct HostedSkillTool {
    source: Arc<dyn HostSkillSource>,
    deny: Vec<PermissionRule>,
}

impl HostedSkillTool {
    pub(crate) fn new(source: Arc<dyn HostSkillSource>, deny: &[String]) -> Self {
        Self {
            source,
            deny: deny.iter().map(|rule| PermissionRule::parse(rule)).collect(),
        }
    }

    pub(crate) fn available(&self) -> Vec<HostSkill> {
        let mut skills: Vec<_> = self
            .source
            .available()
            .into_iter()
            .filter(|skill| !self.denied(&skill.name))
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    fn denied(&self, name: &str) -> bool {
        self.deny.iter().any(|rule| rule.matches(name))
    }

    pub(crate) fn index_prompt(&self) -> String {
        let mut text = String::from(
            "## Available Skills\nUse Skill to read these conversation-selected instructions. Read supplementary files relative to the returned skill root, using permitted file tools or the host skills CLI. Loading a skill does not execute it.\n",
        );
        for skill in self.available() {
            text.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
        let unavailable: Vec<_> = self
            .source
            .unavailable()
            .into_iter()
            .filter(|name| !self.denied(name))
            .collect();
        if !unavailable.is_empty() {
            text.push_str(&format!(
                "Selected skills currently unavailable (do not invoke; report this if needed): {}\n",
                unavailable.join(", ")
            ));
        }
        text
    }
}

#[async_trait]
impl Tool for HostedSkillTool {
    fn name(&self) -> &str {
        "Skill"
    }
    fn description(&self) -> &str {
        "Read a conversation-selected skill's full instructions and root directory. This does not execute the instructions. Use the exact advertised skill name. For chunked delivery use byte offset and limit; continue with next_offset until complete=true."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"skill":{"type":"string"},"args":{"type":"string"},"offset":{"type":"integer","minimum":0,"description":"Zero-based UTF-8 byte offset for chunked delivery"},"limit":{"type":"integer","minimum":1,"description":"Maximum UTF-8 bytes in this skill chunk"}},"required":["skill"],"additionalProperties":false})
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let name = input["skill"].as_str().unwrap_or_default();
        if self.denied(name) {
            return ToolResult {
                content: json!({"code":"skill_denied","skill":name}).to_string(),
                is_error: true,
            };
        }
        match self.source.read(name).await {
            Ok(skill) => {
                let mut content = format!(
                    "[Skill: {name}]\nSkill root (resolve relative paths here): {}\n\n{}",
                    skill.root.display(),
                    skill.body
                );
                if let Some(args) = input["args"].as_str() {
                    content.push_str(&format!("\n\nSkill arguments (plain context, not executed):\n{args}"));
                }
                ToolResult {
                    content,
                    is_error: false,
                }
            }
            Err(error) => ToolResult {
                content: json!({"code":match error {
                HostSkillReadError::NotEnabled=>"skill_not_enabled",HostSkillReadError::Unavailable=>"skill_unavailable"
            },"skill":name})
                .to_string(),
                is_error: true,
            },
        }
    }

    async fn execute_with_follow_up(&self, input: Value) -> ToolExecutionOutput {
        let name = input["skill"].as_str().unwrap_or_default().to_owned();
        let result = self.execute(input.clone()).await;
        deliver_skill(&name, &input, result)
    }
}

#[cfg(test)]
#[path = "host_skills_test.rs"]
mod host_skills_test;
