use super::textual_tool_call;

#[test]
fn only_complete_allowed_calls_outside_examples_are_detected() {
    let xml = "<tool_call><function=Write><parameter=content>x</parameter></function></tool_call>";
    let allowed = |name: &str| name == "Write";
    assert!(textual_tool_call(xml, allowed));
    assert!(textual_tool_call(
        "<tool_call>{\"name\":\"Write\",\"arguments\":{}}</tool_call>",
        allowed
    ));
    for example in [
        format!("```xml\n{xml}\n```"),
        format!("`{xml}`"),
        format!("> {xml}"),
        format!("    {xml}"),
    ] {
        assert!(!textual_tool_call(&example, allowed));
    }
    assert!(!textual_tool_call(xml, |_| false));
    assert!(!textual_tool_call("I will call Write now", allowed));
    assert!(!textual_tool_call("<tool_call><function=Write>", allowed));
}
