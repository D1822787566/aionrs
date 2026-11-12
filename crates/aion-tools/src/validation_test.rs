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
    assert_eq!(errors[0].path, "/tasks/0/prompt");
}

#[test]
fn validates_alternatives_constants_minimum_and_dictionary_values() {
    let schema = json!({"oneOf":[
        {"type":"object","required":["kind","count"],"properties":{"kind":{"const":"a"},"count":{"type":"integer","minimum":1}}},
        {"type":"object","required":["kind","env"],"properties":{"kind":{"const":"b"},"env":{"type":"object","additionalProperties":{"type":"string"}}}}
    ]});
    check_tool_schema(&schema).unwrap();
    assert!(validate_tool_input(&schema, &json!({"kind":"a","count":1})).is_ok());
    assert!(validate_tool_input(&schema, &json!({"kind":"b","env":{"TOKEN":"fixture"}})).is_ok());
    for bad in [
        json!({"kind":"a","count":0}),
        json!({"kind":"b","env":{"TOKEN":3}}),
        json!({"kind":"c"}),
    ] {
        assert!(validate_tool_input(&schema, &bad).is_err());
    }
    let overlap = json!({"oneOf":[{},{}]});
    assert!(validate_tool_input(&overlap, &json!({})).is_err());
    assert!(validate_tool_input(&json!({"anyOf":[{},{}]}), &json!({})).is_ok());
}

#[test]
fn paths_escape_json_pointer_segments_and_reject_unknown_contract_constraints() {
    let errors = validate_tool_input(
        &json!({"type":"object","properties":{"a/b~c":{"type":"object","additionalProperties":{"type":"integer"}}}}),
        &json!({"a/b~c":{"x/y":"private-value"}}),
    )
    .unwrap_err();
    assert_eq!(errors[0].path, "/a~1b~0c/x~1y");
    assert!(!format_tool_input_error("fixture", &errors).contains("private-value"));
    assert!(check_tool_schema(&json!({"type":"string","pattern":".+"})).is_err());
    assert!(check_tool_schema(&json!({"type":"typo"})).is_err());
    assert!(check_tool_schema(&json!({"properties":{"nested":{"maximum":5}}})).is_err());
}

#[test]
fn formats_value_free_model_error() {
    let errors = validate_tool_input(&schema(), &json!({"tasks": "[]"})).unwrap_err();
    let formatted = format_tool_input_error("Spawn", &errors);
    assert!(formatted.contains("\"executed\":false"));
    assert!(formatted.contains("\"actual\":\"string\""));
    assert!(!formatted.contains("[]"));
}
