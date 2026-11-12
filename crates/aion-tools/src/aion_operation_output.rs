//! Bounded, tool-instance-local snapshots for read-only continuation of results.
//! A continuation never calls the helper, including after a successful write.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aion_types::tool::ToolResult;
use serde_json::{Map, Value, json};

use crate::truncate_utf8;

pub(super) const MIN_OUTPUT_BYTES: usize = 512;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STORE_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESULTS: usize = 8;
const SNAPSHOT_TTL: Duration = Duration::from_secs(30 * 60);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct Snapshot {
    id: String,
    value: Value,
    bytes: usize,
    created: Instant,
}

#[derive(Default)]
struct Store {
    entries: VecDeque<Arc<Snapshot>>,
    bytes: usize,
}

impl Store {
    fn prune(&mut self) {
        while self
            .entries
            .front()
            .is_some_and(|x| x.created.elapsed() >= SNAPSHOT_TTL)
        {
            self.pop();
        }
    }

    fn pop(&mut self) {
        if let Some(old) = self.entries.pop_front() {
            self.bytes -= old.bytes;
        }
    }

    fn insert(&mut self, value: Value, bytes: usize) -> Option<Arc<Snapshot>> {
        if bytes > MAX_SNAPSHOT_BYTES {
            return None;
        }
        self.prune();
        while self.entries.len() >= MAX_RESULTS || self.bytes + bytes > MAX_STORE_BYTES {
            self.pop();
        }
        // Identity only, not an authorization token. The containing tool instance
        // supplies isolation; time + process + counter also avoid restart reuse.
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!(
            "{time:x}-{:x}-{:x}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let snapshot = Arc::new(Snapshot {
            id,
            value,
            bytes,
            created: Instant::now(),
        });
        self.bytes += bytes;
        self.entries.push_back(snapshot.clone());
        Some(snapshot)
    }

    fn get(&mut self, id: &str) -> Option<Arc<Snapshot>> {
        self.prune();
        self.entries.iter().find(|x| x.id == id).cloned()
    }
}

#[derive(Default)]
pub(super) struct OperationOutput {
    store: Mutex<Store>,
}

impl OperationOutput {
    pub(super) fn format(&self, input: &Value, result: &ToolResult, budget: usize) -> ToolResult {
        let mut value: Value = serde_json::from_str(&result.content).unwrap_or_else(|_| {
            json!({
                "success": false, "operation": input["operation"], "data": null,
                "code": "aion_operation_not_executed", "error": {"message":result.content},
            })
        });
        if !value.is_object() {
            return error("aion_operation_invalid_output", "Expected a JSON object");
        }
        if value.get("success").is_none() {
            value["success"] = json!(!result.is_error);
        }
        if value.get("operation").is_none() {
            value["operation"] = input["operation"].clone();
        }
        if value.get("data").is_none() {
            value["data"] = Value::Null;
        }
        // The orchestrator calls this again for synthetic errors and its final
        // pass. Pages already fitted to the current budget are byte-stable.
        if value.get("delivery").is_some() {
            if result.content.len() <= budget {
                return result.clone();
            }
            let delivery = &value["delivery"];
            return self.read(
                &json!({
                    "result_id":delivery["result_id"], "path":delivery["path"],
                    "offset":delivery["offset"], "limit":delivery["limit"],
                }),
                budget,
            );
        }
        let execution = execution_status(&value);
        value["delivery"] = json!({"complete":true,"has_more":false,"execution_status":execution});
        let full = encode(value.clone(), result.is_error);
        let operation = value["operation"].as_str().unwrap_or_default().to_owned();
        let prefer_summary = operation.ends_with(".list")
            || operation.ends_with(".capabilities")
            || operation == "config.mcp.servers.get"
            || operation == "config.skills.scan";
        if full.content.len() <= budget && (result.is_error || !prefer_summary) {
            return full;
        }
        value.as_object_mut().expect("object checked above").remove("delivery");
        let bytes = result.content.len();
        let stored = self
            .store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(value.clone(), bytes);
        let Some(snapshot) = stored else {
            return unavailable(&value, "result_exceeds_snapshot_limit");
        };
        let limit = if snapshot.value["data"].is_string() { 4096 } else { 20 };
        let page = if result.is_error {
            error_summary(&snapshot, budget)
        } else {
            page(&snapshot, "/data", 0, limit, budget)
        };
        tracing::debug!(
            operation = if super::OPERATION_PATHS.contains(&operation.as_str()) {
                operation.as_str()
            } else {
                "invalid"
            },
            original_bytes = bytes,
            output_bytes = page.content.len(),
            "Aion operation output adapted"
        );
        page
    }

