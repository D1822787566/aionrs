use super::*;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reading_a_failed_snapshot_preserves_the_original_failure_budget() {
        let calls = [ContentBlock::ToolUse {
            id: "read".into(),
            name: "AionOperation".into(),
            input: json!({"operation":"result.read","input":{"result_id":"fixture"}}),
            extra: None,
        }];
        let failed_snapshot = [ContentBlock::ToolResult {
            tool_use_id: "read".into(),
            content: json!({"success":false,"delivery":{"result_id":"fixture","execution_status":"unknown"}})
                .to_string(),
            is_error: true,
        }];
        assert!(metadata_only_round(&calls, &failed_snapshot));
        let expired = [ContentBlock::ToolResult {
            tool_use_id: "read".into(),
            content: json!({"success":false,"code":"aion_operation_result_expired"}).to_string(),
            is_error: true,
        }];
        assert!(!metadata_only_round(&calls, &expired));
        let mut guards = crate::turn::TurnGuards::new(None, 3, 20);
        for index in 0..8 {
            let action = guards.after_tool_round_with_preserve(
                None,
                failure_fingerprint(&format!("failure-{index}")),
                true,
                false,
            );
            assert_eq!(guards.all_error_tool_round_count(), index + 1);
            if index == 7 {
                assert!(matches!(action, crate::turn::TurnGuardAction::Finalize(_)));
                break;
            }
            guards.after_tool_round_with_preserve(None, None, true, metadata_only_round(&calls, &failed_snapshot));
            assert_eq!(guards.all_error_tool_round_count(), index + 1);
        }
    }

    fn failure_fingerprint(command: &str) -> Option<ToolCallFailureFingerprint> {
        tool_call_failure_fingerprint(&[ContentBlock::ToolUse {
            id: format!("call-{command}"),
            name: "ExecCommand".into(),
            input: json!({ "cmd": command }),
            extra: None,
        }])
    }

    #[test]
    fn reason_detects_blank_name_before_blank_id() {
        assert_eq!(
            tool_call_malformed_reason("", ""),
            Some(ToolCallMalformedReason::EmptyFunctionName)
        );
        assert_eq!(
            tool_call_malformed_reason("call_1", "   "),
            Some(ToolCallMalformedReason::EmptyFunctionName)
        );
    }

    #[test]
    fn reason_detects_blank_id() {
        assert_eq!(
            tool_call_malformed_reason(" ", "Read"),
            Some(ToolCallMalformedReason::EmptyToolCallId)
        );
    }

    #[test]
    fn tracker_counts_only_same_fingerprint() {
        let call = ContentBlock::ToolUse {
            id: "bad".into(),
            name: "".into(),
            input: json!({}),
            extra: None,
        };
        let fingerprint = tool_call_malformed_fingerprint(&[call], &[Some(ToolCallMalformedReason::EmptyFunctionName)]);
        let mut tracker = ToolCallMalformedTracker::new(3);

        assert_eq!(tracker.observe(fingerprint.clone()), 1);
        assert_eq!(tracker.observe(fingerprint), 2);
        assert_eq!(tracker.observe(None), 0);
    }

    #[test]
    fn tool_call_malformed_tracker_limit_zero_disables_breaker() {
        let call = ContentBlock::ToolUse {
            id: "bad".into(),
            name: "".into(),
            input: json!({}),
            extra: None,
        };
        let fingerprint = tool_call_malformed_fingerprint(&[call], &[Some(ToolCallMalformedReason::EmptyFunctionName)]);
        let mut tracker = ToolCallMalformedTracker::new(0);

        assert_eq!(tracker.observe(fingerprint.clone()), 1);
        assert!(!tracker.is_limit_exceeded());
        assert_eq!(tracker.observe(fingerprint), 2);
        assert!(!tracker.is_limit_exceeded());
    }

    #[test]
    fn tool_call_failure_tracker_counts_only_same_fingerprint() {
        let command_a = ContentBlock::ToolUse {
            id: "call-a".into(),
            name: "ExecCommand".into(),
            input: json!({ "cmd": "python update_config.py" }),
            extra: None,
        };
        let command_a_reissued = ContentBlock::ToolUse {
            id: "call-a-reissued".into(),
            name: "ExecCommand".into(),
            input: json!({ "cmd": "python update_config.py" }),
            extra: None,
        };
        let command_b = ContentBlock::ToolUse {
            id: "call-b".into(),
            name: "ExecCommand".into(),
            input: json!({ "cmd": "aioncore assistants update" }),
            extra: None,
        };
        let command_a = tool_call_failure_fingerprint(&[command_a]);
        let command_a_reissued = tool_call_failure_fingerprint(&[command_a_reissued]);
        let command_b = tool_call_failure_fingerprint(&[command_b]);
        let mut tracker = ToolCallFailureTracker::new(3);

        assert_eq!(tracker.observe(command_a.clone()), 1);
        assert_eq!(tracker.observe(command_a_reissued), 2);
        assert_eq!(tracker.observe(command_b), 1);
        assert_eq!(tracker.count(), 1);
        assert!(!tracker.is_limit_exceeded());
        assert_eq!(tracker.observe(None), 0);
        assert_eq!(tracker.observe(command_a.clone()), 1);
        assert_eq!(tracker.observe(command_a.clone()), 2);
        assert_eq!(tracker.observe(command_a), 3);
        assert!(tracker.is_limit_exceeded());
        assert_eq!(tracker.limit(), 3);
    }

    #[test]
    fn tool_call_failure_tracker_limit_zero_disables_breaker() {
        let call = ContentBlock::ToolUse {
            id: "call-a".into(),
            name: "ExecCommand".into(),
            input: json!({ "cmd": "python update_config.py" }),
            extra: None,
        };
        let fingerprint = tool_call_failure_fingerprint(&[call]);
        let mut tracker = ToolCallFailureTracker::new(0);

        assert_eq!(tracker.observe(fingerprint.clone()), 1);
        assert!(!tracker.is_limit_exceeded());
        assert_eq!(tracker.observe(fingerprint), 2);
        assert!(!tracker.is_limit_exceeded());
    }

    #[test]
    fn all_error_round_tracker_counts_consecutive_rounds_and_resets_on_progress() {
        let mut tracker = ToolCallAllErrorRoundTracker::new(3);

        assert_eq!(tracker.observe(true), 1);
        assert_eq!(tracker.observe(true), 2);
        assert!(!tracker.is_limit_exceeded());
        assert_eq!(tracker.observe(false), 0);
        assert_eq!(tracker.observe(true), 1);
        assert_eq!(tracker.observe(true), 2);
        assert_eq!(tracker.observe(true), 3);
        assert!(tracker.is_limit_exceeded());
        assert_eq!(tracker.limit(), 3);
    }

    #[test]
    fn cycle_tracker_detects_two_and_three_repetitions() {
        let mut tracker = ToolCallCycleTracker::new(true);
        let a = failure_fingerprint("command-a");
        let b = failure_fingerprint("command-b");

        assert_eq!(tracker.observe(a.clone()), None);
        assert_eq!(tracker.observe(b.clone()), None);
        assert_eq!(tracker.observe(a.clone()), None);
        assert_eq!(
            tracker.observe(b.clone()),
            Some(ToolCallCycle {
                period: 2,
                repetitions: 2,
            })
        );
        assert_eq!(
            tracker.observe(a),
            Some(ToolCallCycle {
                period: 2,
                repetitions: 2,
            })
        );
        assert_eq!(
            tracker.observe(b),
            Some(ToolCallCycle {
                period: 2,
                repetitions: 3,
            })
        );
    }

    #[test]
    fn cycle_tracker_resets_on_non_failure_round() {
        let mut tracker = ToolCallCycleTracker::new(true);
        let a = failure_fingerprint("command-a");
        let b = failure_fingerprint("command-b");

        tracker.observe(a.clone());
        tracker.observe(b.clone());
        tracker.observe(a.clone());
        assert_eq!(tracker.observe(None), None);
        assert_eq!(tracker.observe(b), None);
        assert_eq!(tracker.observe(a), None);
    }

    #[test]
    fn disabled_cycle_tracker_never_accumulates_history() {
        let mut tracker = ToolCallCycleTracker::new(false);
        let a = failure_fingerprint("command-a");
        let b = failure_fingerprint("command-b");

        for fingerprint in [a.clone(), b.clone(), a, b] {
            assert_eq!(tracker.observe(fingerprint), None);
        }
    }
}
