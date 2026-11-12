use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[tokio::test]
    async fn grep_tool_finds_pattern_in_own_source() {
        let tool = GrepTool::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let input = json!({
            "pattern": "GrepTool",
            "path": env!("CARGO_MANIFEST_DIR")
        });
        let result = tool.execute(input).await;
        assert!(!result.is_error, "grep failed: {}", result.content);
        assert!(result.content.contains("GrepTool"));
    }

    #[tokio::test]
    async fn execute_uses_cwd_for_relative_path() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("searchable.txt"), "unique_grep_marker_xyz").unwrap();

        let tool = GrepTool::new(tmp.path().to_path_buf());
        let input = json!({"pattern": "unique_grep_marker_xyz", "path": "."});
        let result = tool.execute(input).await;
        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert!(
            result.content.contains("unique_grep_marker_xyz"),
            "should find pattern, got: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn execute_applies_glob_ignore_case_and_binary_rules() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(tmp.path().join("src/main.rs"), "Needle in source\n").unwrap();
        fs::write(tmp.path().join("notes.txt"), "Needle in notes\n").unwrap();
        fs::write(tmp.path().join("ignored.rs"), "Needle in ignored\n").unwrap();
        fs::write(tmp.path().join("binary.rs"), b"Needle\0not text\n").unwrap();

        let tool = GrepTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(json!({
                "pattern": "needle",
                "path": ".",
                "glob": "*.rs",
                "case_insensitive": true
            }))
            .await;

        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert!(result.content.contains("main.rs"));
        assert!(!result.content.contains("notes.txt"));
        assert!(!result.content.contains("ignored.rs"));
        assert!(!result.content.contains("binary.rs"));
    }

    #[tokio::test]
    async fn execute_rejects_invalid_pattern_and_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = GrepTool::new(tmp.path().to_path_buf());

        let invalid = tool.execute(json!({"pattern": "("})).await;
        assert!(invalid.is_error);
        assert!(invalid.content.contains("Invalid regex"));

        let missing = tool.execute(json!({"pattern": "needle", "path": "missing"})).await;
        assert!(missing.is_error);
        assert!(missing.content.contains("does not exist"));
    }

    #[tokio::test]
    async fn execute_caps_results_at_250_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let contents = (0..300)
            .map(|index| format!("needle-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(tmp.path().join("many.txt"), contents).unwrap();

        let tool = GrepTool::new(tmp.path().to_path_buf());
        let result = tool.execute(json!({"pattern": "needle", "path": "."})).await;
        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert_eq!(result.content.lines().count(), 250);
    }
}
