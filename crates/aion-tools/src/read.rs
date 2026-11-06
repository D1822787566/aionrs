use std::path::Path;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::{Value, json};

use aion_protocol::events::ToolCategory;
use aion_types::file_state::FileState;
use aion_types::tool::{JsonSchema, ToolResult};

use crate::file_cache::{FileStateCache, file_mtime_ms};
use crate::{Tool, truncate_utf8};

/// Stub returned when a file has not changed since the model last read it.
/// Saves tokens by avoiding re-sending identical content.
const FILE_UNCHANGED_STUB: &str = "File unchanged since last read. The content from the earlier Read \
     tool_result in this conversation is still current — refer to that \
     instead of re-reading.";
const READ_CHUNK_MAX_BYTES: usize = 8_000;

pub struct ReadTool {
    file_cache: Option<Arc<RwLock<FileStateCache>>>,
}

impl ReadTool {
    /// Create a ReadTool with optional file state cache for dedup.
    ///
    /// Pass `None` to disable caching (all reads return full content).
    pub fn new(file_cache: Option<Arc<RwLock<FileStateCache>>>) -> Self {
        Self { file_cache }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Reads a file from the local filesystem. Returns content with line numbers.\n\n\
         Usage:\n\
         - The file_path parameter must be an absolute path, not a relative path.\n\
         - Results are bounded. Follow [READ_TRUNCATED] next_offset (zero-based lines), or next_byte_offset for a long line. Use byte_offset/byte_limit for exact UTF-8 chunks including line endings.\n\
         - Results are returned with line numbers (1-based) followed by a tab and the line content.\n\
         - Binary files return \"(binary file, N bytes)\" instead of content.\n\
         - This tool can only read files, not directories. To list a directory, use ExecCommand with ls."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Line number to start reading from (0-based)"
                },
                "byte_offset": {"type":"integer","minimum":0,"description":"Zero-based UTF-8 byte offset; returns raw text and preserves line endings. Do not combine with offset/limit."},
                "byte_limit": {"type":"integer","minimum":4,"description":"Maximum raw UTF-8 bytes (up to 8000), used with byte_offset"},
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["file_path"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(file_path) = input["file_path"].as_str() else {
            return ToolResult {
                content: "Missing required parameter: file_path".to_string(),
                is_error: true,
            };
        };

        let offset = input["offset"].as_u64().map(|v| v as usize);
        let limit = input["limit"].as_u64().map(|v| v as usize);

        let byte_offset = input["byte_offset"].as_u64();
        if (byte_offset.is_some() && (offset.is_some() || limit.is_some()))
            || (input.get("byte_limit").is_some() && byte_offset.is_none())
        {
            return ToolResult {
                content: "Use either offset/limit or byte_offset/byte_limit.".into(),
                is_error: true,
            };
        }

        // Get file mtime for dedup and cache.
        let mtime_ms = file_mtime_ms(Path::new(file_path));

        // Dedup check: if cache has the same file with matching offset/limit and mtime,
        // return a short stub instead of full content.
        if byte_offset.is_none()
            && let (Some(cache_arc), Some(current_mtime)) = (&self.file_cache, mtime_ms)
            && let Ok(mut cache) = cache_arc.write()
            && let Some(cached) = cache.get(Path::new(file_path))
            && cached.offset == offset
            && cached.limit == limit
            && cached.mtime_ms == current_mtime
        {
            return ToolResult {
                content: FILE_UNCHANGED_STUB.to_string(),
                is_error: false,
            };
        }

        // Read file from disk.
        let content = match std::fs::read(file_path) {
            Ok(bytes) => bytes,
            Err(e) => {
                return ToolResult {
                    content: format!("Failed to read file {}: {}", file_path, e),
                    is_error: true,
                };
            }
        };

        // Check if binary.
        if content.iter().take(8192).any(|&b| b == 0) {
            return ToolResult {
                content: format!("(binary file, {} bytes)", content.len()),
                is_error: false,
            };
        }

        if byte_offset.is_some() && std::str::from_utf8(&content).is_err() {
            return ToolResult {
                content: "Byte range reading requires a valid UTF-8 text file.".into(),
                is_error: true,
            };
        }
        let text = String::from_utf8_lossy(&content);
        if let Some(byte_offset) = byte_offset {
            let offset = usize::try_from(byte_offset).unwrap_or(usize::MAX);
            let limit = input["byte_limit"].as_u64().unwrap_or(READ_CHUNK_MAX_BYTES as u64);
            return read_byte_chunk(&text, offset, usize::try_from(limit).unwrap_or(usize::MAX));
        }
        let lines: Vec<&str> = text.lines().collect();

        let effective_offset = offset.unwrap_or(0).min(lines.len());
        let effective_limit = limit.unwrap_or(lines.len());

        let end = effective_offset.saturating_add(effective_limit).min(lines.len());
        let slice = &lines[effective_offset.min(lines.len())..end];

        let mut numbered = Vec::new();
        let mut output_bytes = 0usize;
        let mut next_offset = None;
        for (i, line) in slice.iter().enumerate() {
            let entry = format!("{:>6}\t{}", effective_offset + i + 1, line);
            let entry_bytes = entry.len() + if numbered.is_empty() { 0 } else { 1 };
            if output_bytes + entry_bytes > READ_CHUNK_MAX_BYTES {
                if numbered.is_empty() {
                    let byte_offset = text.split_inclusive('\n').take(effective_offset).map(str::len).sum();
                    return read_byte_chunk(&text, byte_offset, READ_CHUNK_MAX_BYTES);
                }
                next_offset = Some(effective_offset + i);
                break;
            }
            output_bytes += entry_bytes;
            numbered.push(entry);
        }

        if next_offset.is_none() && end < lines.len() {
            next_offset = Some(end);
        }
        let mut result_content = numbered.join("\n");
        if let Some(next_offset) = next_offset {
            result_content.push_str(&format!(
                "\n\n[READ_TRUNCATED] next_offset={} total_lines={} read_more_with_offset=true",
                next_offset,
                lines.len()
            ));
        }

        // Update cache after successful read.
        if let Some(cache_arc) = &self.file_cache
            && let (Ok(mut cache), Some(mtime)) = (cache_arc.write(), mtime_ms)
        {
            cache.insert(
                file_path.into(),
                FileState {
                    content: result_content.clone(),
                    mtime_ms: mtime,
                    offset,
                    limit,
                },
            );
        }

        ToolResult {
            content: result_content,
            is_error: false,
        }
    }

    fn max_result_size(&self) -> usize {
        100_000
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }

    fn describe(&self, input: &Value) -> String {
        let path = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("unknown");
        format!("Read {}", path)
    }
}

fn read_byte_chunk(text: &str, offset: usize, limit: usize) -> ToolResult {
    if offset > text.len() || !text.is_char_boundary(offset) || limit < 4 {
        return ToolResult {
            content: "Invalid byte range: use a UTF-8 boundary and byte_limit >= 4.".into(),
            is_error: true,
        };
    }
    let body = truncate_utf8(&text[offset..], limit.min(READ_CHUNK_MAX_BYTES));
    let next = offset + body.len();
    let mut content = body.to_owned();
    if next < text.len() {
        content.push_str(&format!("\n\n[READ_TRUNCATED] next_byte_offset={next} total_bytes={} content_bytes={} read_more_with_byte_offset=true", text.len(), body.len()));
    }
    ToolResult {
        content,
        is_error: false,
    }
}

#[cfg(test)]
#[path = "read_test.rs"]
mod read_test;
