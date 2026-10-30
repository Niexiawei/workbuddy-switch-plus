//! Anthropic Messages 适配：请求转换 + SSE → Anthropic 事件流。
//!
//! 目标是让 Claude Code / cc-switch 能直接用本反代。文本与工具调用都支持；
//! 事件顺序与 Python 版一致（message_start → content_block_* → message_delta → message_stop），
//! 用 tests/fixtures/proxy/dataplane/anthropic_stream.json 做对齐。

use std::time::Duration;

use axum::body::Body;
use axum::http::header;
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn tool_choice_to_chat(choice: &Value) -> Option<Value> {
    let kind = choice.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => None,
    }
}

/// Anthropic Messages 请求 → OpenAI chat 请求（上游只吃 chat）。
pub fn request_to_chat(body: &Value) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = body.get("system") {
        let text = text_of(system);
        if !text.trim().is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    if let Some(items) = body.get("messages").and_then(Value::as_array) {
        for message in items {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            let content = message.get("content").cloned().unwrap_or(Value::Null);
            if role == "assistant" {
                let text = text_of(&content);
                let mut tool_calls = Vec::new();
                if let Value::Array(blocks) = &content {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                            continue;
                        }
                        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                        tool_calls.push(json!({
                            "id": block.get("id").and_then(Value::as_str).unwrap_or(""),
                            "type": "function",
                            "function": {
                                "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
                                "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into()),
                            }
                        }));
                    }
                }
                let mut out = json!({"role": "assistant", "content": text});
                if !tool_calls.is_empty() {
                    out["tool_calls"] = Value::Array(tool_calls);
                }
                messages.push(out);
                continue;
            }

            // user：文本块合成一条 user 消息；tool_result 各自变成 tool 消息。
            let mut text = String::new();
            let mut tool_results: Vec<Value> = Vec::new();
            match &content {
                Value::String(value) => text.push_str(value),
                Value::Array(blocks) => {
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(chunk) = block.get("text").and_then(Value::as_str) {
                                    text.push_str(chunk);
                                }
                            }
                            Some("tool_result") => {
                                let tool_use_id = block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                tool_results.push(json!({
                                    "role": "tool",
                                    "tool_call_id": tool_use_id,
                                    "content": text_of(block.get("content").unwrap_or(&Value::Null)),
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            if !text.is_empty() {
                messages.push(json!({"role": "user", "content": text}));
            }
            messages.extend(tool_results);
        }
    }

    let mut chat = json!({
        "model": body.get("model").cloned().unwrap_or_else(|| json!("")),
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    for (from, to) in [
        ("max_tokens", "max_tokens"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
    ] {
        if let Some(value) = body.get(from) {
            chat[to] = value.clone();
        }
    }
    if let Some(stop) = body.get("stop_sequences") {
        chat["stop"] = stop.clone();
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name").and_then(Value::as_str)?;
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": tool.get("description").and_then(Value::as_str).unwrap_or(""),
                        "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type": "object"})),
                    }
                }))
            })
            .collect();
        if !converted.is_empty() {
            chat["tools"] = Value::Array(converted);
        }
    }
    if let Some(choice) = body.get("tool_choice").and_then(tool_choice_to_chat) {
        chat["tool_choice"] = choice;
    }
    chat
}

fn event(name: &str, payload: Value) -> Vec<u8> {
    format!("event: {name}\ndata: {payload}\n\n").into_bytes()
}

fn stop_reason(finish: Option<&str>) -> &'static str {
    match finish {
        Some("length") => "max_tokens",
        Some("tool_calls") | Some("function_call") => "tool_use",
        _ => "end_turn",
    }
}

/// chat SSE → Anthropic 事件流。文本与 tool_use 都转；usage 在 message_delta 里回填。
pub struct AnthropicStream {
    model: String,
    started: bool,
    buffer: Vec<u8>,
    text_open: bool,
    next_index: u32,
    tool_blocks: Vec<(u32, String, String)>, // (index, id, name)
    finish: Option<String>,
    prompt_tokens: i64,
    completion_tokens: i64,
    error: Option<Value>,
}

