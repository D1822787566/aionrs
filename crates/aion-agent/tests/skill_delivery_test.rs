//! Delivery regressions exercise the next provider request, not just tool strings.
mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::host_skills::{HostSkill, HostSkillContent, HostSkillReadError, HostSkillSource};
use aion_agent::orchestration::execute_tool_calls;
use aion_agent::output::null_sink::NullSink;
use aion_agent::skill_tool::SkillTool;
use aion_agent::skills::frontmatter::{parse_frontmatter, parse_skill_fields};
use aion_agent::skills::permissions::SkillPermissionChecker;
use aion_agent::skills::types::{LoadedFrom, SkillSource};
use aion_compact::CompactLevel;
use aion_protocol::ToolApprovalManager;
use aion_protocol::events::ProtocolEvent;
use aion_protocol::writer::ProtocolEmitter;
use aion_providers::{LlmProvider, ProviderError};
use aion_tools::Tool;
use aion_tools::read::ReadTool;
use aion_tools::registry::ToolRegistry;
use aion_types::llm::{LlmEvent, LlmRequest};
use aion_types::message::{ContentBlock, Role, StopReason, TokenUsage};
use async_trait::async_trait;
use common::{MockLlmProvider, test_config};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::sync::mpsc;

#[derive(Debug)]
struct Source {
    name: String,
    body: String,
    root: PathBuf,
}

#[async_trait]
impl HostSkillSource for Source {
    fn available(&self) -> Vec<HostSkill> {
        vec![HostSkill {
            name: self.name.clone(),
            description: "Test instructions".into(),
        }]
    }
    async fn read(&self, name: &str) -> Result<HostSkillContent, HostSkillReadError> {
        if name != self.name {
            return Err(HostSkillReadError::NotEnabled);
        }
        Ok(HostSkillContent {
            body: self.body.clone(),
            root: self.root.clone(),
        })
    }
}

struct Capture {
    inner: MockLlmProvider,
    requests: Arc<Mutex<Vec<LlmRequest>>>,
}

#[async_trait]
impl LlmProvider for Capture {
    async fn stream(&self, request: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.stream(request).await
    }
}

fn call(name: &str, input: Value) -> Vec<LlmEvent> {
    vec![
        LlmEvent::ToolUse {
            id: "call-1".into(),
            name: name.into(),
            input,
            extra: None,
        },
        LlmEvent::Done {
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 1_000,
                output_tokens: 50,
                ..Default::default()
            },
        },
    ]
}

struct Events;
impl ProtocolEmitter for Events {
    fn emit(&self, _: &ProtocolEvent) -> std::io::Result<()> {
        Ok(())
    }
}

async fn next_request(body: &str, window: usize, name: &str, input: Value) -> LlmRequest {
    next_request_with_mode(body, window, name, input, false).await
}

async fn next_request_with_mode(body: &str, window: usize, name: &str, input: Value, protocol: bool) -> LlmRequest {
    let dir = tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let skill_name = input["skill"].as_str().unwrap_or("test-skill").to_owned();
    let provider = Capture {
        inner: MockLlmProvider::with_turns(vec![
            call(name, input),
            vec![
                LlmEvent::TextDelta("done".into()),
                LlmEvent::Done {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                },
            ],
        ]),
        requests: requests.clone(),
    };
    let mut config = test_config();
    config.compact.enabled = false;
    config.compact.context_window = window;
    config.compact.output_reserve = 2_000;
    config.compact.emergency_buffer = 1_000;
    let mut built = AgentBootstrap::new(config, dir.path().to_string_lossy(), Arc::new(NullSink))
        .provider(Arc::new(provider))
        .host_skills(Arc::new(Source {
            name: skill_name,
            body: body.into(),
            root: dir.path().into(),
        }))
        .build()
        .await
        .unwrap();
    if protocol {
        built.engine.set_approval_manager(Arc::new(ToolApprovalManager::new()));
        built.engine.set_protocol_writer(Arc::new(Events));
    }
    built.engine.run("Load the requested instructions.", "").await.unwrap();
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    captured[1].clone()
}

