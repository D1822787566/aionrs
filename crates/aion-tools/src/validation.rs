use serde_json::Value;

/// One schema violation returned to the model without exposing the invalid value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInputValidationError {
    pub path: String,
    pub expected: String,
    pub actual: String,
    pub message: String,
}

/// Validate the JSON Schema subset used by registered tools.
pub fn validate_tool_input(schema: &Value, input: &Value) -> Result<(), Vec<ToolInputValidationError>> {
    let mut errors = Vec::new();
    validate_value(schema, input, "/".to_string(), &mut errors);
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

fn validate_value(schema: &Value, input: &Value, path: String, errors: &mut Vec<ToolInputValidationError>) {
    if errors.len() >= 8 {
        return;
    }
    for (keyword, exact) in [("oneOf", true), ("anyOf", false)] {
        if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
            let results: Vec<Vec<ToolInputValidationError>> = branches
                .iter()
                .map(|branch| {
                    let mut local = Vec::new();
                    validate_value(branch, input, path.clone(), &mut local);
                    local
                })
                .collect();
            let matches = results.iter().filter(|result| result.is_empty()).count();
            if matches == 0 || (exact && matches != 1) {
                if matches == 0 {
                    // Prefer the branch with the matching discriminator, then the
                    // fewest errors, so nested fields keep their precise pointer.
                    if let Some(best) = results.into_iter().min_by_key(|items| {
                        items.len()
                            + 100
                                * items
                                    .iter()
                                    .filter(|e| {
                                        e.message == "value differs from the declared constant"
                                            || e.message == "value is outside the declared enum"
                                    })
                                    .count()
                    }) {
                        errors.extend(best.into_iter().take(8 - errors.len()));
                        return;
                    }
                }
                errors.push(ToolInputValidationError {
                    path: path.clone(),
                    expected: format!(
                        "{} {keyword} branch",
                        if exact { "exactly one" } else { "at least one" }
                    ),
                    actual: format!("{matches} matching branches"),
                    message: "value does not match the declared alternatives".into(),
                });
                return;
            }
        }
    }
    if let Some(constant) = schema.get("const")
        && constant != input
    {
        errors.push(ToolInputValidationError {
            path,
            expected: format!("constant {constant}"),
            actual: json_type_name(input),
            message: "value differs from the declared constant".into(),
        });
        return;
    }
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
        && input.as_f64().is_some_and(|number| number < minimum)
    {
        errors.push(ToolInputValidationError {
            path,
            expected: format!("number >= {minimum}"),
            actual: "below minimum".into(),
            message: "number is below the declared minimum".into(),
        });
        return;
    }
    if let Some(types) = schema.get("type") {
        let valid = types.as_str().map(|kind| matches_type(kind, input)).unwrap_or_else(|| {
            types.as_array().is_some_and(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|kind| matches_type(kind, input))
            })
        });
        if !valid {
            let expected = types.as_str().map(str::to_owned).unwrap_or_else(|| types.to_string());
            errors.push(ToolInputValidationError {
                path,
                expected,
                actual: json_type_name(input),
                message: "value has the wrong JSON type".to_string(),
            });
            return;
        }
    }

    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.iter().any(|option| option == input)
    {
        errors.push(ToolInputValidationError {
            path,
            expected: "one of the declared enum values".to_string(),
            actual: json_type_name(input),
            message: "value is outside the declared enum".to_string(),
        });
        return;
    }

    if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
        && input.as_array().is_some_and(|items| items.len() < min as usize)
    {
        errors.push(ToolInputValidationError {
            path,
            expected: format!("at least {min} items"),
            actual: "fewer items".to_string(),
            message: "array has too few items".to_string(),
        });
        return;
    }

    if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
        && input.as_str().is_some_and(|value| value.chars().count() < min as usize)
    {
        errors.push(ToolInputValidationError {
            path,
            expected: format!("at least {min} characters"),
            actual: "shorter string".to_string(),
            message: "string is too short".to_string(),
        });
        return;
    }

    if let Some(required) = schema.get("required").and_then(Value::as_array)
        && let Some(object) = input.as_object()
    {
        for property in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(property) {
                errors.push(ToolInputValidationError {
                    path: child_path(&path, property),
                    expected: format!("required property {property}"),
                    actual: "missing".to_string(),
                    message: format!("missing required property {property}"),
                });
            }
        }
    }

    if let Some(object) = input.as_object() {
        let empty = serde_json::Map::new();
        let properties = schema.get("properties").and_then(Value::as_object).unwrap_or(&empty);
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            for property in object.keys().filter(|property| !properties.contains_key(*property)) {
                errors.push(ToolInputValidationError {
                    path: child_path(&path, property),
                    expected: "only declared properties".to_string(),
                    actual: format!("unexpected property {property}"),
                    message: "object contains an undeclared property".to_string(),
                });
            }
        }
        for (property, property_schema) in properties {
            if let Some(value) = object.get(property) {
                validate_value(property_schema, value, child_path(&path, property), errors);
            }
        }
        if let Some(extra_schema) = schema.get("additionalProperties").filter(|value| value.is_object()) {
            for (property, value) in object.iter().filter(|(key, _)| !properties.contains_key(*key)) {
                validate_value(extra_schema, value, child_path(&path, property), errors);
            }
        }
    }

    if let (Some(item_schema), Some(items)) = (schema.get("items"), input.as_array()) {
        for (index, item) in items.iter().enumerate() {
            let item_path = if path == "/" {
                format!("/{index}")
            } else {
                format!("{path}/{index}")
            };
            validate_value(item_schema, item, item_path, errors);
        }
    }
}

