//! Validated immutable host contract. No subprocess or environment lookup here.
use crate::validation::{ToolInputValidationError, check_tool_schema, validate_tool_input};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
pub struct OperationContract {
    entries: Result<Arc<BTreeMap<String, Value>>, &'static str>,
}

impl OperationContract {
    pub fn new(value: Value) -> Self {
        let entries = Self::parse(value);
        if let Err(code) = entries {
            tracing::warn!(code, "Aion operation contract rejected");
        }
        Self { entries }
    }

    fn parse(value: Value) -> Result<Arc<BTreeMap<String, Value>>, &'static str> {
        if value["contract_version"] != 1 {
            return Err("aion_operation_contract_version_unsupported");
        }
        let descriptors = value["operations"]
            .as_array()
            .ok_or("aion_operation_contract_invalid")?;
        let mut entries = BTreeMap::new();
        for entry in descriptors {
            let name = entry["operation"].as_str().ok_or("aion_operation_contract_invalid")?;
            if !super::aion_operation::OPERATION_PATHS.contains(&name)
                || check_tool_schema(&entry["input_schema"]).is_err()
                || entry["requires_confirmation"].as_bool().is_none()
                || entries.insert(name.to_owned(), entry.clone()).is_some()
            {
                return Err("aion_operation_contract_invalid");
            }
        }
        if entries.len() != super::aion_operation::OPERATION_PATHS.len() {
            return Err("aion_operation_contract_invalid");
        }
        Ok(Arc::new(entries))
    }

    pub(crate) fn descriptor(&self, operation: &str) -> Result<&Value, &'static str> {
        self.entries
            .as_ref()
            .map_err(|code| *code)?
            .get(operation)
            .ok_or("aion_operation_unknown")
    }
}

pub(crate) fn local_schema(operation: &str) -> Option<Value> {
    match operation {
        "operation.describe" => {
            let mut operations = super::aion_operation::OPERATION_PATHS.to_vec();
            operations.extend(["operation.describe", "result.read"]);
            Some(
                json!({"type":"object","additionalProperties":false,"required":["operation"],"properties":{"operation":{"type":"string","enum":operations}}}),
            )
        }
        "result.read" => Some(
            json!({"type":"object","additionalProperties":false,"required":["result_id"],"properties":{
                "result_id":{"type":"string","minLength":1},"path":{"type":["string","null"]},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1}
            }}),
        ),
        _ => None,
    }
}

pub(crate) fn local_descriptor(operation: &str) -> Option<Value> {
    let schema = local_schema(operation)?;
    let example = if operation == "operation.describe" {
        json!({"operation":"config.mcp.servers.get"})
    } else {
        json!({"result_id":"result_id_from_delivery","path":"","offset":0,"limit":20})
    };
    Some(
        json!({"operation":operation,"contract_version":1,"input_schema":schema,"examples":[example],"selectors":[],"requires_confirmation":false,"description":"Local read-only metadata or immutable result continuation; never executes a helper operation."}),
    )
}

pub(crate) fn validate_operation_input(schema: &Value, input: &Value) -> Result<(), Vec<ToolInputValidationError>> {
    validate_tool_input(schema, input).map_err(|mut errors| {
        for error in &mut errors {
            error.path = format!("/input{}", if error.path == "/" { "" } else { &error.path });
        }
        errors
    })
}
