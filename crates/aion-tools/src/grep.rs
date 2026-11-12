use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use glob::Pattern;
use ignore::{DirEntry, WalkBuilder, WalkState};
use regex::bytes::{Regex, RegexBuilder};
use serde_json::{Value, json};
use tokio::task;

use aion_protocol::events::ToolCategory;
use aion_types::tool::{JsonSchema, ToolResult};

use crate::Tool;

const MAX_RESULT_LINES: usize = 250;

pub struct GrepTool {
    cwd: PathBuf,
}

impl GrepTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Searches file contents using regex patterns in-process.\n\n\
         IMPORTANT: ALWAYS use this Grep tool for content search. \
         NEVER run grep or rg as a ExecCommand command.\n\n\
         - Supports full regex syntax (e.g., \"log.*Error\", \"fn\\\\s+\\\\w+\").\n\
         - Use the glob parameter to filter by file pattern (e.g., \"*.rs\").\n\
         - Output is truncated to 250 lines.\n\
         - Set case_insensitive to true for case-insensitive search."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "The regex pattern to search for" },
                "path": { "type": "string", "description": "Directory to search in (default: cwd)" },
                "glob": { "type": "string", "description": "File filter pattern, e.g. \"*.rs\"" },
                "case_insensitive": { "type": "boolean", "description": "Case insensitive search" }
            },
            "required": ["pattern"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(pattern) = input["pattern"].as_str() else {
            return ToolResult {
                content: "Missing required parameter: pattern".to_string(),
                is_error: true,
            };
        };

        let raw_path = input["path"].as_str().unwrap_or(".");
        let path = if Path::new(raw_path).is_relative() {
            self.cwd.join(raw_path)
        } else {
            PathBuf::from(raw_path)
        };
        let glob_pattern = input["glob"].as_str().map(str::to_owned);
        let case_insensitive = input["case_insensitive"].as_bool().unwrap_or(false);
        let pattern = pattern.to_owned();

        tracing::debug!(cwd = %self.cwd.display(), resolved_path = %path.display(), pattern = %pattern, "GrepTool searching in-process");

        match task::spawn_blocking(move || search_files(&pattern, &path, glob_pattern.as_deref(), case_insensitive))
            .await
        {
            Ok(result) => result,
            Err(error) => ToolResult {
                content: format!("Grep search task failed: {error}"),
                is_error: true,
            },
        }
    }

    fn max_result_size(&self) -> usize {
        20_000
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }

    fn describe(&self, input: &Value) -> String {
        let pattern = input.get("pattern").and_then(Value::as_str).unwrap_or("");
        let raw_path = input.get("path").and_then(Value::as_str).unwrap_or(".");
        format!("Grep '{}' in {}", pattern, raw_path)
    }
}

fn search_files(pattern: &str, path: &Path, glob_pattern: Option<&str>, case_insensitive: bool) -> ToolResult {
    let regex = match RegexBuilder::new(pattern).case_insensitive(case_insensitive).build() {
        Ok(regex) => Arc::new(regex),
        Err(error) => {
            return ToolResult {
                content: format!("Invalid regex: {error}"),
                is_error: true,
            };
        }
    };
    let glob = match glob_pattern.map(Pattern::new).transpose() {
        Ok(glob) => glob,
        Err(error) => {
            return ToolResult {
                content: format!("Invalid glob: {error}"),
                is_error: true,
            };
        }
    };
    if !path.exists() {
        return ToolResult {
            content: format!("Grep path does not exist: {}", path.display()),
            is_error: true,
        };
    }

    let results = Arc::new(Mutex::new(Vec::with_capacity(MAX_RESULT_LINES)));
    let root = Arc::new(path.to_path_buf());
    let glob = Arc::new(glob);
    let root_for_walk = Arc::clone(&root);
    let glob_for_walk = Arc::clone(&glob);
    let results_for_walk = Arc::clone(&results);
    let regex_for_walk = Arc::clone(&regex);
    let walker = WalkBuilder::new(path)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .build_parallel();

    walker.run(|| {
        let results = Arc::clone(&results_for_walk);
        let regex = Arc::clone(&regex_for_walk);
        let root = Arc::clone(&root_for_walk);
        let glob = Arc::clone(&glob_for_walk);
        Box::new(move |entry: Result<DirEntry, ignore::Error>| {
            if results
                .lock()
                .map(|items| items.len() >= MAX_RESULT_LINES)
                .unwrap_or(true)
            {
                return WalkState::Quit;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::debug!(error = %error, "Grep walker skipped an entry");
                    return WalkState::Continue;
                }
            };
            if entry.file_type().map(|file_type| file_type.is_file()) != Some(true) {
                return WalkState::Continue;
            }
            if !matches_glob(glob.as_ref().as_ref(), root.as_path(), entry.path()) {
                return WalkState::Continue;
            }
            append_matches(&regex, entry.path(), &results)
        })
    });

    let lines = results.lock().map(|items| items.clone()).unwrap_or_default();
    ToolResult {
        content: if lines.is_empty() {
            "No matches found".to_string()
        } else {
            lines.join("\n")
        },
        is_error: false,
    }
}

fn matches_glob(pattern: Option<&Pattern>, root: &Path, candidate: &Path) -> bool {
    let Some(pattern) = pattern else { return true };
    let relative = candidate.strip_prefix(root).unwrap_or(candidate);
    let normalized = relative.to_string_lossy().replace('\\', "/");
    pattern.matches(&normalized) || pattern.matches(candidate.file_name().and_then(|name| name.to_str()).unwrap_or(""))
}

fn append_matches(regex: &Regex, path: &Path, results: &Mutex<Vec<String>>) -> WalkState {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            tracing::debug!(path = %path.display(), error = %error, "Grep could not read a file");
            return WalkState::Continue;
        }
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut line_number = 0usize;
    let mut file_matches = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                line_number += 1;
                if line.contains(&0) {
                    return WalkState::Continue;
                }
                if regex.is_match(&line) {
                    let line_end = line
                        .iter()
                        .rposition(|byte| *byte != b'\r' && *byte != b'\n')
                        .map(|index| index + 1)
                        .unwrap_or(0);
                    file_matches.push(format!(
                        "{}:{}:{}",
                        path.display(),
                        line_number,
                        String::from_utf8_lossy(&line[..line_end])
                    ));
                }
            }
            Err(error) => {
                tracing::debug!(path = %path.display(), error = %error, "Grep stopped reading a file");
                break;
            }
        }
    }

    let mut items = match results.lock() {
        Ok(items) => items,
        Err(_) => return WalkState::Quit,
    };
    for rendered in file_matches {
        if items.len() >= MAX_RESULT_LINES {
            return WalkState::Quit;
        }
        items.push(rendered);
    }
    WalkState::Continue
}

#[cfg(test)]
#[path = "grep_test.rs"]
mod grep_test;
