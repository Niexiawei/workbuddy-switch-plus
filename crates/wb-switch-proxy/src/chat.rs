//! 单账号 chat 透传：请求规范化 → 上游 SSE → 透传 / 聚合（P1 收尾）。
//!
//! 池化与多账号属 P4；这里只取「第一个有可用 token 的账号」，输出与 Python
//! 版的单账号路径一致。流式部分带空闲超时与终态帧判定：没有 finish / [DONE]
//! 就补一帧错误，绝不把半截回答当成功。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::errors::{self, Failure};
use crate::http::AppState;
use crate::{accounts, profile, wire};

/// 空闲上限：超过这么久没有新分片就判定连接已死（与 Python 版一致）。
const STREAM_IDLE: Duration = Duration::from_secs(300);

/// SSE 增量守卫：统计终态、内容与流内错误。
pub struct StreamGuard {
    buffer: Vec<u8>,
    finish: Option<String>,
    done: bool,
    saw_content: bool,
    error: Option<Failure>,
}

impl Default for StreamGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamGuard {
    pub fn new() -> Self {
        Self { buffer: Vec::new(), finish: None, done: false, saw_content: false, error: None }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=index).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                self.done = true;
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(error) = value.get("error") {
                let code = error.get("code").and_then(Value::as_i64);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("msg").and_then(Value::as_str))
                    .unwrap_or("upstream error");
                self.error = Some(errors::classify(400, &json!({"code": code}).to_string(), None))
                    .map(|mut failure| {
                        failure.message = message.to_string();
                        failure
                    });
                let _ = code;
                continue;
            }
            if let Some(choices) = value.get("choices").and_then(Value::as_array) {
                for choice in choices {
                    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                        if !reason.is_empty() {
                            self.finish = Some(reason.to_string());
                        }
                    }
                    if let Some(delta) = choice.get("delta") {
                        let has_content = delta.get("content").and_then(Value::as_str).map(|text| !text.is_empty())
                            .unwrap_or(false)
                            || delta.get("reasoning_content").and_then(Value::as_str).is_some()
                            || delta.get("tool_calls").and_then(Value::as_array).map(|calls| !calls.is_empty())
                                .unwrap_or(false);
                        if has_content {
                            self.saw_content = true;
                        }
                    }
                }
            }
        }
    }

    /// 流结束时判定：有流内错误优先，其次是「没有终态帧」。
    pub fn failure(&self) -> Option<Failure> {
        if let Some(error) = &self.error {
            return Some(error.clone());
        }
        if self.done || self.finish.is_some() {
            return None;
        }
        let kind = if self.saw_content {
            errors::INCOMPLETE_STREAM
        } else {
            errors::EMPTY_RESPONSE
        };
        Some(errors::classify(0, "", Some(kind)))
    }
}

fn sse_error(failure: &Failure) -> Bytes {
    let payload = errors::error_payload(failure);
    Bytes::from(format!("data: {}\n\n", payload))
}

/// 供适配器复用的 502 错误响应。
pub fn transport_error_response(message: &str) -> Response {
    let failure = errors::classify(0, message, Some(errors::UNAVAILABLE));
    (StatusCode::BAD_GATEWAY, Json(errors::error_payload(&failure))).into_response()
}

fn not_implemented(message: &str) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": {"message": message, "type": "not_implemented", "code": 501}})),
    )
        .into_response()
}

fn upstream_headers(view: &accounts::AccountView, token: &str, incoming: &BTreeMap<String, String>,
                     profile: &str) -> BTreeMap<String, String> {
    let mut base = profile::lower_headers([
        ("content-type", "application/json"),
        ("accept", "application/json"),
        ("user-agent", "codebuddy2openai/2.0"),
    ]);
    base.insert("authorization".into(), format!("Bearer {token}"));
    base.insert("x-user-id".into(), view.uid.clone());
    base.insert("x-domain".into(), if view.domain.is_empty() {
        "www.codebuddy.cn".to_string()
    } else {
        view.domain.clone()
    });
    profile::merge_outbound_headers(base, incoming, profile)
}

