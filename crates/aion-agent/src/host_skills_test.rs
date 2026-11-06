use super::*;
use aion_types::message::ContentBlock;

#[derive(Debug)]
struct Source;

#[async_trait]
impl HostSkillSource for Source {
    fn available(&self) -> Vec<HostSkill> {
        vec![HostSkill {
            name: "form".into(),
            description: "Fillable forms".into(),
        }]
    }
    async fn read(&self, name: &str) -> Result<HostSkillContent, HostSkillReadError> {
        if name != "form" {
            return Err(HostSkillReadError::NotEnabled);
        }
        Ok(HostSkillContent {
            root: PathBuf::from("skill-root"),
            body: "Literal $ARGUMENTS and !`not a command`".into(),
        })
    }
}

#[tokio::test]
async fn host_read_keeps_instructions_literal_and_enforces_deny() {
    let tool = HostedSkillTool::new(Arc::new(Source), &[]);
    let result = tool.execute(json!({"skill":"form","args":"a; b"})).await;
    assert!(!result.is_error);
    assert!(result.content.contains("skill-root"));
    assert!(result.content.contains("$ARGUMENTS and !`not a command`"));
    assert!(result.content.contains("a; b"));
    let missing = tool.execute(json!({"skill":"other"})).await;
    assert!(missing.is_error && missing.content.contains("skill_not_enabled"));
    let denied = HostedSkillTool::new(Arc::new(Source), &["form".into()]);
    assert!(denied.available().is_empty());
    assert!(
        denied
            .execute(json!({"skill":"form"}))
            .await
            .content
            .contains("skill_denied")
    );
}

#[tokio::test]
async fn host_large_skill_uses_follow_up_context_without_truncating_the_tool_summary() {
    let body = "x".repeat(20_000);
    let source = Arc::new(SourceWithBody { body });
    let tool = HostedSkillTool::new(source.clone(), &[]);
    let output = tool.execute_with_follow_up(json!({"skill":"form"})).await;

    assert!(!output.result.is_error);
    assert!(output.result.content.contains("skill_loaded"));
    assert!(output.result.content.len() < 1_000);
    assert_eq!(output.follow_up_blocks.len(), 1);
    let ContentBlock::Text { text } = &output.follow_up_blocks[0] else {
        panic!("expected skill text")
    };
    assert!(text.contains(&"x".repeat(20_000)));
}

#[derive(Debug)]
struct SourceWithBody {
    body: String,
}

#[async_trait]
impl HostSkillSource for SourceWithBody {
    fn available(&self) -> Vec<HostSkill> {
        vec![]
    }
    async fn read(&self, name: &str) -> Result<HostSkillContent, HostSkillReadError> {
        if name != "form" {
            return Err(HostSkillReadError::NotEnabled);
        }
        Ok(HostSkillContent {
            root: PathBuf::from("skill-root"),
            body: self.body.clone(),
        })
    }
}
