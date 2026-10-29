//! Responses 长上下文投影：把 Codex / Claude Code 的 agentic 请求压成最小语义闭包。
//!
//! 逐条对应 Python 版 core/responses_projection.py：
//!   · harness 模板（AGENTS.md / system-reminder / Codex 声明）整段丢弃；
//!   · 保留最新用户意图（anchor user）与最近一段真实 assistant / tool 链路；
//!   · 更早历史压成规则摘要；
//!   · tool schema 只留结构键，长参数与长工具输出压成可继续推理的摘要。
//!
//! 目标不是「省 token」，而是让上游审核与长上下文都稳定：保留可继续工作的信息，
//! 丢掉运行时脚手架。

use serde_json::{json, Map, Value};

const AGENTIC_TOOL_NAMES: &[&str] = &[
    "exec_command", "write_stdin", "update_plan", "request_user_input", "view_image",
    "get_goal", "create_goal", "update_goal", "apply_patch", "tool_search_tool",
];

const HARNESS_USER_MARKERS: &[&str] = &[
    "# AGENTS.md instructions", "<environment_context>", "<permissions instructions>",
    "<collaboration_mode>", "<skills_instructions>", "<system-reminder>", "# claudeMd",
];

const HARNESS_SYSTEM_MARKERS: &[&str] = &[
    "You are a coding agent running in the Codex CLI", "Within this context, Codex refers to",
    "# AGENTS.md spec", "<permissions instructions>", "<collaboration_mode>",
    "<skills_instructions>", "The following deferred tools are now available via ToolSearch.",
    "### Available skills", "## request_user_input availability", "You are Claude Code",
];

const BASE_SYSTEM_PROMPT: &str = "You are a coding assistant serving an OpenAI-compatible CLI. \
Be precise, concise, safe, and action-oriented. Use available tools when needed, follow repository \
instructions and durable user context, and continue from the preserved recent context. If earlier \
history was condensed, rely on the preserved recent messages and rerun tools when exact old details are required.";

const HISTORY_PREFIX: &str = "Earlier conversation summary (condensed):";
const MAX_SYSTEM_GUIDANCE_CHARS: usize = 1200;
const MAX_USER_CHARS: usize = 3200;
const MAX_ASSISTANT_CHARS: usize = 1800;
const MAX_TOOL_OUTPUT_CHARS: usize = 1600;
const MAX_TOOL_ARGS_CHARS: usize = 900;
const MAX_HISTORY_SUMMARY_CHARS: usize = 2200;
const MAX_HISTORY_ITEMS: usize = 10;
const MAX_TAIL_MESSAGES: usize = 8;
const MAX_TAIL_CHARS: usize = 7000;

const SCHEMA_KEEP_KEYS: &[&str] = &[
    "type", "properties", "required", "items", "enum", "oneOf", "anyOf", "allOf",
    "additionalProperties", "format", "minimum", "maximum", "minItems", "maxItems",
    "minLength", "maxLength", "nullable",
];

fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

fn truncate_text(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let all = chars(text);
    if all.len() <= limit {
        return text.to_string();
    }
    let keep = limit.saturating_sub(24);
    let head: String = all.iter().take(keep).collect();
    format!("{} ... [truncated {} chars]", head.trim_end(), all.len() - keep)
}