pub fn desensitize_enabled() -> bool {
    std::env::var("WB_PROXY_DESENSITIZE").map(|value| value == "1").unwrap_or(false)
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // 不继承 HTTP(S)_PROXY：桌面端是直连，继承会把请求导进系统代理。
            .no_proxy()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client")
    })
}

/// 打开一次上游调用：选号、组头、发送、把非 2xx 转成客户端错误。
///
/// 适配器（Anthropic / Responses）只需要拿到「成功的上游响应」，不必重复这段。
pub async fn open_upstream(
    state: &Arc<AppState>,
    incoming: &BTreeMap<String, String>,
    body: &Value,
    key: Option<&crate::keys::KeyInfo>,
) -> Result<reqwest::Response, Response> {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    let affinity = incoming
        .get("x-conversation-id")
        .or_else(|| incoming.get("x-session-id"))
        .map(String::as_str);
    let selected = state
        .pool
        .select(model, affinity, key.and_then(|key| key.realm.as_deref()))
        .map_err(|error| pool_error_response(error))?;
    state.pool.acquire(&selected.account_id);

    let profile = profile::detect(incoming);
    let headers = upstream_headers(&selected.account, &selected.token, incoming, profile);
    let mut request = http_client().post(state.cfg.chat_url()).json(body);
    for (name, value) in &headers {
        if let (Ok(name), Ok(value)) = (name.parse::<header::HeaderName>(), HeaderValue::from_str(value)) {
            request = request.header(name, value);
        }
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            let failure = errors::classify(0, &error.to_string(), Some(errors::UNAVAILABLE));
            state.pool.record(&selected.account_id, 502, model, Some(&failure));
            state.pool.release(&selected.account_id);
            return Err((StatusCode::BAD_GATEWAY, Json(errors::error_payload(&failure))).into_response());
        }
    };
    let status = response.status();
    if status.is_success() {
        state.pool.record(&selected.account_id, status.as_u16(), model, None);
        state.pool.release(&selected.account_id);
        return Ok(response);
    }
    let text = response.text().await.unwrap_or_default();
    let failure = errors::classify(status.as_u16(), &text, None);
    state.pool.record(&selected.account_id, status.as_u16(), model, Some(&failure));
    state.pool.release(&selected.account_id);
    let code = StatusCode::from_u16(failure.http_status).unwrap_or(StatusCode::BAD_GATEWAY);
    Err((code, Json(errors::error_payload(&failure))).into_response())
}

fn pool_error_response(error: crate::pool::PoolError) -> Response {
    use crate::pool::PoolError;
    let message = match error {
        PoolError::NoAccount(message) => message,
    };
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": {"message": message, "type": "no_account", "code": 503}})),
    )
        .into_response()
}

pub async fn handle_chat(
    state: Arc<AppState>,
    incoming: BTreeMap<String, String>,
    payload: Value,
    key: Option<crate::keys::KeyInfo>,
) -> Response {
    let client_stream = payload.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let profile = profile::detect(&incoming);
    let mut body = wire::normalize_chat(&payload, profile);
    // 脱敏默认关闭（与 Python converter 默认一致）：golden 也是关着录的。
    // 需要缓解客户端合规模板被误判时开 WB_PROXY_DESENSITIZE=1。
    if desensitize_enabled() {
        crate::desensitize::desensitize_body(&mut body, &["system", "developer"]);
    }

    let response = match open_upstream(&state, &incoming, &body, key.as_ref()).await {
        Ok(response) => response,
        Err(response) => return response,
    };
    if client_stream {
        return stream_response(response);
    }
    aggregate_response(response, false).await
}