    pub(super) fn read(&self, input: &Value, budget: usize) -> ToolResult {
        if input.as_object().is_none_or(|v| {
            v.keys()
                .any(|k| !["result_id", "path", "offset", "limit"].contains(&k.as_str()))
        }) {
            return error(
                "aion_operation_invalid_input",
                "result.read accepts result_id, path, offset and limit",
            );
        }
        let Some(id) = input["result_id"].as_str() else {
            return error("aion_operation_invalid_input", "result_id is required");
        };
        let path = match input.get("path") {
            None | Some(Value::Null) => "/data",
            Some(Value::String(s)) if s.len() <= 4096 => s,
            _ => {
                return error(
                    "aion_operation_invalid_input",
                    "path must be a JSON pointer of at most 4096 bytes",
                );
            }
        };
        let offset = match nonnegative(input.get("offset"), 0) {
            Some(n) => n,
            None => return error("aion_operation_invalid_input", "offset must be a nonnegative integer"),
        };
        let snapshot = self.store.lock().unwrap_or_else(|e| e.into_inner()).get(id);
        match snapshot {
            Some(snapshot) => {
                let text = snapshot.value.pointer(path).is_some_and(Value::is_string);
                let limit = match nonnegative(input.get("limit"), if text { 4096 } else { 20 }) {
                    Some(n) if n > 0 => n.min(if text { 65536 } else { 1000 }),
                    _ => return error("aion_operation_invalid_input", "limit must be a positive integer"),
                };
                page(&snapshot, path, offset, limit, budget)
            }
            None => error(
                "aion_operation_result_expired",
                "Result is missing, expired or belongs to another runtime. Do not repeat a write; query its resource state instead.",
            ),
        }
    }
}

fn nonnegative(value: Option<&Value>, default: usize) -> Option<usize> {
    match value {
        None | Some(Value::Null) => Some(default),
        Some(value) => usize::try_from(value.as_u64()?).ok(),
    }
}

fn execution_status(root: &Value) -> &'static str {
    if let Some(status) = root["execution"]["status"].as_str() {
        return match status {
            "not_executed" => "not_executed",
            "succeeded" => "succeeded",
            _ => "unknown",
        };
    }
    if root["success"] == true {
        "succeeded"
    } else if root["executed"] == false
        || matches!(
            root["code"].as_str(),
            Some(
                "aion_operation_confirmation_required"
                    | "aion_operation_invalid_input"
                    | "aion_operation_unknown"
                    | "aion_operation_not_executed"
            )
        )
    {
        "not_executed"
    } else {
        // A timeout or readback failure does not prove that no mutation occurred.
        "unknown"
    }
}

fn error_summary(snapshot: &Snapshot, budget: usize) -> ToolResult {
    let root = &snapshot.value;
    let primary = root
        .pointer("/errors/0")
        .or_else(|| root.pointer("/error/details/errors/0"));
    let message = root
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Operation failed; read the original error before retrying.");
    let mut value = json!({"success":false,"operation":root["operation"],"code":root["code"],
        "error":{"message":truncate_utf8(message,160)},
        "delivery":{"complete":false,"has_more":false,"execution_status":execution_status(root),"result_id":snapshot.id,"path":"","read_operation":"result.read","retry_write":false}});
    if let Some(primary) = primary {
        value["error"]["field_error"] = primary.clone();
    }
    if value.to_string().len() > budget {
        value["error"]["message"] = json!("Read the original error before retrying.");
        if let Some(primary) = primary {
            value["error"]["field_error"] =
                json!({"path":primary["path"],"expected":primary["expected"],"actual":primary["actual"]});
        }
    }
    if value.to_string().len() > budget {
        value["error"].as_object_mut().expect("object").remove("field_error");
    }
    if value.to_string().len() > budget {
        return unavailable(root, "output_budget_too_small_for_error");
    }
    encode(value, true)
}