impl AnthropicStream {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            started: false,
            buffer: Vec::new(),
            text_open: false,
            next_index: 0,
            tool_blocks: Vec::new(),
            finish: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            error: None,
        }
    }

    fn ensure_start(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_rust_proxy",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }
            }),
        )
    }

    fn open_text(&mut self) -> Vec<u8> {
        if self.text_open {
            return Vec::new();
        }
        self.text_open = true;
        let index = self.next_index;
        self.next_index += 1;
        let mut out = event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "text", "text": ""},
            }),
        );
        out.extend(self.index_of_text(index));
        out
    }

    fn index_of_text(&self, _index: u32) -> Vec<u8> {
        Vec::new()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = self.ensure_start();
        self.buffer.extend_from_slice(chunk);
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=position).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(error) = value.get("error") {
                self.error = Some(error.clone());
                continue;
            }
            if let Some(usage) = value.get("usage") {
                self.prompt_tokens = usage
                    .get("prompt_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(self.prompt_tokens);
                self.completion_tokens = usage
                    .get("completion_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(self.completion_tokens);
            }
            let Some(choice) = value
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|rows| rows.first())
            else {
                continue;
            };
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if !reason.is_empty() {
                    self.finish = Some(reason.to_string());
                }
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    out.extend(self.open_text());
                    out.extend(event(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta",
                            "index": 0,
                            "delta": {"type": "text_delta", "text": text},
                        }),
                    ));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (order, call) in calls.iter().enumerate() {
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = call
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if self
                        .tool_blocks
                        .iter()
                        .all(|(_, existing, _)| existing != &id || id.is_empty())
                    {
                        let index = self.next_index;
                        self.next_index += 1;
                        self.tool_blocks.push((index, id.clone(), name.clone()));
                        out.extend(event("content_block_start", json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
                        })));
                    }
                    let Some((index, _, _)) = self
                        .tool_blocks
                        .iter()
                        .find(|(_, existing, _)| existing == &id)
                        .cloned()
                    else {
                        let _ = order;
                        continue;
                    };
                    if let Some(args) = call
                        .get("function")
                        .and_then(|function| function.get("arguments"))
                        .and_then(Value::as_str)
                    {
                        if !args.is_empty() {
                            out.extend(event(
                                "content_block_delta",
                                json!({
                                    "type": "content_block_delta",
                                    "index": index,
                                    "delta": {"type": "input_json_delta", "partial_json": args},
                                }),
                            ));
                        }
                    }
                }
            }
        }
        out
    }

    /// 收尾：关闭所有块，发 message_delta + message_stop；没有终态帧时报错。
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = self.ensure_start();
        if let Some(error) = self.error.take() {
            out.extend(event("error", json!({"type": "error", "error": error})));
            return out;
        }
        if self.finish.is_none() {
            out.extend(event(
                "error",
                json!({
                    "type": "error",
                    "error": {"type": "incomplete_stream", "message": "上游流在终态帧之前结束"},
                }),
            ));
            return out;
        }
        if self.text_open {
            out.extend(event(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ));
        }
        for (index, _, _) in self.tool_blocks.clone() {
            out.extend(event(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            ));
        }
        out.extend(event("message_delta", json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason(self.finish.as_deref()), "stop_sequence": null},
            "usage": {"input_tokens": self.prompt_tokens, "output_tokens": self.completion_tokens},
        })));
        out.extend(event("message_stop", json!({"type": "message_stop"})));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_conversion_matches_golden() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/proxy/dataplane/anthropic_stream.json");
        let case: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let chat = request_to_chat(&case["request"]["body"]);
        let expected: Value =
            serde_json::from_str(case["upstream"]["body"].as_str().unwrap()).unwrap();
        assert_eq!(chat, expected);
    }

    #[test]
    fn stream_emits_anthropic_event_sequence() {
        let upstream = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pong\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n\
data: [DONE]\n\n";
        let mut stream = AnthropicStream::new("deepseek-v4-flash");
        let mut out = stream.feed(upstream.as_bytes());
        out.extend(stream.finish());
        let text = String::from_utf8_lossy(&out).to_string();
        for name in [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ] {
            assert!(text.contains(&format!("event: {name}")), "缺少事件 {name}");
        }
        assert!(text.contains("\"text\":\"pong\""));
        assert!(text.contains("\"stop_reason\":\"end_turn\""));
    }

    #[test]
    fn incomplete_upstream_reports_error_event() {
        let upstream = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half\"}}]}\n\n";
        let mut stream = AnthropicStream::new("m");
        let mut out = stream.feed(upstream.as_bytes());
        out.extend(stream.finish());
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("incomplete_stream"));
    }
}

/// 非流式：把 chat.completion 转成 Anthropic message 对象。
pub fn completion_to_message(completion: &Value, model: &str) -> Value {
    let choice = completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|rows| rows.first());
    let message = choice
        .and_then(|row| row.get("message"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let text = message.get("content").and_then(Value::as_str).unwrap_or("");
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let name = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let arguments = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                "name": name,
                "input": input,
            }));
        }
    }
    let finish = choice
        .and_then(|row| row.get("finish_reason"))
        .and_then(Value::as_str);
    let usage = completion
        .get("usage")
        .cloned()
        .unwrap_or_else(|| json!({}));
    json!({
        "id": "msg_rust_proxy",
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason(finish),
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(Value::as_i64).unwrap_or(0),
        }
    })
}

/// 把上游 chat SSE 转成 Anthropic 事件流返回给客户端。
pub fn stream_response(response: reqwest::Response, model: String) -> Response {
    const IDLE: Duration = Duration::from_secs(300);
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    tokio::spawn(async move {
        let mut stream = response.bytes_stream();
        let mut converter = AnthropicStream::new(&model);
        loop {
            match tokio::time::timeout(IDLE, stream.next()).await {
                Err(_) => {
                    let _ = tx
                        .send(Bytes::from(event(
                            "error",
                            json!({
                                "type": "error",
                                "error": {"type": "stream_timeout", "message": "上游长时间无响应"},
                            }),
                        )))
                        .await;
                    return;
                }
                Ok(None) => break,
                Ok(Some(Ok(chunk))) => {
                    let out = converter.feed(&chunk);
                    if !out.is_empty() && tx.send(Bytes::from(out)).await.is_err() {
                        return;
                    }
                }
                Ok(Some(Err(error))) => {
                    let _ = tx
                        .send(Bytes::from(event(
                            "error",
                            json!({
                                "type": "error",
                                "error": {"type": "transport", "message": error.to_string()},
                            }),
                        )))
                        .await;
                    return;
                }
            }
        }
        let tail = converter.finish();
        let _ = tx.send(Bytes::from(tail)).await;
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|item| (Ok::<_, std::io::Error>(item), rx))
    });
    Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}