#[tokio::test]
async fn skill_full_body_reaches_next_request() {
    let body = format!("{}\n## Delivery Gate\nlast instruction", "instructions\n".repeat(6_000));
    let request = next_request(&body, 200_000, "Skill", json!({"skill":"test-skill"})).await;
    assert!(request.messages.iter().any(|m| m.role == Role::User
        && m.content.iter().any(|b| {
            matches!(b, ContentBlock::Text { text } if text.contains(&body) && text.contains("END LOADED SKILL"))
        })));
    assert!(request.messages.iter().flat_map(|m| &m.content).any(|b| {
        matches!(b, ContentBlock::ToolResult { content, is_error: false, .. }
            if content.len() < 1_000 && content.contains("skill_loaded"))
    }));
}

#[tokio::test]
async fn skill_too_large_returns_explicit_chunk_request_without_partial_body() {
    let body = "secret skill instruction\n".repeat(4_000);
    let request = next_request(&body, 12_000, "Skill", json!({"skill":"test-skill"})).await;
    assert!(
        !request
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| { matches!(b, ContentBlock::Text { text } if text.contains("secret skill instruction")) })
    );
    assert!(request.messages.iter().flat_map(|m| &m.content).any(|b| {
        matches!(b, ContentBlock::ToolResult { content, is_error: true, .. }
            if content.contains("skill_requires_chunked_read") && content.contains("suggested_limit"))
    }));
}

#[tokio::test]
async fn protocol_path_delivers_full_content_or_requests_chunks() {
    let body = "instruction\n".repeat(6_000);
    for window in [200_000, 12_000] {
        let request = next_request_with_mode(&body, window, "Skill", json!({"skill":"test-skill"}), true).await;
        let texts: Vec<_> = request.messages.iter().flat_map(|m| &m.content).collect();
        if window == 200_000 {
            assert!(
                texts
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { text } if text.contains(&body)))
            );
        } else {
            assert!(texts.iter().any(|b| matches!(b, ContentBlock::ToolResult { content, is_error:true, .. } if content.contains("skill_requires_chunked_read"))));
            assert!(
                !texts
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { text } if text.contains(&body)))
            );
        }
    }
}

#[tokio::test]
async fn exec_command_still_obeys_global_limit() {
    let (shell, cmd) = if cfg!(windows) {
        ("powershell", "'x' * 20000")
    } else {
        ("sh", "printf '%020000d' 0")
    };
    let request = next_request("unused", 200_000, "ExecCommand", json!({"cmd":cmd,"shell":shell})).await;
    for block in request.messages.iter().flat_map(|m| &m.content) {
        if let ContentBlock::ToolResult { content, is_error, .. } = block {
            assert!(!is_error, "command failed: {content}");
            assert!(
                content.contains("truncated"),
                "expected truncation, got {} bytes: {}",
                content.len(),
                &content[..content.len().min(300)]
            );
        }
    }
    assert!(
        request
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .all(|b| { !matches!(b, ContentBlock::ToolResult { content, .. } if content.len() > 10_000) })
    );
}

