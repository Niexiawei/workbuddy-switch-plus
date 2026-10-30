//! OpenAI Responses 适配（Codex CLI）：请求转换 + chat SSE → Responses 事件流。
//!
//! 事件顺序对齐 tests/fixtures/proxy/dataplane/responses_stream.json：
//! response.created → in_progress → output_item.added → content_part.added →
//! output_text.delta → output_text.done → content_part.done → output_item.done →
//! response.completed。工具调用走 function_call 系列事件。
//!
//! 未做：responses_projection（Codex 长上下文投影）。当前把 input 原样转成 chat
//! 消息，超长会话的裁剪留到 projection 阶段。

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| block.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Responses 请求 → chat 请求。
pub fn request_to_chat(body: &Value) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(instructions) = body.get("instructions") {
        let text = text_of(instructions);
        if !text.trim().is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    match body.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                let kind = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                match kind {
                    "function_call_output" => {
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
                            "content": item.get("output").map(text_of).unwrap_or_default(),
                        }));
                    }
                    "function_call" => {
                        messages.push(json!({
                            "role": "assistant",
                            "content": "",
                            "tool_calls": [{
                                "id": item.get("call_id").or_else(|| item.get("id"))
                                    .and_then(Value::as_str).unwrap_or(""),
                                "type": "function",
                                "function": {
                                    "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
                                    "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
                                }
                            }]
                        }));
                    }
                    _ => {
                        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                        messages.push(json!({
                            "role": role,
                            "content": item.get("content").map(text_of).unwrap_or_default(),
                        }));
                    }
                }
            }
        }
        _ => {}
    }

    let mut chat = json!({
        "model": body.get("model").cloned().unwrap_or_else(|| json!("")),
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(value) = body.get("max_output_tokens") {
        chat["max_tokens"] = value.clone();
    }
    for key in ["temperature", "top_p", "parallel_tool_calls"] {
        if let Some(value) = body.get(key) {
            chat[key] = value.clone();
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        chat["tool_choice"] = choice.clone();
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
                        "parameters": tool.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"})),
                    }
                }))
            })
            .collect();
        if !converted.is_empty() {
            chat["tools"] = Value::Array(converted);
        }
    }
    chat
}

fn event(name: &str, payload: Value) -> Vec<u8> {
    format!("event: {name}\ndata: {payload}\n\n").into_bytes()
}

fn response_object(
    id: &str,
    model: &str,
    status: &str,
    created_at: i64,
    output: Vec<Value>,
    usage: Option<Value>,
) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "status": status,
        "error": null,
        "incomplete_details": null,
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "usage": usage,
    })
}

struct ToolItem {
    output_index: u32,
    id: String,
    name: String,
    arguments: String,
}

pub struct ResponsesStream {
    model: String,
    response_id: String,
    created_at: i64,
    sequence: u64,
    buffer: Vec<u8>,
    started: bool,
    text: String,
    text_open: bool,
    tools: Vec<ToolItem>,
    finish: Option<String>,
    prompt_tokens: i64,
    completion_tokens: i64,
    failed: bool,
}

