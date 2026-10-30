//! HTTP 路由（**纯数据面**）。
//!
//! 对外只暴露：`/health`、`/v1/models`、4 条对话接口、`/v1/usage`——
//! 这就是「智能体 API 协议接口」的全部。管理面不在 HTTP 上：
//! `crate::admin::router()` 由桌面端进程内调用（Tauri 命令 → tower `oneshot`），
//! 见 docs/reverse-proxy-internal-plane-plan.md 附录 C。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use crate::accounts;
use crate::anthropic;
use crate::catalog;
use crate::chat;
use crate::config::ProxyConfig;
use crate::profile;
use crate::projection;
use crate::prompt;
use crate::responses;
use crate::state::StateStore;
use crate::usage;

pub struct AppState {
    pub cfg: ProxyConfig,
    pub store: Arc<StateStore>,
    pub pool: Arc<crate::pool::Pool>,
    pub metrics: Arc<crate::metrics::Metrics>,
    pub started_at: Instant,
    /// 模型目录缓存（60s）。放这里而不是模块级静态：测试之间不会串。
    pub catalog: std::sync::Mutex<Option<(Instant, Vec<Value>)>>,
}

impl AppState {
    pub fn new(cfg: ProxyConfig, store: StateStore) -> Self {
        let store = Arc::new(store);
        let pool = Arc::new(crate::pool::Pool::new(
            store.clone(),
            cfg.accounts_path.clone(),
        ));
        Self {
            cfg,
            store,
            pool,
            metrics: Arc::new(crate::metrics::Metrics::new()),
            started_at: Instant::now(),
            catalog: std::sync::Mutex::new(None),
        }
    }

    /// 管理面通道的**降级态**：state.json 打不开（极端环境）时兜底，让通道仍可
    /// 装配——调用会得到明确的 403/500 错误，而不是整个通道 Err。正常路径不用它。
    pub fn for_degraded() -> Self {
        let dir = std::env::temp_dir().join("wb-switch-degraded-admin");
        let cfg = ProxyConfig::for_user()
            .with_data_dir(&dir)
            .with_accounts(dir.join("accounts.json"));
        let store = StateStore::open(cfg.state_path()).unwrap_or_else(|_| {
            // 连 temp 都打不开：最后兜底（写一个内存外的假文件）。正常机器到不了这里。
            let fallback = dir.join("state.degraded.json");
            std::fs::write(&fallback, "{}").ok();
            StateStore::open(&fallback).expect("降级态 state 无法构建")
        });
        Self::new(cfg, store)
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/usage", get(gateway_usage))
        .route("/v1/chat/completions", post(chat_handler))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/messages", post(messages_handler))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .with_state(state)
    // 不挂 CORS：本 router 的消费者是 CLI / SDK（非浏览器），没有预检问题；
    // 曾经的 CORS 层（2026-10-30 a861be5）是为跨源打管理面加的，管理面离开
    // HTTP 后它失去了存在意义。
}

/// 健康检查。刻意不要求鉴权：外部客户端与宿主都靠它判断「服务起来没有」。
///
/// `desiredPort` / `portShifted` 让排障一眼看出端口是否发生过偏移
/// （自动 +1，见 `crate::ports`）。
async fn health(
    State(state): State<Arc<AppState>>,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Json<Value> {
    let peer = crate::peer::peer_from_connect(connect);
    let accounts = accounts::list_at(&state.cfg.accounts_path);
    Json(json!({
        "status": "ok",
        "mode": "rust",
        "version": env!("CARGO_PKG_VERSION"),
        "engine": "rust",
        "uptimeSeconds": state.started_at.elapsed().as_secs(),
        "accounts": accounts.len(),
        "dataDir": state.cfg.data_dir.to_string_lossy(),
        "accountsPath": state.cfg.accounts_path.to_string_lossy(),
        "stateWritable": state.store.writable(),
        // 未知来源（无 ConnectInfo）按非回环处理，与 peer 模块的保守口径一致。
        "peerIsLoopback": peer.map(|ip| ip.is_loopback()).unwrap_or(false),
        // 能力边界：客户端不应把 501 当成临时故障重试。管理面不在 HTTP 上（进程内通道）。
        "capabilities": {
            "models": true,
            "chat": true,
            "admin": false,
            "auth": false,
            "usage": true,
            "ipAllowlist": true,
        },
    }))
}

/// `GET /v1/usage`：网关密钥自查（对照 Python 的 `gateway_key_usage`）。
async fn gateway_usage(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    usage::handle(&state.store, &headers)
}

async fn chat_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    Json(payload): Json<Value>,
) -> Response {
    let peer = crate::peer::peer_from_connect(connect);
    let model = payload.get("model").and_then(Value::as_str);
    let key = match crate::keys::authenticate(&state.store, &headers, model, peer) {
        Ok(key) => key,
        Err(response) => return response,
    };
    // 指标打点：与 Python MetricsMiddleware 同口径，只记数据面四条路径。
    let started = std::time::Instant::now();
    state.metrics.begin();
    let response = chat::handle_chat(state.clone(), incoming_headers(&headers), payload, key).await;
    let ok = response.status().is_success();
    state.metrics.finish(
        "/v1/chat/completions",
        "api",
        response.status().as_u16(),
        ok,
        started.elapsed().as_millis() as u64,
        if ok { "success" } else { "http_error" },
    );
    response
}

async fn messages_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    Json(payload): Json<Value>,
) -> Response {
    let peer = crate::peer::peer_from_connect(connect);
    let key = match crate::keys::authenticate(
        &state.store,
        &headers,
        payload.get("model").and_then(Value::as_str),
        peer,
    ) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let incoming = incoming_headers(&headers);
    let client_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut body = anthropic::request_to_chat(&payload);
    // 适配器已经拼好 system 消息，仍需过一遍身份过滤与 11128 改写。
    prompt::normalize_body(&mut body);
    if chat::desensitize_enabled() {
        crate::desensitize::desensitize_body(&mut body, &["system", "developer"]);
    }
    let response = match chat::open_upstream(&state, &incoming, &body, key.as_ref()).await {
        Ok(response) => response,
        Err(response) => return response,
    };
    if client_stream {
        return anthropic::stream_response(response, model.clone());
    }
    let text = match response.text().await {
        Ok(text) => text,
        Err(error) => return chat::transport_error_response(&error.to_string()),
    };
    match chat::aggregate_sse(&text) {
        Some(completion) => Json(anthropic::completion_to_message(&completion, &model)).into_response(),
        None => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"type": "error", "error": {"type": "invalid_response", "message": "上游响应无法聚合"}})),
        )
            .into_response(),
    }
}