fn content_to_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(blocks) => {
            let mut out = String::new();
            for block in blocks {
                match block {
                    Value::String(text) => out.push_str(text),
                    Value::Object(map) => {
                        if let Some(text) = map.get("text").and_then(Value::as_str) {
                            out.push_str(text);
                        } else if let Some(output) = map.get("output").and_then(Value::as_str) {
                            out.push_str(output);
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        other => other.to_string(),
    }
}

fn looks_like_harness_user(text: &str) -> bool {
    HARNESS_USER_MARKERS.iter().any(|marker| text.contains(marker))
}

fn looks_like_harness_system(text: &str) -> bool {
    HARNESS_SYSTEM_MARKERS.iter().any(|marker| text.contains(marker))
}

fn message_cost(message: &Value) -> usize {
    let mut cost = content_to_text(message.get("content").unwrap_or(&Value::Null)).chars().count();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            if let Some(function) = call.get("function") {
                cost += function.get("name").and_then(Value::as_str).unwrap_or("").chars().count();
                cost += function.get("arguments").and_then(Value::as_str).unwrap_or("").chars().count();
            }
        }
    }
    cost
}

fn messages_size(messages: &[Value]) -> usize {
    messages.iter().map(|message| message_cost(message) + message.get("role").and_then(Value::as_str).unwrap_or("").chars().count()).sum()
}

fn tool_name(tool: &Value) -> String {
    tool.get("function")
        .and_then(|function| function.get("name"))
        .or_else(|| tool.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn tools_size(tools: &[Value]) -> usize {
    serde_json::to_string(tools).map(|text| text.chars().count()).unwrap_or(0)
}

fn summarize_free_text(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let all = chars(text);
    if all.len() <= limit {
        return text.to_string();
    }
    let head: String = all.iter().take(limit / 2).collect();
    let tail: String = all.iter().skip(all.len() - limit / 3).collect();
    let omitted = all.len() - head.chars().count() - tail.chars().count();
    format!("{}\n... [{} chars omitted] ...\n{}", head.trim_end(), omitted, tail.trim_start())
}

fn summarize_tool_output(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    if text.chars().count() <= MAX_TOOL_OUTPUT_CHARS && text.matches('\n').count() <= 24 {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let exit_line = lines.iter().find(|line| line.contains("Process exited with code")).map(|line| line.trim().to_string());
    let useful: Vec<&str> = lines
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| {
            !line.starts_with("Chunk ID:")
                && !line.starts_with("Wall time:")
                && !line.starts_with("Original token count:")
                && !line.starts_with("Process exited with code")
                && *line != "Output:"
        })
        .collect();

    let head = &useful[..useful.len().min(10)];
    let tail: &[&str] = if useful.len() > 16 { &useful[useful.len() - 6..] } else { &[] };
    let omitted = useful.len().saturating_sub(head.len() + tail.len());

    let mut parts: Vec<String> = Vec::new();
    if let Some(line) = exit_line {
        parts.push(line);
    }
    if !head.is_empty() {
        parts.push("Key output:".to_string());
        parts.extend(head.iter().map(|line| (*line).to_string()));
    }
    if omitted > 0 {
        parts.push(format!("... [omitted {omitted} lines] ..."));
    }
    if !tail.is_empty() {
        parts.push("Recent tail:".to_string());
        parts.extend(tail.iter().map(|line| (*line).to_string()));
    }
    let summary = parts.join("\n").trim().to_string();
    truncate_text(if summary.is_empty() { text } else { &summary }, MAX_TOOL_OUTPUT_CHARS)
}

fn tool_output_inline_summary(text: &str) -> String {
    truncate_text(&summarize_tool_output(text).replace('\n', " | "), 220)
}

fn shrink_json_value(value: &Value, depth: usize, key: &str) -> Value {
    if depth >= 4 {
        return json!("<omitted>");
    }
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (index, (name, item)) in map.iter().enumerate() {
                if index >= 12 {
                    out.insert("_omitted_keys".to_string(), json!(map.len() - index));
                    break;
                }
                out.insert(name.clone(), shrink_json_value(item, depth + 1, name));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let mut out: Vec<Value> = items.iter().take(6).map(|item| shrink_json_value(item, depth + 1, key)).collect();
            if items.len() > 6 {
                out.push(json!(format!("<omitted {} items>", items.len() - 6)));
            }
            Value::Array(out)
        }
        Value::String(text) => {
            let limit = if matches!(key, "cmd" | "chars" | "patch" | "content" | "text" | "question") { 240 } else { 120 };
            json!(truncate_text(text, limit))
        }
        other => other.clone(),
    }
}

fn summarize_tool_arguments(name: &str, arguments: &Value) -> String {
    let Some(text) = arguments.as_str() else {
        return serde_json::to_string(&shrink_json_value(arguments, 0, "")).unwrap_or_else(|_| "{}".to_string());
    };
    if text.chars().count() <= MAX_TOOL_ARGS_CHARS {
        return text.to_string();
    }
    if name == "apply_patch" {
        return json!({"summary": "Large apply_patch payload omitted; a patch was prepared or applied in a previous step."}).to_string();
    }
    match serde_json::from_str::<Value>(text) {
        Ok(parsed) => serde_json::to_string(&shrink_json_value(&parsed, 0, "")).unwrap_or_else(|_| "{}".to_string()),
        Err(_) => json!({"summary": truncate_text(text, 320)}).to_string(),
    }
}

fn project_tool_call(call: &Value) -> Option<Value> {
    let function = call.get("function")?;
    let name = function.get("name").and_then(Value::as_str).unwrap_or("");
    let arguments = function.get("arguments").cloned().unwrap_or_else(|| json!(""));
    Some(json!({
        "id": call.get("id").cloned().unwrap_or(Value::Null),
        "type": call.get("type").and_then(Value::as_str).unwrap_or("function"),
        "function": {"name": name, "arguments": summarize_tool_arguments(name, &arguments)},
    }))
}

fn project_schema(schema: &Value, depth: usize) -> Value {
    if depth >= 6 {
        return json!({"type": "object"});
    }
    match schema {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                if !SCHEMA_KEEP_KEYS.contains(&key.as_str()) {
                    continue;
                }
                let projected = match key.as_str() {
                    "properties" => match value {
                        Value::Object(props) => Value::Object(
                            props.iter().map(|(name, schema)| (name.clone(), project_schema(schema, depth + 1))).collect(),
                        ),
                        other => other.clone(),
                    },
                    "items" | "additionalProperties" => project_schema(value, depth + 1),
                    "oneOf" | "anyOf" | "allOf" => match value {
                        Value::Array(items) => Value::Array(items.iter().take(6).map(|item| project_schema(item, depth + 1)).collect()),
                        other => other.clone(),
                    },
                    _ => value.clone(),
                };
                out.insert(key.clone(), projected);
            }
            if out.is_empty() {
                json!({"type": "object"})
            } else {
                Value::Object(out)
            }
        }
        Value::Array(items) => Value::Array(items.iter().take(6).map(|item| project_schema(item, depth + 1)).collect()),
        other => other.clone(),
    }
}