fn stream_response(response: reqwest::Response) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    tokio::spawn(async move {
        let mut stream = response.bytes_stream();
        let mut guard = StreamGuard::new();
        loop {
            match tokio::time::timeout(STREAM_IDLE, stream.next()).await {
                Err(_) => {
                    let failure = errors::classify(0, "", Some(errors::STREAM_TIMEOUT));
                    let _ = tx.send(sse_error(&failure)).await;
                    break;
                }
                Ok(None) => break,
                Ok(Some(Ok(chunk))) => {
                    guard.feed(&chunk);
                    if tx.send(chunk).await.is_err() {
                        return;
                    }
                }
                Ok(Some(Err(error))) => {
                    let failure = errors::classify(0, &error.to_string(), Some(errors::UNAVAILABLE));
                    let _ = tx.send(sse_error(&failure)).await;
                    break;
                }
            }
        }
        if let Some(failure) = guard.failure() {
            let _ = tx.send(sse_error(&failure)).await;
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (Ok::<_, std::io::Error>(item), rx))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn aggregate_response(response: reqwest::Response, _client_stream: bool) -> Response {
    let text = match response.text().await {
        Ok(text) => text,
        Err(error) => {
            let failure = errors::classify(0, &error.to_string(), Some(errors::UNAVAILABLE));
            return (StatusCode::BAD_GATEWAY, Json(errors::error_payload(&failure))).into_response();
        }
    };
    let mut guard = StreamGuard::new();
    guard.feed(text.as_bytes());
    if let Some(failure) = guard.failure() {
        let code = StatusCode::from_u16(failure.http_status).unwrap_or(StatusCode::BAD_GATEWAY);
        return (code, Json(errors::error_payload(&failure))).into_response();
    }
    match aggregate_sse(&text) {
        Some(completion) => Json(completion).into_response(),
        None => not_implemented("上游响应无法聚合为 chat.completion"),
    }
}

/// 把上游 SSE 聚合成非流式 chat.completion（与 Python 版同形）。
pub fn aggregate_sse(text: &str) -> Option<Value> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut finish: Option<String> = None;
    let mut usage: Option<Value> = None;
    let mut model = String::new();

    for line in text.lines() {
        let Some(data) = line.trim().strip_prefix("data:") else { continue };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
        if let Some(name) = value.get("model").and_then(Value::as_str) {
            model = name.to_string();
        }
        if let Some(tokens) = value.get("usage") {
            usage = Some(tokens.clone());
        }
        let Some(choices) = value.get("choices").and_then(Value::as_array) else { continue };
        let Some(choice) = choices.first() else { continue };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            if !reason.is_empty() {
                finish = Some(reason.to_string());
            }
        }
        let Some(delta) = choice.get("delta") else { continue };
        if let Some(chunk) = delta.get("content").and_then(Value::as_str) {
            content.push_str(chunk);
        }
        if let Some(chunk) = delta.get("reasoning_content").and_then(Value::as_str) {
            reasoning.push_str(chunk);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while tool_calls.len() <= index {
                    tool_calls.push(json!({"id": "", "type": "function", "function": {"name": "", "arguments": ""}}));
                }
                let slot = &mut tool_calls[index];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        slot["id"] = json!(id);
                    }
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        if !name.is_empty() {
                            slot["function"]["name"] = json!(name);
                        }
                    }
                    if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                        let merged = format!(
                            "{}{}",
                            slot["function"]["arguments"].as_str().unwrap_or(""),
                            args
                        );
                        slot["function"]["arguments"] = json!(merged);
                    }
                }
            }
        }
    }

    if content.is_empty() && reasoning.is_empty() && tool_calls.is_empty() && finish.is_none() {
        return None;
    }
    let mut message = json!({"role": "assistant", "content": content});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let mut completion = json!({
        "id": "chatcmpl-rust-proxy",
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
    });
    if let Some(usage) = usage {
        completion["usage"] = usage;
    }
    Some(completion)
}