async fn responses_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    Json(payload): Json<Value>,
) -> Response {
    let peer = crate::peer::peer_from_connect(connect);
    let key = match crate::keys::authenticate(
        &state.store,
        &headers,
        payload.get("model").and_then(Value::as_str),
        peer,
    ) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let incoming = incoming_headers(&headers);
    let client_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut body = responses::request_to_chat(&payload);
    prompt::normalize_body(&mut body);
    if chat::desensitize_enabled() {
        crate::desensitize::desensitize_body(&mut body, &["system", "developer"]);
    }
    // 长上下文投影：默认关闭（与 Python 的 CODEBUDDY_LOSSY_PROJECTION 默认一致），
    // 需要压缩 agentic 脚手架时设 WB_PROXY_LOSSY_PROJECTION=1。
    let (projected, _stats) = projection::project(&body, projection::preserve_by_default());
    body = projected;
    let response = match chat::open_upstream(&state, &incoming, &body, key.as_ref()).await {
        Ok(response) => response,
        Err(response) => return response,
    };
    if client_stream {
        return responses::stream_response(response, model);
    }
    let text = match response.text().await {
        Ok(text) => text,
        Err(error) => return chat::transport_error_response(&error.to_string()),
    };
    match chat::aggregate_sse(&text) {
        Some(completion) => Json(responses::completion_to_response(&completion, &model)).into_response(),
        None => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"type": "error", "error": {"type": "invalid_response", "message": "上游响应无法聚合"}})),
        )
            .into_response(),
    }
}

async fn count_tokens(Json(payload): Json<Value>) -> Json<Value> {
    // 粗估：4 个字符 ≈ 1 token；只用于客户端的上下文预算，不参与计费。
    let text = payload
        .get("messages")
        .map(|messages| messages.to_string())
        .unwrap_or_default()
        + &payload
            .get("system")
            .map(|system| system.to_string())
            .unwrap_or_default();
    Json(serde_json::json!({"input_tokens": (text.chars().count() / 4).max(1)}))
}

fn incoming_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let pairs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    profile::lower_headers(
        pairs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
}

async fn models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = crate::peer::peer_from_connect(connect);
    if let Err(response) = crate::keys::authenticate(&state.store, &headers, None, peer) {
        return response;
    }
    Json(json!({ "object": "list", "data": catalog::load(&state).await })).into_response()
}