fn project_tools(tools: &[Value]) -> (Vec<Value>, Value) {
    let mut projected = Vec::new();
    for tool in tools {
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            continue;
        }
        let function = tool.get("function").unwrap_or(tool);
        let Some(name) = function.get("name").and_then(Value::as_str) else { continue };
        let mut out = Map::new();
        out.insert("name".to_string(), json!(name));
        if let Some(parameters) = function.get("parameters") {
            out.insert("parameters".to_string(), project_schema(parameters, 0));
        }
        if let Some(strict) = function.get("strict") {
            out.insert("strict".to_string(), strict.clone());
        }
        projected.push(json!({"type": "function", "function": Value::Object(out)}));
    }
    let stats = json!({
        "original_tools": tools.len(),
        "projected_tools": projected.len(),
        "original_tool_chars": tools_size(tools),
        "projected_tool_chars": tools_size(&projected),
    });
    (projected, stats)
}

fn project_conversation_message(message: &Value, conservative: bool) -> Option<Value> {
    let role = message.get("role").and_then(Value::as_str)?;
    let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
    let mut out = message.clone();
    match role {
        "system" => {
            out["content"] = json!(truncate_text(&text, MAX_SYSTEM_GUIDANCE_CHARS));
            Some(out)
        }
        "user" => {
            out["content"] = json!(truncate_text(&text, MAX_USER_CHARS));
            Some(out)
        }
        "assistant" => {
            out["content"] = json!(summarize_free_text(&text, MAX_ASSISTANT_CHARS));
            let calls: Vec<Value> = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| calls.iter().filter_map(project_tool_call).collect())
                .unwrap_or_default();
            if !calls.is_empty() {
                out["tool_calls"] = Value::Array(calls);
            } else if let Some(object) = out.as_object_mut() {
                object.remove("tool_calls");
            }
            Some(out)
        }
        "tool" => {
            out["content"] = json!(summarize_tool_output(&text));
            Some(out)
        }
        _ if conservative => {
            out["content"] = json!(truncate_text(&text, MAX_ASSISTANT_CHARS));
            Some(out)
        }
        _ => None,
    }
}

fn project_messages_conservative(messages: &[Value]) -> Vec<Value> {
    messages.iter().filter_map(|message| project_conversation_message(message, true)).collect()
}