fn child_path(path: &str, property: &str) -> String {
    format!(
        "{}/{}",
        path.trim_end_matches('/'),
        property.replace('~', "~0").replace('/', "~1")
    )
}

/// Reject unsupported contract constraints rather than silently weakening validation.
pub fn check_tool_schema(schema: &Value) -> Result<(), String> {
    if schema.is_boolean() {
        return Err("boolean schemas are not supported".into());
    }
    let object = schema.as_object().ok_or("schema must be an object")?;
    for (key, value) in object {
        match key.as_str() {
            "type" => {
                let types = value.as_array().cloned().unwrap_or_else(|| vec![value.clone()]);
                if types.is_empty()
                    || types.iter().any(|kind| {
                        !matches!(
                            kind.as_str(),
                            Some("null" | "boolean" | "integer" | "number" | "string" | "array" | "object")
                        )
                    })
                {
                    return Err("unsupported schema type".into());
                }
            }
            "properties" => {
                for child in value.as_object().ok_or("properties must be an object")?.values() {
                    check_tool_schema(child)?;
                }
            }
            "items" => check_tool_schema(value)?,
            "additionalProperties" if value.is_object() => check_tool_schema(value)?,
            "additionalProperties" if value.is_boolean() => {}
            "oneOf" | "anyOf" => {
                let branches = value
                    .as_array()
                    .filter(|branches| !branches.is_empty())
                    .ok_or("alternatives must be a nonempty array")?;
                for child in branches {
                    check_tool_schema(child)?;
                }
            }
            "required" if value.as_array().is_some_and(|items| items.iter().all(Value::is_string)) => {}
            "enum" if value.as_array().is_some_and(|items| !items.is_empty()) => {}
            "minItems" | "minLength" if value.as_u64().is_some() => {}
            "minimum" if value.is_number() => {}
            "const" | "description" | "title" | "default" | "examples" | "$schema" => {}
            _ => return Err(format!("unsupported schema keyword: {key}")),
        }
    }
    Ok(())
}

fn matches_type(kind: &str, value: &Value) -> bool {
    match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

fn json_type_name(value: &Value) -> String {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
    .to_string()
}

/// Format a model-facing, value-free validation result.
pub fn format_tool_input_error(tool_name: &str, errors: &[ToolInputValidationError]) -> String {
    let items: Vec<Value> = errors
        .iter()
        .map(|error| {
            serde_json::json!({
                "path": error.path, "expected": error.expected, "actual": error.actual, "message": error.message,
            })
        })
        .collect();
    serde_json::json!({
        "code": "invalid_tool_arguments", "tool": tool_name, "executed": false,
        "errors": items,
        "instruction": "Regenerate this tool call with arguments matching the tool schema. Do not encode an array or object as a JSON string.",
    }).to_string()
}

#[cfg(test)]
#[path = "validation_test.rs"]
mod validation_test;
