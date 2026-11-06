use std::collections::HashSet;

use aion_types::tool::ToolDef;

use crate::Tool;

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    loaded_deferred: HashSet<String>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            loaded_deferred: HashSet::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Mark deferred tools returned by ToolSearch as available with full schemas.
    pub fn mark_deferred_loaded<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.loaded_deferred.extend(names.into_iter().map(Into::into));
    }

    /// Find a tool by name
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
    }

    /// Get all registered tool names
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Resolve a server tool's actual advertised name after collision handling.
    pub fn mcp_tool_name(&self, server: &str, original: &str) -> Option<String> {
        self.tools
            .iter()
            .find(|tool| tool.mcp_origin() == Some((server, original)))
            .map(|tool| tool.name().to_owned())
    }

    /// Remove only proxies owned by this server before replacing its connection.
    pub fn remove_mcp_server(&mut self, server: &str) {
        self.tools
            .retain(|tool| !matches!(tool.mcp_origin(), Some((name, _)) if name == server));
        let names: HashSet<_> = self.tools.iter().map(|tool| tool.name()).collect();
        self.loaded_deferred.retain(|name| names.contains(name.as_str()));
    }

    /// Generate API tool definitions for all registered tools
    pub fn to_tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
                deferred: t.is_deferred() && !self.loaded_deferred.contains(t.name()),
            })
            .collect()
    }

    /// Generate API tool definitions for tools matching a predicate.
    ///
    /// Used by plan mode to restrict the tool set sent to the LLM.
    pub fn to_tool_defs_filtered<F>(&self, filter: F) -> Vec<ToolDef>
    where
        F: Fn(&dyn Tool) -> bool,
    {
        self.tools
            .iter()
            .filter(|t| filter(t.as_ref()))
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
                deferred: t.is_deferred() && !self.loaded_deferred.contains(t.name()),
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "registry_test.rs"]
mod registry_test;