fn choose_tail_start(messages: &[Value]) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let mut start = messages.len().saturating_sub(1);
    let mut total = 0;
    let mut kept = 0;
    for index in (0..messages.len()).rev() {
        let cost = message_cost(&messages[index]);
        if kept > 0 && (kept >= MAX_TAIL_MESSAGES || total + cost > MAX_TAIL_CHARS) {
            break;
        }
        start = index;
        total += cost;
        kept += 1;
    }
    start
}

fn expand_tail_for_tool_context(messages: &[Value], start: usize) -> usize {
    if start == 0 {
        return start;
    }
    let mut needed: Vec<String> = messages[start..]
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|message| message.get("tool_call_id").and_then(Value::as_str).map(str::to_string))
        .collect();
    if needed.is_empty() {
        return start;
    }
    let mut expanded = start;
    for index in (0..start).rev() {
        let message = &messages[index];
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let ids: Vec<String> = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| calls.iter().filter_map(|call| call.get("id").and_then(Value::as_str).map(str::to_string)).collect())
            .unwrap_or_default();
        if ids.iter().any(|id| needed.contains(id)) {
            expanded = index;
            needed.retain(|id| !ids.contains(id));
            if needed.is_empty() {
                break;
            }
        }
    }
    expanded
}

fn latest_user_index(messages: &[Value]) -> Option<usize> {
    messages.iter().rposition(|message| message.get("role").and_then(Value::as_str) == Some("user"))
}

fn tool_call_name_map(messages: &[Value]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let id = call.get("id").and_then(Value::as_str);
                let name = call.get("function").and_then(|function| function.get("name")).and_then(Value::as_str);
                if let (Some(id), Some(name)) = (id, name) {
                    map.insert(id.to_string(), name.to_string());
                }
            }
        }
    }
    map
}

fn history_line(message: &Value, names: &std::collections::HashMap<String, String>) -> String {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
    match role {
        "user" => format!("User asked: {}", truncate_text(&text, 220)),
        "assistant" => {
            let tools: Vec<String> = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| calls.iter().filter_map(|call| call.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).map(str::to_string)).collect())
                .unwrap_or_default();
            if !text.is_empty() && !tools.is_empty() {
                format!("Assistant replied: {} Then called tools: {}.", truncate_text(&text, 160), tools.iter().take(4).cloned().collect::<Vec<_>>().join(", "))
            } else if !tools.is_empty() {
                format!("Assistant called tools: {}.", tools.iter().take(4).cloned().collect::<Vec<_>>().join(", "))
            } else if !text.is_empty() {
                format!("Assistant replied: {}", truncate_text(&text, 180))
            } else {
                String::new()
            }
        }
        "tool" => {
            let name = message.get("tool_call_id").and_then(Value::as_str).and_then(|id| names.get(id)).cloned().unwrap_or_else(|| "tool".to_string());
            format!("Tool {} returned: {}", name, tool_output_inline_summary(&text))
        }
        "system" => format!("System guidance: {}", truncate_text(&text, 180)),
        _ => String::new(),
    }
}