fn encode(value: Value, is_error: bool) -> ToolResult {
    ToolResult {
        content: value.to_string(),
        is_error,
    }
}

fn error(code: &str, message: &str) -> ToolResult {
    encode(
        json!({"success":false,"operation":"result.read","code":code,"error":{"message":message},
        "delivery":{"complete":true,"has_more":false,"execution_status":"not_executed"}}),
        true,
    )
}

fn unavailable(root: &Value, reason: &str) -> ToolResult {
    encode(
        json!({"success":root["success"] == true,
        "code":root["code"].as_str().filter(|s| s.len()<=128).unwrap_or("aion_operation_output_unavailable"),
        "resource_id":root["data"]["id"].as_str().filter(|s|s.len()<=96),
        "delivery":{"complete":false,"has_more":false,"execution_status":execution_status(root),"reason":reason,
        "retry_write":false}}),
        root["success"] != true,
    )
}

fn child_path(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}

fn reference(value: &Value, path: &str) -> Value {
    let (kind, size) = match value {
        Value::String(s) => ("text", s.len()),
        Value::Array(a) => ("array", a.len()),
        Value::Object(o) => ("object", o.len()),
        _ => ("value", value.to_string().len()),
    };
    json!({"$details":path,"$type":kind,"$size":size})
}

fn preview(value: &Value, path: &str) -> Value {
    if let Value::Object(object) = value {
        if object.len() <= 12
            && value.to_string().len() <= 768
            && !object
                .keys()
                .any(|k| ["tools", "input_schema", "original_json", "body", "content"].contains(&k.as_str()))
        {
            return value.clone();
        }
        let mut out = Map::new();
        for key in [
            "id",
            "name",
            "enabled",
            "type",
            "status",
            "last_test_status",
            "builtin",
            "description",
            "message",
            "command",
            "subject",
        ] {
            if let Some(v) = object.get(key) {
                if v.is_string() {
                    let s = v.as_str().unwrap_or_default();
                    out.insert(key.into(), Value::String(truncate_utf8(s, 120).to_owned()));
                } else if v.is_boolean() || v.is_number() || v.is_null() {
                    out.insert(key.into(), v.clone());
                }
            }
        }
        if let Some(tools) = object.get("tools").and_then(Value::as_array) {
            out.insert("tool_count".into(), json!(tools.len()));
        }
        if let Some(transport) = object.get("transport").and_then(|v| v.get("type")) {
            out.insert("transport_type".into(), transport.clone());
        }
        out.insert("$details".into(), path.into());
        return Value::Object(out);
    }
    if value.to_string().len() <= 768 {
        value.clone()
    } else {
        reference(value, path)
    }
}