#[tokio::test]
async fn skill_byte_chunks_reconstruct_utf8_without_executing_commands() {
    let dir = tempdir().unwrap();
    let body = format!(
        "{}\n!`echo changed > delivery-marker`\n## Delivery Gate\n",
        "中文🙂\n".repeat(3_000)
    );
    let parsed = parse_frontmatter(&body);
    let skill = parse_skill_fields(
        &parsed.frontmatter,
        &parsed.content,
        "test",
        SkillSource::Project,
        LoadedFrom::Skills,
        None,
    );
    let tool = SkillTool::new(
        Arc::new(vec![skill]),
        dir.path().into(),
        SkillPermissionChecker::new(vec![], vec![], false),
    );
    let mut offset = 0;
    let mut assembled = String::new();
    for _ in 0..1_000 {
        let output = tool
            .execute_with_follow_up(json!({"skill":"test","offset":offset,"limit":301}))
            .await;
        assert!(!output.result.is_error, "{}", output.result.content);
        let info: Value = serde_json::from_str(&output.result.content).unwrap();
        assert_eq!(info["offset"], offset);
        let ContentBlock::Text { text } = &output.follow_up_blocks[0] else {
            panic!("expected text")
        };
        let start = text.find('\n').unwrap() + 1;
        let length = info["content_bytes"].as_u64().unwrap() as usize;
        assembled.push_str(&text[start..start + length]);
        if info["complete"] == true {
            break;
        }
        let next = info["next_offset"].as_u64().unwrap();
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(assembled, body);
    assert!(!dir.path().join("delivery-marker").exists());
}

#[tokio::test]
async fn read_line_offsets_reconstruct_every_line() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("lines.txt");
    let expected: Vec<_> = (0..2_000).map(|n| format!("{n}: 中文 instructions")).collect();
    std::fs::write(&file, expected.join("\n")).unwrap();
    let tool = ReadTool::new(None);
    let mut offset = 0;
    let mut actual = Vec::new();
    for _ in 0..1_000 {
        let output = tool.execute(json!({"file_path":file,"offset":offset})).await;
        assert!(!output.is_error);
        let (content, marker) = output
            .content
            .split_once("\n\n[READ_TRUNCATED]")
            .unwrap_or((&output.content, ""));
        actual.extend(content.lines().map(|l| l.split_once('\t').unwrap().1.to_owned()));
        if marker.is_empty() {
            break;
        }
        let next = marker
            .split_whitespace()
            .find_map(|v| v.strip_prefix("next_offset="))
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert!(next > offset);
        offset = next;
    }
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn read_byte_offsets_reconstruct_long_lines_and_crlf() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("long.txt");
    let body = format!("{}\r\n\r\ntail\r\n", "中文🙂".repeat(3_000));
    std::fs::write(&file, &body).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ReadTool::new(None)));
    let confirmer = common::auto_approve_confirmer();
    let mut offset = 0;
    let mut actual = String::new();
    for _ in 0..1_000 {
        let calls = [ContentBlock::ToolUse {
            id: "read".into(),
            name: "Read".into(),
            input: json!({"file_path":file,"byte_offset":offset}),
            extra: None,
        }];
        let output = execute_tool_calls(&registry, &calls, &confirmer, None, CompactLevel::Full, true)
            .await
            .unwrap();
        let ContentBlock::ToolResult { content, is_error, .. } = &output.results[0] else {
            panic!("expected result")
        };
        assert!(!is_error);
        assert!(content.len() <= 10_000);
        if let Some((content, marker)) = content.rsplit_once("\n\n[READ_TRUNCATED]") {
            actual.push_str(content);
            let next = marker
                .split_whitespace()
                .find_map(|v| v.strip_prefix("next_byte_offset="))
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert!(next > offset);
            offset = next;
        } else {
            actual.push_str(content);
            break;
        }
    }
    assert_eq!(actual, body);
}

#[tokio::test]
#[ignore = "requires AION_SKILL_FIXTURE_DIR containing the real OfficeCLI skills"]
async fn real_officecli_skills_reach_next_request_completely() {
    let root = PathBuf::from(std::env::var_os("AION_SKILL_FIXTURE_DIR").expect("set AION_SKILL_FIXTURE_DIR"));
    for name in ["officecli-word-form", "officecli-pitch-deck"] {
        let body = std::fs::read_to_string(root.join(name).join("SKILL.md")).unwrap();
        assert!(body.len() > 50_000);
        assert!(body.contains("Delivery Gate"));
        let parsed = parse_frontmatter(&body);
        let skill = parse_skill_fields(
            &parsed.frontmatter,
            &parsed.content,
            name,
            SkillSource::Project,
            LoadedFrom::Skills,
            None,
        );
        let tool = SkillTool::new(
            Arc::new(vec![skill]),
            root.clone(),
            SkillPermissionChecker::new(vec![], vec![], false),
        );
        let native = tool.execute_with_follow_up(json!({"skill":name})).await;
        assert!(!native.result.is_error);
        assert!(
            native
                .follow_up_blocks
                .iter()
                .any(|b| matches!(b,ContentBlock::Text{text} if text.contains(&parsed.content)))
        );
        let request = next_request(&body, 200_000, "Skill", json!({"skill":name})).await;
        assert!(
            request
                .messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|b| matches!(b, ContentBlock::Text { text } if text.contains(&body)))
        );
        let small = next_request(&body, 12_000, "Skill", json!({"skill":name})).await;
        assert!(small.messages.iter().flat_map(|m| &m.content).any(|b| matches!(b, ContentBlock::ToolResult {content,is_error:true,..} if content.contains("skill_requires_chunked_read"))));
        println!(
            "{name}: {} bytes, exact full body and Delivery Gate reached next provider request; small window requests chunked reading",
            body.len()
        );
    }
}
