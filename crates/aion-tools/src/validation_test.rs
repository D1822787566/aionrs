use super::*;
use serde_json::json;

fn schema() -> Value {
    json!({
        "type": "object",
        "required": ["tasks"],
        "properties": {"tasks": {"type": "array", "items": {"type": "object", "required": ["prompt"], "properties": {"prompt": {"type": "string"}}}}}
    })
}

#[test]
fn reports_array_as_expected_type_without_executing() {
    let errors = validate_tool_input(&schema(), &json!({"tasks": "[]"})).unwrap_err();
    assert_eq!(errors[0].path, "/tasks");
    assert_eq!(errors[0].actual, "string");
    assert_eq!(errors[0].expected, "array");
}

#[test]
fn reports_nested_required_property() {
    let errors = validate_tool_input(&schema(), &json!({"tasks": [{}]})).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.path.contains("tasks") && error.message.contains("prompt"))
    );
}

#[test]
fn formats_value_free_model_error() {
    let errors = validate_tool_input(&schema(), &json!({"tasks": "[]"})).unwrap_err();
    let formatted = format_tool_input_error("Spawn", &errors);
    assert!(formatted.contains("\"executed\":false"));
    assert!(formatted.contains("\"actual\":\"string\""));
    assert!(!formatted.contains("[]"));
}