impl ResponsesStream {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            response_id: "resp_rust_proxy".to_string(),
            created_at: 0,
            sequence: 0,
            buffer: Vec::new(),
            started: false,
            text: String::new(),
            text_open: false,
            tools: Vec::new(),
            finish: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            failed: false,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence - 1
    }

    fn start(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let created = response_object(
            &self.response_id,
            &self.model,
            "in_progress",
            self.created_at,
            vec![],
            None,
        );
        let mut out = event(
            "response.created",
            json!({
                "type": "response.created",
                "sequence_number": self.next_seq(),
                "response": created,
            }),
        );
        let in_progress = response_object(
            &self.response_id,
            &self.model,
            "in_progress",
            self.created_at,
            vec![],
            None,
        );
        out.extend(event(
            "response.in_progress",
            json!({
                "type": "response.in_progress",
                "sequence_number": self.next_seq(),
                "response": in_progress,
            }),
        ));
        out
    }

    fn ensure_text_item(&mut self) -> Vec<u8> {
        if self.text_open {
            return Vec::new();
        }
        self.text_open = true;
        let mut out = event(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "sequence_number": self.next_seq(),
                "output_index": 0,
                "item": {"type": "message", "id": "msg_rust_0", "status": "in_progress", "role": "assistant", "content": []},
            }),
        );
        out.extend(event(
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "sequence_number": self.next_seq(),
                "output_index": 0,
                "item_id": "msg_rust_0",
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
            }),
        ));
        out
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = self.start();
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
            if value.get("error").is_some() {
                self.failed = true;
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
                    out.extend(self.ensure_text_item());
                    self.text.push_str(text);
                    out.extend(event(
                        "response.output_text.delta",
                        json!({
                            "type": "response.output_text.delta",
                            "sequence_number": self.next_seq(),
                            "output_index": 0,
                            "item_id": "msg_rust_0",
                            "content_index": 0,
                            "delta": text,
                        }),
                    ));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let args = call
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !self
                        .tools
                        .iter()
                        .any(|tool| tool.id == id && !id.is_empty())
                    {
                        let output_index = self.tools.len() as u32 + 1;
                        self.tools.push(ToolItem {
                            output_index,
                            id: id.clone(),
                            name: name.clone(),
                            arguments: String::new(),
                        });
                        out.extend(event("response.output_item.added", json!({
                            "type": "response.output_item.added",
                            "sequence_number": self.next_seq(),
                            "output_index": output_index,
                            "item": {"type": "function_call", "id": format!("fc_{id}"), "status": "in_progress",
                                     "call_id": id, "name": name, "arguments": ""},
                        })));
                    }
                    if let Some(position) = self.tools.iter().position(|tool| tool.id == id) {
                        if !args.is_empty() {
                            let sequence = self.next_seq();
                            let output_index = self.tools[position].output_index;
                            self.tools[position].arguments.push_str(&args);
                            out.extend(event(
                                "response.function_call_arguments.delta",
                                json!({
                                    "type": "response.function_call_arguments.delta",
                                    "sequence_number": sequence,
                                    "output_index": output_index,
                                    "item_id": format!("fc_{id}"),
                                    "delta": args,
                                }),
                            ));
                        }
                    }
                }
            }
        }
        out
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = self.start();
        if self.failed || self.finish.is_none() {
            let sequence = self.next_seq();
            out.extend(event("response.failed", json!({
                "type": "response.failed",
                "sequence_number": sequence,
                "response": response_object(&self.response_id, &self.model, "failed", self.created_at, vec![], None),
            })));
            return out;
        }
        let mut output: Vec<Value> = Vec::new();
        if self.text_open {
            let text = self.text.clone();
            let seq_text = self.next_seq();
            out.extend(event(
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done",
                    "sequence_number": seq_text,
                    "output_index": 0,
                    "item_id": "msg_rust_0",
                    "content_index": 0,
                    "text": text,
                }),
            ));
            let seq_part = self.next_seq();
            out.extend(event(
                "response.content_part.done",
                json!({
                    "type": "response.content_part.done",
                    "sequence_number": seq_part,
                    "output_index": 0,
                    "item_id": "msg_rust_0",
                    "content_index": 0,
                    "part": {"type": "output_text", "text": text, "annotations": []},
                }),
            ));
            let item = json!({"type": "message", "id": "msg_rust_0", "status": "completed",
                              "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]});
            let seq_item = self.next_seq();
            out.extend(event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "sequence_number": seq_item,
                    "output_index": 0,
                    "item": item,
                }),
            ));
            output.push(item);
        }

        let tool_count = self.tools.len();
        for position in 0..tool_count {
            let (output_index, id, name, arguments) = {
                let tool = &self.tools[position];
                (
                    tool.output_index,
                    tool.id.clone(),
                    tool.name.clone(),
                    tool.arguments.clone(),
                )
            };
            let seq_args = self.next_seq();
            out.extend(event(
                "response.function_call_arguments.done",
                json!({
                    "type": "response.function_call_arguments.done",
                    "sequence_number": seq_args,
                    "output_index": output_index,
                    "item_id": format!("fc_{id}"),
                    "arguments": arguments,
                }),
            ));
            let item = json!({"type": "function_call", "id": format!("fc_{id}"), "status": "completed",
                              "call_id": id, "name": name, "arguments": arguments});
            let seq_item = self.next_seq();
            out.extend(event(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "sequence_number": seq_item,
                    "output_index": output_index,
                    "item": item,
                }),
            ));
            output.push(item);
        }

        let usage = json!({
            "input_tokens": self.prompt_tokens,
            "output_tokens": self.completion_tokens,
            "total_tokens": self.prompt_tokens + self.completion_tokens,
        });
        let seq_done = self.next_seq();
        out.extend(event("response.completed", json!({
            "type": "response.completed",
            "sequence_number": seq_done,
            "response": response_object(&self.response_id, &self.model, "completed", self.created_at, output, Some(usage)),
        })));
        out
    }
}