fn page(snapshot: &Snapshot, path: &str, offset: usize, limit: usize, budget: usize) -> ToolResult {
    let root = &snapshot.value;
    let Some(selected) = root.pointer(path) else {
        return error(
            "aion_operation_result_path_missing",
            "JSON pointer not found in this result",
        );
    };
    let (unit, total) = match selected {
        Value::String(s) => ("utf8_bytes", s.len()),
        Value::Array(a) => ("items", a.len()),
        Value::Object(o) => ("fields", o.len()),
        _ => ("value", 1),
    };
    if offset > total || selected.as_str().is_some_and(|s| !s.is_char_boundary(offset)) {
        return error(
            "aion_operation_invalid_offset",
            "offset is outside the result or inside a UTF-8 character",
        );
    }
    let response = |data: Value, end: usize, summarized: bool| {
        let mut value = json!({
            "success":root["success"] == true,"operation":root["operation"],"data":data,
            "code":root["code"].as_str().map(|s| truncate_utf8(s,128)),
            "error":if root["error"].is_null() { Value::Null } else { preview(&root["error"], "/error") },
            "delivery":{"result_id":snapshot.id,"path":path,"offset":offset,"limit":limit,"unit":unit,"total":total,
                "next_offset":if end < total { Some(end) } else { None },"has_more":end < total,
                "complete":path.is_empty() && offset == 0 && end == total && !summarized,"summarized":summarized,"execution_status":execution_status(root),
                "read_operation":"result.read"}
        });
        if root["success"] == true
            && !root["data"].is_null()
            && root["operation"].as_str().is_some_and(|s| s.starts_with("config.mcp."))
        {
            value["delivery"]["state_basis"] = "saved_configuration_or_historical_test".into();
        }
        encode(value, root["success"] != true)
    };
    if let Value::String(s) = selected {
        // Binary search counts JSON escapes and envelope overhead, not just text.
        let mut low = offset;
        let mut high = total.min(offset.saturating_add(limit));
        while high > offset && !s.is_char_boundary(high) {
            high -= 1;
        }
        while low < high {
            let mut mid = low + (high - low).div_ceil(2);
            while mid > low && !s.is_char_boundary(mid) {
                mid -= 1;
            }
            if mid == low {
                mid = s[low..].char_indices().nth(1).map_or(total, |(n, _)| low + n);
            }
            if response(json!(&s[offset..mid]), mid, false).content.len() <= budget {
                low = mid;
            } else {
                high = s[..mid].char_indices().next_back().map_or(low, |(n, _)| n);
            }
        }
        let result = response(json!(&s[offset..low]), low, false);
        if (low == offset && offset < total) || result.content.len() > budget {
            return unavailable(root, "output_budget_too_small_for_page");
        }
        return result;
    }
    let mut end = offset;
    let mut data = if selected.is_array() {
        json!([])
    } else if selected.is_object() {
        json!({})
    } else {
        selected.clone()
    };
    let mut summarized = false;
    let mut result = response(data.clone(), end, false);
    for index in offset..total.min(offset.saturating_add(limit)) {
        let mut candidate = data.clone();
        let is_summary = match selected {
            Value::Array(items) => {
                let item = preview(&items[index], &child_path(path, &index.to_string()));
                let changed = item != items[index];
                candidate.as_array_mut().expect("array constructed above").push(item);
                changed
            }
            Value::Object(fields) => {
                let Some((key, value)) = fields.iter().nth(index) else {
                    break;
                };
                let item = if value.to_string().len() <= 768 {
                    value.clone()
                } else {
                    reference(value, &child_path(path, key))
                };
                let changed = item != *value;
                candidate
                    .as_object_mut()
                    .expect("object constructed above")
                    .insert(key.clone(), item);
                changed
            }
            _ => false,
        };
        let mut is_summary = is_summary;
        let mut next = response(candidate.clone(), index + 1, summarized || is_summary);
        if next.content.len() > budget {
            // A single otherwise-small item may not fit a nondefault budget.
            // Return a pointer to that item rather than getting stuck on it.
            match selected {
                Value::Array(items) => {
                    let last = candidate
                        .as_array_mut()
                        .expect("array constructed above")
                        .last_mut()
                        .expect("item appended above");
                    *last = reference(&items[index], &child_path(path, &index.to_string()));
                }
                Value::Object(fields) => {
                    if let Some((key, value)) = fields.iter().nth(index) {
                        candidate
                            .as_object_mut()
                            .expect("object constructed above")
                            .insert(key.clone(), reference(value, &child_path(path, key)));
                    }
                }
                _ => {}
            }
            is_summary = true;
            next = response(candidate.clone(), index + 1, true);
            if next.content.len() > budget {
                break;
            }
        }
        data = candidate;
        end = index + 1;
        summarized |= is_summary;
        result = next;
    }
    if (end == offset && offset < total) || result.content.len() > budget {
        return unavailable(root, "output_budget_too_small_for_page");
    }
    result
}

#[cfg(test)]
#[path = "aion_operation_output_test.rs"]
mod tests;
