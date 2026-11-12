# Bundled Tools

The agent includes a core local tool suite and agent-level helpers. The LLM automatically selects and invokes them based on the task. MCP servers can add more tools at runtime.

| Tool | Function | Concurrent |
|------|----------|------------|
| **Read** | Read file contents (with line numbers) | Yes |
| **Write** | Write files (auto-creates directories) | No |
| **Edit** | Precise string replacement | No |
| **ExecCommand** | Execute shell commands | No |
| **Grep** | Regex search file contents (via ripgrep) | Yes |
| **Glob** | Find files by pattern matching | Yes |
| **ViewImage** | Load a local JPEG, PNG, GIF, or WebP image for model inspection | Yes |
| **AionOperation** | Call a whitelisted AionUi operation through the injected AionCore helper | No |
| **Spawn** | Spawn sub-agents for parallel tasks | No |
| **ToolSearch** | Load schemas for deferred tools | Yes |

---

## Read

Read file contents with line numbers, similar to `cat -n`.

- `offset` is a zero-based line index; `limit` caps the number of lines.
- Results use bounded chunks (about 8,000 bytes). `[READ_TRUNCATED]` gives `next_offset` and `total_lines`; pass that offset to continue.
- An oversized single line switches to raw UTF-8 bytes and reports `next_byte_offset`. Continue with `byte_offset`, optionally `byte_limit` (4–8,000 bytes); do not mix byte and line parameters.
- Byte mode preserves CRLF, blank lines, and final newlines for exact UTF-8 reconstruction. Invalid UTF-8 files or byte boundaries return errors. Read output skips whitespace/TOON transformations and still obeys the global tool-result limit.
- Binary files are detected automatically.
- Line mode formats each line as a one-based line number, a tab, and the line text.

## Write

Write content to a file atomically.

- Atomic write: writes to a temp file first, then renames
- Auto-creates parent directories

## Edit

Find and replace exact strings in a file.

- Matches `old_string` exactly and replaces with `new_string`
- Requires a unique match by default; errors on multiple matches
- Use `replace_all` to replace all occurrences

## ExecCommand

Execute a shell command and return the result.

- Default timeout: 120 seconds, max 600 seconds
- Returns exit code, stdout, and stderr

## AionOperation

Call fixed AionUi operations without constructing a PowerShell command.

- The `operation` field is a strict whitelist covering the configured AionUi, conversation, session, team, and skill operations.
- Operation-specific arguments go in the `input` object and are passed as JSON to the AionCore helper.
- State-changing operations require `confirm: true` and still pass through the normal tool approval flow.
- If the host did not inject `AIONUI_HELPER_BIN`, the tool is not registered. Use `ExecCommand` for ordinary system commands.
- Results use a normalized JSON envelope with `success`, `operation`, `data`, `error`, and `code` fields.
- Final model-visible output is valid JSON within the effective tool-output byte budget, including errors and approval denials. Text compaction and TOON do not rewrite this structured output. A budget below 512 bytes stops the batch before any helper runs.
- `delivery.complete` states whether the full original result was delivered. `has_more` and `next_offset` refer to the selected path, so `has_more: false` does not mean nested details were delivered. Lists show summaries; `$details` contains a JSON pointer to omitted content. MCP configuration and historical test status do not establish live availability in this session.
- To retrieve details, use `operation: "result.read"` with `input: {"result_id": "...", "path": "/data/0/tools", "offset": 0, "limit": 20}`. This is a local read-only operation, not a helper command. The pointer can address the original envelope, including `/meta/before`, `/meta/after` and `/error`; an empty pointer selects its root.
- Offsets count array items, object fields, or UTF-8 bytes for strings. Follow `next_offset`; concatenate string `data` fragments exactly. Text defaults to 4,096 bytes per page, arrays/objects to 20 entries; JSON escaping and envelope overhead can reduce the delivered page. The server reports the actual continuation position.
- Continuations read an immutable result and never run the original helper again. Each tool instance retains at most 8 results, 8 MiB per result and 32 MiB total serialized size, for 30 minutes. Runtime restart, eviction and expiry return `aion_operation_result_expired`. Snapshots are not persisted or shared between runtimes.
- Write receipts retain `delivery.execution_status: "succeeded"` after a successful helper result, even if details are summarized. Timeouts/readback failures can report `unknown`; they do not prove a write did not occur. Never repeat a write to retrieve its receipt: use `result.read`, or query resource state after expiry. An oversized result that cannot be retained explicitly reports `aion_operation_output_unavailable` while preserving execution status.

## Grep

Search file contents with regular expressions.

- Runs in-process with the Rust regex engine and recursive ignore-aware walker; it does not require `rg`, `grep`, or `findstr`
- Supports glob filtering and case-insensitive search
- Results limited to 250 lines

## Glob

Find files matching a glob pattern.

- Standard glob patterns (e.g., `**/*.rs`)
- Results sorted by modification time (newest first)
- Returns up to 100 files

## ViewImage

Load a supported local image and attach it to the next model turn.

- Accepts an absolute file path
- Supports JPEG, PNG, GIF, and WebP files up to 20 MB
- Validates that the file content matches its extension

## Spawn

See [Sub-Agent Spawning](advanced.md#sub-agent-spawning) in the Advanced Features guide.

## ToolSearch

Load full schemas for deferred tools so the LLM can invoke them. Deferred bundled or MCP tools are registered without their full parameter schemas until the LLM calls ToolSearch.

- Search by tool name or a keyword from its description
- Returns the full schemas of all matching deferred tools

Skills are exposed through the **Skill** tool. When plan mode is enabled, **EnterPlanMode** and **ExitPlanMode** are also registered. See [Skills](skills.md) and [Plan Mode](advanced.md#plan-mode) for details.

---

## How It Works

```
User input → Build request (system prompt + history + tool definitions)
           → Stream LLM API response
           → Output text to stdout in real-time
           → If LLM returns tool_use → confirm → execute → send result back
           → Loop until LLM stops calling tools
           → Output final reply → save session
```

- Concurrent-safe tools (Read, Grep, Glob, ViewImage) execute in parallel
- Non-concurrent tools (Write, Edit, ExecCommand) execute sequentially
- Tool output is auto-truncated to prevent context window overflow
- Tool output can be compacted (see [Output Compaction](advanced.md#output-compaction))

## Tool Descriptions

Each built-in tool includes a detailed description and usage guidance that is injected into the system prompt. These descriptions help the LLM select the right tool and use it effectively — for example, preferring Grep over ExecCommand for content search, or using Edit instead of Write for modifications.