/// 非流式：chat.completion → Responses response 对象。
pub fn completion_to_response(completion: &Value, model: &str) -> Value {
    let choice = completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|rows| rows.first());
    let message = choice
        .and_then(|row| row.get("message"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let text = message.get("content").and_then(Value::as_str).unwrap_or("");
    let mut output = Vec::new();
    if !text.is_empty() {
        output.push(json!({"type": "message", "id": "msg_rust_0", "status": "completed", "role": "assistant",
                           "content": [{"type": "output_text", "text": text, "annotations": []}]}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            output.push(json!({
                "type": "function_call",
                "id": format!("fc_{}", call.get("id").and_then(Value::as_str).unwrap_or("")),
                "status": "completed",
                "call_id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                "name": call.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or(""),
                "arguments": call.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("{}"),
            }));
        }
    }
    let usage = completion
        .get("usage")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mapped = json!({
        "input_tokens": usage.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0),
        "output_tokens": usage.get("completion_tokens").and_then(Value::as_i64).unwrap_or(0),
        "total_tokens": usage.get("total_tokens").and_then(Value::as_i64).unwrap_or(0),
    });
    response_object(
        "resp_rust_proxy",
        model,
        "completed",
        0,
        output,
        Some(mapped),
    )
}

/// chat SSE → Responses 事件流。
pub fn stream_response(response: reqwest::Response, model: String) -> Response {
    const IDLE: Duration = Duration::from_secs(300);
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    tokio::spawn(async move {
        let mut stream = response.bytes_stream();
        let mut converter = ResponsesStream::new(&model);
        loop {
            match tokio::time::timeout(IDLE, stream.next()).await {
                Err(_) => {
                    let _ = tx
                        .send(Bytes::from(event(
                            "response.failed",
                            json!({
                                "type": "response.failed",
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
                            "response.failed",
                            json!({
                                "type": "response.failed",
                                "error": {"type": "transport", "message": error.to_string()},
                            }),
                        )))
                        .await;
                    return;
                }
            }
        }
        let _ = tx.send(Bytes::from(converter.finish())).await;
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|item| (Ok::<_, std::io::Error>(item), rx))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_conversion_matches_golden() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/proxy/dataplane/responses_stream.json");
        let case: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let chat = request_to_chat(&case["request"]["body"]);
        let expected: Value =
            serde_json::from_str(case["upstream"]["body"].as_str().unwrap()).unwrap();
        assert_eq!(chat, expected);
    }

    #[test]
    fn stream_emits_expected_event_order() {
        let upstream = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pong\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n\
data: [DONE]\n\n";
        let mut stream = ResponsesStream::new("deepseek-v4-flash");
        let mut out = stream.feed(upstream.as_bytes());
        out.extend(stream.finish());
        let text = String::from_utf8_lossy(&out).to_string();
        let names: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("event: "))
            .collect();
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert!(text.contains("\"text\":\"pong\""));
        assert!(text.contains("\"status\":\"completed\""));
    }

    #[test]
    fn incomplete_stream_emits_response_failed() {
        let upstream = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half\"}}]}\n\n";
        let mut stream = ResponsesStream::new("m");
        let mut out = stream.feed(upstream.as_bytes());
        out.extend(stream.finish());
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("response.failed"));
    }

    #[test]
    fn tool_call_emits_function_call_events() {
        let frame1 = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}}]}}]});
        let frame2 = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]});
        let upstream = format!("data: {frame1}\n\ndata: {frame2}\n\n");
        let mut stream = ResponsesStream::new("m");
        let mut out = stream.feed(upstream.as_bytes());
        out.extend(stream.finish());
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("response.function_call_arguments.delta"));
        assert!(text.contains("response.function_call_arguments.done"));
        assert!(text.contains("\"type\":\"function_call\""));
    }
}