fn build_history_summary(messages: &[Value], names: &std::collections::HashMap<String, String>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut total = 0;
    let mut summarized = 0;
    for message in messages {
        let line = history_line(message, names);
        if line.is_empty() {
            continue;
        }
        if summarized >= MAX_HISTORY_ITEMS || total + line.chars().count() > MAX_HISTORY_SUMMARY_CHARS {
            break;
        }
        lines.push(format!("- {line}"));
        total += line.chars().count();
        summarized += 1;
    }
    let remaining = messages.len().saturating_sub(summarized);
    if remaining > 0 {
        lines.push(format!("- {remaining} earlier messages or tool results were further condensed."));
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("{HISTORY_PREFIX}\n{}", lines.join("\n"))
}

fn merge_guidance(messages: &[String]) -> String {
    let mut merged: Vec<String> = Vec::new();
    let mut total = 0;
    for message in messages.iter().take(2) {
        let text = message.trim();
        if text.is_empty() {
            continue;
        }
        let mut text = text.to_string();
        if total + text.chars().count() > MAX_SYSTEM_GUIDANCE_CHARS {
            text = truncate_text(&text, MAX_SYSTEM_GUIDANCE_CHARS.saturating_sub(total));
        }
        total += text.chars().count();
        merged.push(text);
        if total >= MAX_SYSTEM_GUIDANCE_CHARS {
            break;
        }
    }
    match merged.len() {
        0 => String::new(),
        1 => merged[0].clone(),
        _ => format!("Additional instructions:\n{}", merged.join("\n\n")),
    }
}

fn looks_like_agentic_cli(messages: &[Value], tools: &[Value]) -> bool {
    if tools.iter().any(|tool| AGENTIC_TOOL_NAMES.contains(&tool_name(tool).as_str())) {
        return true;
    }
    messages.iter().any(|message| {
        let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
        looks_like_harness_user(&text) || looks_like_harness_system(&text)
    })
}

/// Python 版的默认行为是「不投影」，只有 CODEBUDDY_LOSSY_PROJECTION=1 才启用。
/// 这里沿用同一开关语义，避免灰度期两边行为不一致。
pub fn preserve_by_default() -> bool {
    std::env::var("WB_PROXY_LOSSY_PROJECTION").map(|value| value != "1").unwrap_or(true)
}

/// 投影入口。返回 (新 body, 统计)。\`preserve\` 用于排障：完全不动。
pub fn project(body: &Value, preserve: bool) -> (Value, Value) {
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let tools = body.get("tools").and_then(Value::as_array).cloned().unwrap_or_default();
    if preserve {
        let stats = json!({
            "mode": "preserve", "aggressive": false,
            "original_messages": messages.len(), "projected_messages": messages.len(),
            "original_message_chars": messages_size(&messages),
            "projected_message_chars": messages_size(&messages),
            "original_tools": tools.len(), "projected_tools": tools.len(),
            "original_tool_chars": tools_size(&tools), "projected_tool_chars": tools_size(&tools),
        });
        return (body.clone(), stats);
    }

    let mut projected = body.clone();
    let (projected_tools, tool_stats) = project_tools(&tools);
    projected["tools"] = Value::Array(projected_tools);

    if !looks_like_agentic_cli(&messages, &tools) {
        let conservative = project_messages_conservative(&messages);
        projected["messages"] = Value::Array(conservative.clone());
        let mut stats = json!({
            "mode": "conservative", "aggressive": false,
            "original_messages": messages.len(), "projected_messages": conservative.len(),
            "original_message_chars": messages_size(&messages),
            "projected_message_chars": messages_size(&conservative),
        });
        if let (Some(target), Some(source)) = (stats.as_object_mut(), tool_stats.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        return (projected, stats);
    }

    let names = tool_call_name_map(&messages);
    let mut guidance: Vec<String> = Vec::new();
    let mut conversation: Vec<Value> = Vec::new();
    let mut dropped = 0usize;
    for message in &messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
        if role == "system" {
            if looks_like_harness_system(&text) {
                dropped += 1;
                continue;
            }
            let kept = truncate_text(&text, MAX_SYSTEM_GUIDANCE_CHARS);
            if !kept.is_empty() {
                guidance.push(kept);
            }
            continue;
        }
        if role == "user" && looks_like_harness_user(&text) {
            dropped += 1;
            continue;
        }
        if let Some(item) = project_conversation_message(message, false) {
            conversation.push(item);
        }
    }
    if conversation.is_empty() {
        conversation = project_messages_conservative(&messages);
    }

    let mut tail_start = choose_tail_start(&conversation);
    tail_start = expand_tail_for_tool_context(&conversation, tail_start);
    let latest_user = latest_user_index(&conversation);
    let anchor = latest_user.filter(|index| *index < tail_start).map(|index| conversation[index].clone());

    let omitted: Vec<Value> = conversation
        .iter()
        .enumerate()
        .take(tail_start)
        .filter(|(index, _)| Some(*index) != latest_user)
        .map(|(_, message)| message.clone())
        .collect();

    let mut final_messages: Vec<Value> = vec![json!({"role": "system", "content": BASE_SYSTEM_PROMPT})];
    let merged = merge_guidance(&guidance);
    if !merged.is_empty() {
        final_messages.push(json!({"role": "system", "content": merged}));
    }
    let summary = build_history_summary(&omitted, &names);
    if !summary.is_empty() {
        final_messages.push(json!({"role": "system", "content": summary}));
    }
    if let Some(anchor) = &anchor {
        final_messages.push(anchor.clone());
    }
    final_messages.extend(conversation[tail_start..].iter().cloned());
    projected["messages"] = Value::Array(final_messages.clone());

    let mut stats = json!({
        "mode": "aggressive", "aggressive": true,
        "dropped_harness_messages": dropped,
        "preserved_guidance_messages": guidance.len(),
        "summarized_history_messages": omitted.len(),
        "anchor_user_preserved": anchor.is_some(),
        "tail_messages": conversation.len() - tail_start,
        "original_messages": messages.len(),
        "projected_messages": final_messages.len(),
        "original_message_chars": messages_size(&messages),
        "projected_message_chars": messages_size(&final_messages),
    });
    if let (Some(target), Some(source)) = (stats.as_object_mut(), tool_stats.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    (projected, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agentic_body() -> Value {
        let mut messages = vec![
            json!({"role": "system", "content": "You are a coding agent running in the Codex CLI."}),
            json!({"role": "user", "content": "# AGENTS.md instructions\nrules"}),
            json!({"role": "system", "content": "Repo guidance: use pnpm."}),
            json!({"role": "user", "content": "Fix the failing test."}),
        ];
        for index in 0..12 {
            messages.push(json!({"role": "assistant", "content": format!("step {index}")}));
            messages.push(json!({"role": "user", "content": format!("follow {index}")}));
        }
        messages.push(json!({"role": "assistant", "content": "", "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": {"name": "exec_command", "arguments": "{\"cmd\":\"cargo test\"}"}
        }]}));
        messages.push(json!({"role": "tool", "tool_call_id": "call_1", "content": "test failed at line 42"}));
        messages.push(json!({"role": "user", "content": "Latest question: why?"}));
        // 之后再堆 10 条 assistant 消息，把最新用户意图挤出 tail 窗口，
        // 这样才走 anchor_user 分支（否则它天然就在 tail 里）。
        for index in 0..10 {
            messages.push(json!({"role": "assistant", "content": format!("post {index}")}));
        }
        json!({"model": "m", "messages": messages})
    }

    #[test]
    fn preserve_is_a_noop() {
        let body = agentic_body();
        let (projected, stats) = project(&body, true);
        assert_eq!(projected, body);
        assert_eq!(stats["mode"], json!("preserve"));
    }

    #[test]
    fn conservative_keeps_message_count() {
        let body = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"}
        ]});
        let (projected, stats) = project(&body, false);
        assert_eq!(stats["mode"], json!("conservative"));
        assert_eq!(projected["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn aggressive_drops_harness_and_anchors_latest_user() {
        let body = agentic_body();
        let (projected, stats) = project(&body, false);
        assert_eq!(stats["aggressive"], json!(true));
        assert!(stats["dropped_harness_messages"].as_u64().unwrap() >= 2);
        assert_eq!(stats["anchor_user_preserved"], json!(true));
        assert!(stats["tail_messages"].as_u64().unwrap() <= MAX_TAIL_MESSAGES as u64);

        let messages = projected["messages"].as_array().unwrap();
        assert_eq!(messages[0]["content"], json!(BASE_SYSTEM_PROMPT));
        let texts: Vec<&str> = messages.iter().filter_map(|m| m["content"].as_str()).collect();
        assert!(texts.contains(&"Latest question: why?"), "必须保留最新用户意图");
        assert!(texts.iter().any(|text| text.starts_with(HISTORY_PREFIX)), "更早历史应压成摘要");
    }

    #[test]
    fn tool_schema_keeps_only_structural_keys() {
        let body = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {
                "name": "exec_command",
                "description": "run",
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string", "description": "cmd"}}, "x-private": 1}
            }}]
        });
        let (projected, _) = project(&body, false);
        let schema = &projected["tools"][0]["function"]["parameters"];
        assert!(schema.get("x-private").is_none());
        assert_eq!(schema["properties"]["cmd"]["type"], json!("string"));
        assert!(schema["properties"]["cmd"].get("description").is_none());
    }
}
