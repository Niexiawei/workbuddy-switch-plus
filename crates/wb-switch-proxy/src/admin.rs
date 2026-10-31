//! 管理面：进程内通道的唯一服务对象。
//!
//! HTTP 上不再挂载本 router（数据面与它是唯一的对外 HTTP 面，见 `crate::http`）；
//! 桌面端经 Tauri 命令 `proxy_admin_request` → tower `oneshot` 直接调用。
//! 因此这里**没有鉴权**：能构造成本 router 请求的只有宿主进程自己。
//! 新增端点必须 JSON-in/JSON-out（通道不做流式），见 docs/reverse-proxy-internal-plane-plan.md §3.1。

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Json as JsonBody, Router};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::accounts;
use crate::catalog;
use crate::http::AppState;

/// 进程内通道的固定「操作者」三元组。保留返回形状是为了让 handler 侧的
/// `authorize(...)` 调用点零改动——曾经这里还有宿主令牌与会话两条分支，
/// 管理面离开 HTTP 后（同上）它们整体退役，只剩这个常量。
fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<std::net::IpAddr>,
) -> Result<(String, String, &'static str), Response> {
    let _ = (state, headers, peer);
    Ok(("in-process".to_string(), String::new(), "admin"))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn random_id(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tick = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(format!("{prefix}:{}:{tick}:{}", now(), std::process::id()).as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{prefix}{hex}")
}

/// 把 `ConnectInfo` 转成鉴权需要的来源 IP（测试里没有注入时是 `None`）。
fn peer_of(connect: Option<ConnectInfo<std::net::SocketAddr>>) -> Option<std::net::IpAddr> {
    connect.map(|ConnectInfo(addr)| addr.ip())
}

pub fn router() -> Router<std::sync::Arc<AppState>> {
    Router::new()
        .route("/overview", get(overview))
        .route("/pool/summary", get(pool_summary))
        .route("/keys", get(list_keys).post(create_key))
        .route("/keys/check-models", post(check_models))
        .route(
            "/keys/:id",
            axum::routing::patch(update_key).delete(delete_key),
        )
        .route("/keys/:id/reset-usage", post(reset_usage))
        .route("/models", get(models))
        .route("/pool/settings", axum::routing::patch(pool_settings))
        .route("/accounts/:aid/actions/:action", post(account_action))
        .route("/pool/actions/:action", post(pool_action))
        .route("/pool/checkin-logs", get(checkin_logs))
        .route("/pool/checkin-logs/summary", get(checkin_logs_summary))
        .route("/pool/checkin-logs/clear", post(clear_checkin_logs))
        .route("/groups", get(list_groups).post(create_group))
        .route(
            "/groups/:gid",
            axum::routing::patch(update_group).delete(delete_group),
        )
        .route("/egress", get(egress_view))
        .route("/egress/lines", post(create_egress_line))
        .route(
            "/egress/lines/:lid",
            axum::routing::patch(update_egress_line).delete(delete_egress_line),
        )
        .route("/egress/assign", post(assign_egress))
        .route(
            "/security/config",
            get(security_config).post(save_security_config),
        )
        .route("/security/rules", get(list_ip_rules).post(create_ip_rule))
        .route(
            "/security/rules/:rid",
            axum::routing::delete(delete_ip_rule),
        )
        .route("/security/logs", get(ip_logs))
        .route("/security/logs/clear", post(clear_ip_logs))
        .route("/request-logs", get(request_logs))
        .route("/usage/stats", get(usage_stats))
        .route("/model-aliases", get(list_aliases).post(save_alias))
        .route(
            "/accounts/:aid",
            axum::routing::patch(patch_account).delete(delete_account),
        )
        .route("/accounts/:aid/limits", axum::routing::put(account_limits))
        .route("/accounts", post(create_account_stub))
        .route("/accounts/credits-snapshot", get(credits_snapshot))
        .route("/keys/:id/export", get(export_key))
        .route("/settings", get(settings).put(save_settings))
        .route("/alerts", get(alerts).put(save_alerts))
        .route("/alerts/test", post(alerts_test))
        .route("/cli/key", get(cli_key_get).delete(cli_key_revoke))
        .route("/cli/key/rotate", post(cli_key_rotate))
        .route("/oauth/start", post(oauth_stub))
        .route("/oauth/batch/start", post(oauth_stub))
        .route("/oauth/:fid/poll", post(oauth_stub))
        .route("/oauth/:fid", axum::routing::delete(oauth_stub))
        .route("/test", post(chat_test))
}

fn public_key(entry: &Value, reveal: bool) -> Value {
    json!({
        "id": entry.get("id").cloned().unwrap_or(Value::Null),
        "name": entry.get("name").cloned().unwrap_or_else(|| json!("")),
        "hint": entry.get("hint").cloned().unwrap_or_else(|| json!("")),
        "enabled": entry.get("enabled").cloned().unwrap_or_else(|| json!(true)),
        "expires_at": entry.get("expires_at").cloned().unwrap_or(Value::Null),
        "realm": entry.get("realm").cloned().unwrap_or_else(|| json!("")),
        "models": entry.get("models").cloned().unwrap_or_else(|| json!([])),
        "quota": entry.get("quota").cloned().unwrap_or_else(|| json!(0)),
        "used_tokens": entry.get("used_tokens").cloned().unwrap_or_else(|| json!(0)),
        // IP 白名单要与写入路径对称：列表看不到，用户就无从判断"配了为什么没用"。
        "ip_allowlist": entry.get("ip_allowlist").cloned().unwrap_or_else(|| json!([])),
        "max_ips": entry.get("max_ips").cloned().unwrap_or_else(|| json!(0)),
        "purpose": entry.get("purpose").cloned().unwrap_or_else(|| json!("")),
        "secret": if reveal { entry.get("secret").cloned().unwrap_or_else(|| json!("")) } else { json!("") },
    })
}

async fn keys_of(state: &AppState, reveal: bool) -> Vec<Value> {
    let value = state.store.get("keys").unwrap_or_else(|| json!({}));
    value
        .as_object()
        .map(|keys| {
            keys.iter()
                .map(|(id, entry)| {
                    let mut row = entry.clone();
                    row["id"] = json!(id);
                    public_key(&row, reveal)
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn overview(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    let (_, _, role) = match authorize(&state, &headers, peer) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let reveal = role == "admin";
    let rows = state
        .pool
        .rows(&accounts::list_at(&state.cfg.accounts_path));
    let keys = keys_of(&state, reveal).await;
    let models: Vec<String> = catalog::load(&state)
        .await
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    Json(json!({
        "accounts": rows,
        "pool": state.store.get("pool").unwrap_or_else(|| json!({})),
        // 概览页消费 metrics.recent 等字段；曾返回空 {} 导致 m.recent.map 白屏。
        "metrics": state.metrics.snapshot(),
        "keys": keys,
        "models": models,
        "default_model": models.first().cloned().unwrap_or_default(),
        "checkin_logs": [],
        "request_logs": [],
        // Python overview 的 groups 是数组（groups.list_groups 输出）；
        // 曾错误返回 state.json 原始对象，keys-panel 对它 .map 崩。
        "groups": groups_list(&state.store),
        "uptime": state.started_at.elapsed().as_secs(),
        "runtime": {"session": state.started_at.elapsed().as_secs(), "total": state.started_at.elapsed().as_secs()},
        "events": [],
    })).into_response()
}

async fn pool_summary(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let accounts = state
        .pool
        .rows(&accounts::list_at(&state.cfg.accounts_path));
    Json(json!({
        "accounts": accounts,
        "pool": state.store.get("pool").unwrap_or_else(|| json!({})),
    }))
    .into_response()
}

async fn list_keys(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    let (_, _, role) = match authorize(&state, &headers, peer) {
        Ok(value) => value,
        Err(response) => return response,
    };
    Json(json!(keys_of(&state, role == "admin").await)).into_response()
}

async fn models(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    // Python /admin/api/models 返回 { models, total, free_count, default_model }；
    // 数据面 /v1/models 才是裸数组（OpenAI 协议）。曾返回裸数组导致模型页 .length 崩。
    let entries = catalog::load(&state).await;
    let total = entries.len();
    // free 判据与 Python is_free_model 一致：只有 tags；宁可漏判也不误判。
    let is_free = |entry: &Value| -> bool {
        entry
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .any(|tag| tag.contains("免费") || tag.to_lowercase().contains("free"))
            })
            .unwrap_or(false)
    };
    let free_count = entries.iter().filter(|e| is_free(e)).count();
    Json(json!({
        "models": entries,
        "total": total,
        "free_count": free_count,
        "default_model": entries.first().and_then(|e| e.get("id")).cloned().unwrap_or_else(|| json!("")),
    }))
    .into_response()
}

async fn settings(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    Json(state.store.get("settings").unwrap_or_else(|| json!({}))).into_response()
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn new_token() -> String {
    format!("sk-wb-{}", random_id(""))
}

fn write_failed(error: std::io::Error) -> Response {
    let status = if error.kind() == std::io::ErrorKind::PermissionDenied {
        StatusCode::FORBIDDEN
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        status,
        Json(json!({"detail": format!("状态写入失败：{error}")})),
    )
        .into_response()
}

fn clean_name(value: Option<&Value>) -> Option<String> {
    let text = value.and_then(Value::as_str).unwrap_or("").trim();
    if text.is_empty() {
        None
    } else {
        Some(text.chars().take(60).collect())
    }
}

async fn create_key(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let Some(name) = clean_name(body.get("name")) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "密钥名称不能只有空格"})),
        )
            .into_response();
    };
    let token = new_token();
    let kid = random_id("key_");
    let entry = json!({
        "name": name,
        "hash": hash_token(&token),
        "secret": token,
        "hint": format!("{}…{}", &token[..5], &token[token.len() - 4..]),
        "prefix": token.chars().take(12).collect::<String>(),
        "enabled": body.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        "expires_at": body.get("expires_at").cloned().unwrap_or(Value::Null),
        "max_ips": body.get("max_ips").and_then(Value::as_i64).unwrap_or(0),
        "ip_allowlist": body.get("ip_allowlist").cloned().unwrap_or_else(|| json!([])),
        "models": body.get("models").cloned().unwrap_or_else(|| json!([])),
        "realm": body.get("realm").and_then(Value::as_str).unwrap_or(""),
        "groups": [],
        "quota": body.get("quota").and_then(Value::as_i64).unwrap_or(0),
        "used_tokens": 0,
        "quota_credit": 0,
        "used_credit": 0.0,
        "created": now(),
        "last_used_at": Value::Null,
        "created_by": "rust-admin",
        "purpose": "",
    });
    let mut too_many = false;
    let result = state.store.update(|value| {
        let keys = value["keys"].as_object_mut().expect("keys 必须是对象");
        if keys.len() >= 100 {
            too_many = true;
            return;
        }
        keys.insert(kid.clone(), entry.clone());
    });
    if let Err(error) = result {
        return write_failed(error);
    }
    if too_many {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "最多保存 100 枚 API Key"})),
        )
            .into_response();
    }
    Json(json!({"id": kid, "key": token})).into_response()
}

async fn update_key(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let mut missing = false;
    let result = state.store.update(|value| {
        let Some(keys) = value["keys"].as_object_mut() else {
            missing = true;
            return;
        };
        let Some(item) = keys.get_mut(&id) else {
            missing = true;
            return;
        };
        if let Some(name) = clean_name(body.get("name")) {
            item["name"] = json!(name);
        }
        if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
            item["enabled"] = json!(enabled);
        }
        if let Some(expires) = body.get("expires_at") {
            item["expires_at"] = expires.clone();
        }
        if let Some(models) = body.get("models") {
            item["models"] = models.clone();
        }
        if let Some(realm) = body.get("realm") {
            item["realm"] = realm.clone();
        }
        if let Some(quota) = body.get("quota") {
            item["quota"] = quota.clone();
        }
        // IP 白名单：与数据面 keys::ip_allowed 同一套字段（I4 起真正生效）。
        if let Some(allowlist) = body.get("ip_allowlist") {
            item["ip_allowlist"] = allowlist.clone();
        }
        if let Some(max_ips) = body.get("max_ips") {
            item["max_ips"] = max_ips.clone();
        }
    });
    if let Err(error) = result {
        return write_failed(error);
    }
    if missing {
        return (StatusCode::NOT_FOUND, Json(json!({"detail": "密钥不存在"}))).into_response();
    }
    Json(json!({"ok": true})).into_response()
}

async fn delete_key(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let mut missing = false;
    let mut last = false;
    let result = state.store.update(|value| {
        let Some(keys) = value["keys"].as_object_mut() else {
            missing = true;
            return;
        };
        if !keys.contains_key(&id) {
            missing = true;
            return;
        }
        if keys.len() <= 1 {
            last = true;
            return;
        }
        keys.remove(&id);
    });
    if let Err(error) = result {
        return write_failed(error);
    }
    if missing {
        return (StatusCode::NOT_FOUND, Json(json!({"detail": "密钥不存在"}))).into_response();
    }
    if last {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "请先创建一枚新密钥，再撤销最后一枚密钥"})),
        )
            .into_response();
    }
    Json(json!({"ok": true})).into_response()
}

async fn reset_usage(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let mut missing = false;
    let result = state.store.update(|value| {
        let Some(keys) = value["keys"].as_object_mut() else {
            missing = true;
            return;
        };
        let Some(item) = keys.get_mut(&id) else {
            missing = true;
            return;
        };
        item["used_tokens"] = json!(0);
        item["used_credit"] = json!(0.0);
    });
    if let Err(error) = result {
        return write_failed(error);
    }
    if missing {
        return (StatusCode::NOT_FOUND, Json(json!({"detail": "密钥不存在"}))).into_response();
    }
    Json(json!({"ok": true})).into_response()
}

async fn check_models(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let known: Vec<String> = catalog::load(&state)
        .await
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let requested: Vec<String> = body
        .get("models")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let unknown: Vec<&String> = requested
        .iter()
        .filter(|model| !known.contains(model))
        .collect();
    Json(json!({"checked": true, "unknown": unknown})).into_response()
}

fn account_record(state: &AppState, aid: &str) -> Option<Value> {
    wb_switch_core::modules::account::load_accounts_at(&state.cfg.accounts_path)
        .into_iter()
        .find(|row| {
            row.get("id").and_then(Value::as_str) == Some(aid)
                || row.get("uid").and_then(Value::as_str) == Some(aid)
        })
}

async fn pool_settings(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    if let Some(routing) = body.get("routing").and_then(Value::as_str) {
        if !matches!(routing, "round_robin" | "manual" | "least_used") {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": "未知的调度方式"})),
            )
                .into_response();
        }
    }
    let allowed = [
        "routing",
        "auto_checkin",
        "checkin_time",
        "auto_travel",
        "travel_time",
        "auto_keepalive",
        "keepalive_hours",
        "auto_active",
        "active_time",
        "active_reports",
        "spread",
        "spread_minutes",
        "default_weight",
        "default_max_concurrency",
        "default_min_interval",
        "max_total_concurrency",
        "retry_enabled",
    ];
    let mut updated = Value::Null;
    let result = state.store.update(|value| {
        let pool = value["pool"].as_object_mut().expect("pool 必须是对象");
        for key in allowed {
            if let Some(item) = body.get(key) {
                pool.insert(key.to_string(), item.clone());
            }
        }
        updated = Value::Object(pool.clone());
    });
    if let Err(error) = result {
        return write_failed(error);
    }
    Json(json!({"ok": true, "pool": updated})).into_response()
}

async fn account_action(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path((aid, action)): axum::extract::Path<(String, String)>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    // 未知动作在这里就 404，不再落到「尚未迁移」——那会把「拼错了」和
    // 「还没做」糊成同一句话，排查时往完全错误的方向找。
    let Some(action) = crate::tasks::Action::parse(&action) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": format!("未知动作：{action}")})),
        )
            .into_response();
    };
    let result = state
        .tasks
        .operate(&aid, action, crate::tasks::Source::Manual)
        .await;
    // 单个动作的失败（账号不存在 / 已暂停 / 正在执行）是**结果**不是传输错误：
    // 一律 200，前端拿 `ok` 与 `message` 自己展示。只有状态写入失败才是 500。
    Json(result).into_response()
}

async fn pool_action(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(action): axum::extract::Path<String>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let Some(action) = crate::tasks::Action::parse(&action) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": format!("未知动作：{action}")})),
        )
            .into_response();
    };
    match state.tasks.batch(action).await {
        Some(result) => Json(result).into_response(),
        // 与 Python 同文案：批量是资源，同一时刻只允许一个。
        None => (
            StatusCode::CONFLICT,
            Json(json!({"detail": "已有批量任务进行中，请稍后刷新"})),
        )
            .into_response(),
    }
}

// ── 其余管理端点（P5 收口）────────────────────────────────────────────────

fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"detail": message}))).into_response()
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"detail": message}))).into_response()
}

fn still_python(what: &str) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"detail": format!("{what} 尚未迁移到 Rust 引擎，请使用 Python 服务")})),
    )
        .into_response()
}

fn ok() -> Response {
    Json(json!({"ok": true})).into_response()
}

async fn checkin_logs(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    // `limit` / `kind` 是前端筛选项直接发过来的查询参数（`accounts-panel.tsx`
    // 的 loadTaskLogs）。不接的话「全部类别 / 签到 / 派猫猫旅行 / 凭据保活 /
    // 活跃上报」这个下拉点了没反应。
    let params = query_params(query.as_deref());
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(200);
    let kind = params
        .get("kind")
        .map(String::as_str)
        .filter(|value| !value.is_empty() && *value != "all");
    Json(json!(state.tasks.task_logs(limit, kind))).into_response()
}

/// 解析查询串成 `k=v` 映射（值做最简百分号解码：`%20` 与 `+`）。
///
/// 自己写而不是引 `serde_urlencoded`：这里只有两个短参数，且值域限定在
/// 类别名（`checkin`/`travel`/`keepalive`/`active`），不需要完整解码器。
fn query_params(query: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in query.unwrap_or("").split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        map.insert(percent_decode(key), percent_decode(value));
    }
    map
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

async fn checkin_logs_summary(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let hours = query_params(query.as_deref())
        .get("hours")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(24);
    Json(state.tasks.task_summary(hours)).into_response()
}

async fn clear_checkin_logs(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    // 给了 `kind` 只清那一类；没给就全清（Python `clear_task_logs` 同口径）。
    let kind = query_params(query.as_deref())
        .get("kind")
        .cloned()
        .filter(|value| !value.is_empty() && value != "all");
    let mut removed = 0_i64;
    let result = state.store.update(|value| {
        let existing = value
            .get("checkin_logs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let kept: Vec<Value> = match &kind {
            Some(kind) => existing
                .iter()
                .filter(|row| {
                    row.get("kind").and_then(Value::as_str).unwrap_or("checkin") != kind.as_str()
                })
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        removed = (existing.len() - kept.len()) as i64;
        value["checkin_logs"] = json!(kept);
    });
    match result {
        Ok(()) => Json(json!({"ok": true, "removed": removed})).into_response(),
        Err(error) => write_failed(error),
    }
}

/// 把 state.json 的 groups 对象转成前端期望的数组（每项带 id）。
/// overview 与 GET /groups 共用，保证两处形状一致。
fn groups_list(store: &crate::state::StateStore) -> Vec<Value> {
    let value = store.get("groups").unwrap_or_else(|| json!({}));
    value
        .as_object()
        .map(|groups| {
            groups
                .iter()
                .map(|(id, row)| {
                    let mut row = row.clone();
                    row["id"] = json!(id);
                    row
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn list_groups(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    // Python 返回 { groups, accounts, routings }；前端勾选框直接用 accounts，
    // routings 是分组策略清单。曾只返回数组，groups-panel 解构 undefined 崩。
    let accounts: Vec<Value> =
        crate::pool::Pool::new(state.store.clone(), state.cfg.accounts_path.clone())
            .rows(&crate::accounts::list_at(&state.cfg.accounts_path))
            .iter()
            .map(|row| {
                json!({
                    "id": row["id"],
                    "name": row.get("name").cloned().unwrap_or_else(|| row["id"].clone()),
                    "pool_state": row.get("pool_state").cloned().unwrap_or_else(|| json!("")),
                    "realm": row.get("realm").cloned().unwrap_or_else(|| json!("")),
                })
            })
            .collect();
    Json(json!({
        "groups": groups_list(&state.store),
        "accounts": accounts,
        "routings": ["round_robin", "least_used", "manual"],
    }))
    .into_response()
}

async fn create_group(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let Some(name) = clean_name(body.get("name")) else {
        return bad_request("分组名称不能为空");
    };
    let id = random_id("grp_");
    let entry = json!({"name": name, "desc": body.get("desc").cloned().unwrap_or_else(|| json!("")), "enabled": true, "created": now()});
    let id_cloned = id.clone();
    match state.store.update(|value| {
        let groups = value["groups"].as_object_mut().expect("groups 必须是对象");
        groups.insert(id_cloned.clone(), entry.clone());
    }) {
        Ok(()) => Json(json!({"ok": true, "id": id})).into_response(),
        Err(error) => write_failed(error),
    }
}

async fn update_group(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(gid): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let mut missing = false;
    let result = state.store.update(|value| {
        let Some(groups) = value["groups"].as_object_mut() else {
            missing = true;
            return;
        };
        let Some(group) = groups.get_mut(&gid) else {
            missing = true;
            return;
        };
        for key in ["name", "desc", "enabled"] {
            if let Some(item) = body.get(key) {
                group[key] = item.clone();
            }
        }
    });
    match result {
        Err(error) => write_failed(error),
        Ok(()) if missing => not_found("分组不存在"),
        Ok(()) => ok(),
    }
}

async fn delete_group(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(gid): axum::extract::Path<String>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    match state.store.update(|value| {
        value["groups"]
            .as_object_mut()
            .map(|groups| groups.remove(&gid));
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn egress_view(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let egress = state
        .store
        .get("egress")
        .unwrap_or_else(|| json!({"lines": {}, "bindings": {}}));
    let lines: Vec<Value> = egress
        .get("lines")
        .and_then(Value::as_object)
        .map(|lines| {
            lines
                .iter()
                .map(|(id, row)| {
                    let mut row = row.clone();
                    row["id"] = json!(id);
                    row
                })
                .collect()
        })
        .unwrap_or_default();
    // Python 返回 { lines, accounts, orphans, stale }；曾缺 accounts，
    // egress-panel 对 undefined 读 .length 崩。
    let bindings = egress.get("bindings").cloned().unwrap_or_else(|| json!({}));
    let accounts: Vec<Value> =
        crate::pool::Pool::new(state.store.clone(), state.cfg.accounts_path.clone())
            .rows(&crate::accounts::list_at(&state.cfg.accounts_path))
            .iter()
            .map(|row| {
                let id = row["id"].as_str().unwrap_or("");
                json!({
                    "id": row["id"],
                    "name": row.get("name").cloned().unwrap_or_else(|| row["id"].clone()),
                    "pool_state": row.get("pool_state").cloned().unwrap_or_else(|| json!("")),
                    "line_id": bindings.get(id).cloned().unwrap_or_else(|| json!("")),
                })
            })
            .collect();
    // orphans：绑定指向了不存在的线路；stale：绑定指向的账号已不在池里。
    let orphans: Vec<String> = bindings
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(_aid, line_id)| {
                    !lines
                        .iter()
                        .any(|line| line.get("id").and_then(Value::as_str) == line_id.as_str())
                })
                .map(|(aid, _)| aid.clone())
                .collect()
        })
        .unwrap_or_default();
    Json(json!({
        "lines": lines,
        "accounts": accounts,
        "orphans": orphans,
        "stale": [],
    }))
    .into_response()
}

async fn create_egress_line(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let Some(name) = clean_name(body.get("name")) else {
        return bad_request("线路名称不能为空");
    };
    let Some(url) = body
        .get("url")
        .and_then(Value::as_str)
        .filter(|value| value.starts_with("http"))
    else {
        return bad_request("线路地址必须是 http(s) URL");
    };
    let id = random_id("line_");
    let entry = json!({"name": name, "url": url, "enabled": true, "created": now()});
    let id_cloned = id.clone();
    match state.store.update(|value| {
        let lines = value["egress"]["lines"]
            .as_object_mut()
            .expect("egress.lines 必须是对象");
        lines.insert(id_cloned.clone(), entry.clone());
    }) {
        Ok(()) => Json(json!({"ok": true, "id": id})).into_response(),
        Err(error) => write_failed(error),
    }
}

async fn update_egress_line(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(lid): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let mut missing = false;
    let result = state.store.update(|value| {
        let Some(lines) = value["egress"]["lines"].as_object_mut() else {
            missing = true;
            return;
        };
        let Some(line) = lines.get_mut(&lid) else {
            missing = true;
            return;
        };
        for key in ["name", "url", "enabled"] {
            if let Some(item) = body.get(key) {
                line[key] = item.clone();
            }
        }
    });
    match result {
        Err(error) => write_failed(error),
        Ok(()) if missing => not_found("线路不存在"),
        Ok(()) => ok(),
    }
}

async fn delete_egress_line(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(lid): axum::extract::Path<String>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    match state.store.update(|value| {
        value["egress"]["lines"]
            .as_object_mut()
            .map(|lines| lines.remove(&lid));
        if let Some(bindings) = value["egress"]["bindings"].as_object_mut() {
            bindings.retain(|_, line| line.as_str() != Some(lid.as_str()));
        }
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn assign_egress(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let aid = body.get("account_id").and_then(Value::as_str).unwrap_or("");
    let line = body.get("line_id").and_then(Value::as_str).unwrap_or("");
    if aid.is_empty() {
        return bad_request("缺少 account_id");
    }
    let aid = aid.to_string();
    let line = line.to_string();
    match state.store.update(|value| {
        if !value["egress"].is_object() {
            value["egress"] = json!({"lines": {}, "bindings": {}});
        }
        let bindings = value["egress"]["bindings"]
            .as_object_mut()
            .expect("bindings 必须是对象");
        if line.is_empty() {
            bindings.remove(&aid);
        } else {
            bindings.insert(aid.clone(), json!(line));
        }
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn security_config(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    Json(state.store.get("security").unwrap_or_else(|| json!({}))).into_response()
}

async fn save_security_config(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    match state.store.update(|value| {
        let security = value["security"]
            .as_object_mut()
            .expect("security 必须是对象");
        if let Some(source) = body.as_object() {
            for (key, item) in source {
                security.insert(key.clone(), item.clone());
            }
        }
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn list_ip_rules(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let value = state.store.get("ip_rules").unwrap_or_else(|| json!({}));
    let rows: Vec<Value> = value
        .as_object()
        .map(|rules| {
            rules
                .iter()
                .map(|(id, row)| {
                    let mut row = row.clone();
                    row["id"] = json!(id);
                    row
                })
                .collect()
        })
        .unwrap_or_default();
    Json(json!(rows)).into_response()
}

async fn create_ip_rule(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let Some(cidr) = clean_name(body.get("cidr")) else {
        return bad_request("缺少 CIDR");
    };
    let action = match body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("block")
    {
        "block" | "allow" => body
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("block"),
        _ => return bad_request("动作只能是 block 或 allow"),
    };
    let id = random_id("ip_");
    let entry = json!({"cidr": cidr, "action": action, "note": body.get("note").cloned().unwrap_or_else(|| json!("")), "created": now()});
    let id_cloned = id.clone();
    match state.store.update(|value| {
        let rules = value["ip_rules"]
            .as_object_mut()
            .expect("ip_rules 必须是对象");
        rules.insert(id_cloned.clone(), entry.clone());
    }) {
        Ok(()) => Json(json!({"ok": true, "id": id})).into_response(),
        Err(error) => write_failed(error),
    }
}

async fn delete_ip_rule(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(rid): axum::extract::Path<String>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    match state.store.update(|value| {
        value["ip_rules"]
            .as_object_mut()
            .map(|rules| rules.remove(&rid));
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn ip_logs(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    Json(
        state
            .store
            .get("ip_access_logs")
            .unwrap_or_else(|| json!([])),
    )
    .into_response()
}

async fn clear_ip_logs(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    match state
        .store
        .update(|value| value["ip_access_logs"] = json!([]))
    {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn request_logs(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    // Rust 引擎暂不落请求日志（Python 落 state.request_logs）；先返回空表，
    // SPA 的日志页会显示「暂无记录」，不会报错。
    Json(state.store.get("request_logs").unwrap_or_else(|| json!([]))).into_response()
}

async fn usage_stats(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let stats = state.store.get("usage_stats").unwrap_or_else(|| json!({}));
    Json(json!({
        "daily": stats.get("daily").cloned().unwrap_or_else(|| json!({})),
        "hourly": stats.get("hourly").cloned().unwrap_or_else(|| json!({})),
        "models": stats.get("models").cloned().unwrap_or_else(|| json!([])),
        "totals": stats.get("totals").cloned().unwrap_or_else(|| json!({"requests": 0, "tokens": 0})),
    })).into_response()
}

async fn list_aliases(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    // Python 返回 { aliases, known_models, default_model, unknown_targets }；
    // 曾返回别名行数组，models-usage-panel 解构 undefined 崩。
    let aliases = state
        .store
        .get("model_aliases")
        .unwrap_or_else(|| json!({}));
    let known: Vec<String> = catalog::load(&state)
        .await
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let unknown: Vec<String> = aliases
        .as_object()
        .map(|map| {
            let mut values: Vec<String> = map
                .values()
                .filter_map(Value::as_str)
                .filter(|target| known.iter().all(|k| k != target))
                .map(str::to_string)
                .collect();
            values.sort();
            values.dedup();
            values
        })
        .unwrap_or_default();
    Json(json!({
        "aliases": aliases,
        "known_models": known,
        "default_model": known.first().cloned().unwrap_or_default(),
        "unknown_targets": unknown,
    }))
    .into_response()
}

async fn save_alias(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let Some(alias) = clean_name(body.get("alias")) else {
        return bad_request("缺少别名");
    };
    let Some(model) = clean_name(body.get("model")) else {
        return bad_request("缺少目标模型");
    };
    match state.store.update(|value| {
        let aliases = value["model_aliases"]
            .as_object_mut()
            .expect("model_aliases 必须是对象");
        aliases.insert(alias.clone(), json!(model));
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn patch_account(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(aid): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let Some(record) = account_record(&state, &aid) else {
        return not_found("账号不存在");
    };
    let uid = record
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or(&aid)
        .to_string();
    match state.store.update(|value| {
        if !value["account_overrides"].is_object() {
            value["account_overrides"] = json!({});
        }
        let overrides = value["account_overrides"]
            .as_object_mut()
            .expect("overrides 必须是对象");
        let entry = overrides.entry(uid.clone()).or_insert_with(|| json!({}));
        for key in ["name", "enabled"] {
            if let Some(item) = body.get(key) {
                entry[key] = item.clone();
            }
        }
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn delete_account(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(aid): axum::extract::Path<String>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let Some(record) = account_record(&state, &aid) else {
        return not_found("账号不存在");
    };
    let uid = record
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or(&aid)
        .to_string();
    match state.store.update(|value| {
        let overrides = value["account_overrides"]
            .as_object_mut()
            .expect("overrides 必须是对象");
        let entry = overrides.entry(uid.clone()).or_insert_with(|| json!({}));
        entry["hidden"] = json!(true);
    }) {
        Ok(()) => Json(json!({"ok": true, "scope": "service"})).into_response(),
        Err(error) => write_failed(error),
    }
}

async fn account_limits(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    axum::extract::Path(aid): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let Some(record) = account_record(&state, &aid) else {
        return not_found("账号不存在");
    };
    let uid = record
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or(&aid)
        .to_string();
    match state.store.update(|value| {
        if !value["account_limits"].is_object() {
            value["account_limits"] = json!({});
        }
        value["account_limits"][&uid] = body.clone();
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn create_account_stub() -> Response {
    still_python("新增账号（需要凭据导入与扫码）")
}

async fn credits_snapshot(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let status = state
        .store
        .get("account_status")
        .unwrap_or_else(|| json!({}));
    Json(json!({"accounts": status})).into_response()
}

async fn export_key(axum::extract::Path(_id): axum::extract::Path<String>) -> Response {
    still_python("密钥导出（含加密格式）")
}

async fn save_settings(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    match state.store.update(|value| {
        let settings = value["settings"]
            .as_object_mut()
            .expect("settings 必须是对象");
        if let Some(source) = body.as_object() {
            for (key, item) in source {
                settings.insert(key.clone(), item.clone());
            }
        }
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn alerts(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let value = state
        .store
        .get("alerts")
        .unwrap_or_else(|| json!({"enabled": false, "password_set": false}));
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        object.remove("password");
        object.insert(
            "password_set".to_string(),
            json!(object.get("password_hash").is_some()),
        );
    }
    Json(value).into_response()
}

async fn save_alerts(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    match state.store.update(|value| {
        let alerts = value["alerts"].as_object_mut().expect("alerts 必须是对象");
        if let Some(source) = body.as_object() {
            for (key, item) in source {
                // 口令单独存，永不回显；客户端传回掩码时不覆盖真口令。
                if key == "password" {
                    let text = item.as_str().unwrap_or("");
                    if !text.is_empty() && text != "********" {
                        alerts.insert("password".to_string(), item.clone());
                        alerts.insert("password_hash".to_string(), json!(hash_token(text)));
                    }
                    continue;
                }
                alerts.insert(key.clone(), item.clone());
            }
        }
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn alerts_test() -> Response {
    still_python("告警测试邮件（SMTP 发送）")
}

fn cli_key_of(state: &AppState) -> Option<(String, Value)> {
    let keys = state.store.get("keys").unwrap_or_else(|| json!({}));
    keys.as_object().and_then(|keys| {
        keys.iter()
            .find(|(_, row)| row.get("purpose").and_then(Value::as_str) == Some("cli"))
            .map(|(id, row)| (id.clone(), row.clone()))
    })
}

async fn cli_key_get(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    match cli_key_of(&state) {
        Some((id, row)) => Json(json!({
            "id": id,
            "key": row.get("secret").cloned().unwrap_or_else(|| json!("")),
            "realm": "cn",
            "created": false,
        })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": "还没有 CLI 专用密钥：用 POST /admin/api/cli/key/rotate 生成一枚"})),
        )
            .into_response(),
    }
}

async fn cli_key_rotate(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let token = new_token();
    let kid = random_id("key_cli_");
    let entry = json!({
        "name": "CodeBuddy CLI", "hash": hash_token(&token), "secret": token,
        "hint": format!("{}…{}", &token[..5], &token[token.len() - 4..]),
        "enabled": true, "expires_at": Value::Null, "max_ips": 0, "ip_allowlist": [],
        "models": [], "realm": "cn", "groups": [], "quota": 0, "used_tokens": 0,
        "quota_credit": 0, "used_credit": 0.0, "created": now(), "last_used_at": Value::Null,
        "created_by": "rust-admin", "purpose": "cli",
    });
    let kid_cloned = kid.clone();
    match state.store.update(|value| {
        let keys = value["keys"].as_object_mut().expect("keys 必须是对象");
        keys.retain(|_, row| row.get("purpose").and_then(Value::as_str) != Some("cli"));
        keys.insert(kid_cloned.clone(), entry.clone());
    }) {
        Ok(()) => {
            Json(json!({"id": kid, "key": token, "realm": "cn", "created": true})).into_response()
        }
        Err(error) => write_failed(error),
    }
}

async fn cli_key_revoke(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let mut last = false;
    let mut revoked = 0;
    let result = state.store.update(|value| {
        let Some(keys) = value["keys"].as_object_mut() else {
            return;
        };
        let cli_ids: Vec<String> = keys
            .iter()
            .filter(|(_, row)| row.get("purpose").and_then(Value::as_str) == Some("cli"))
            .map(|(id, _)| id.clone())
            .collect();
        if keys.len() == cli_ids.len() && !cli_ids.is_empty() {
            last = true;
            return;
        }
        for id in cli_ids {
            keys.remove(&id);
            revoked += 1;
        }
    });
    match result {
        Err(error) => write_failed(error),
        Ok(()) if last => bad_request("CLI 密钥是当前唯一密钥，请先创建其它密钥"),
        Ok(()) => Json(json!({"ok": true, "revoked": revoked})).into_response(),
    }
}

async fn oauth_stub() -> Response {
    still_python("扫码登录")
}

/// `POST /admin/api/test`：连接测试。
///
/// 与 Python 版同一动作：用一条极短的 chat 请求验证「凭据、账号额度、模型权限」
/// 这条链路是否通。**会真实消耗额度**，所以前端必须先二次确认。
///
/// 实现上刻意走与数据面完全相同的路径（`chat::handle_chat`），而不是另写一份
/// 上游调用：测试通过但真实请求失败，是这个端点最没价值的失败方式。
///
/// 与 Python 的差异（记录备查）：
/// - 不创建临时密钥：`handle_chat` 直接收 `key = None`，效果等同「不受策略限制」；
/// - 不做「同一时刻只跑一个测试」的互斥：桌面端是单用户，重复点击由前端禁用按钮保证。
async fn chat_test(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    connect: Option<ConnectInfo<std::net::SocketAddr>>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    let peer = peer_of(connect);
    if let Err(response) = authorize(&state, &headers, peer) {
        return response;
    }
    let models: Vec<String> = catalog::load(&state)
        .await
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    // 默认模型：清单里第一个。与 Python 的 resolve_default_model 口径一致（同样来自目录）。
    let model = match body.get("model").and_then(Value::as_str) {
        Some(model) => model.to_string(),
        None => match models.first() {
            Some(model) => model.clone(),
            None => return bad_request("模型目录为空，请先确认账号已登录"),
        },
    };
    if !models.iter().any(|item| item == &model) {
        return bad_request("请选择列表中的模型");
    }
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("请只回复：连接成功");
    if prompt.trim().is_empty() || prompt.chars().count() > 2000 {
        return bad_request("测试消息需为 1–2000 个字符");
    }
    if accounts::list_at(&state.cfg.accounts_path).is_empty() {
        return bad_request("请先导入并启用一个账号");
    }

    let started = std::time::Instant::now();
    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 1024,
        "stream": false,
    });
    let upstream_headers = HeaderMap::new();
    // 连接测试也计入指标（source = test）：Python 侧 MetricsMiddleware 同样统计 test。
    state.metrics.begin();
    let response = crate::chat::handle_chat(
        state.clone(),
        incoming_headers(&upstream_headers),
        payload,
        None,
    )
    .await;
    let seconds = (started.elapsed().as_millis() as f64) / 1000.0;

    let status = response.status();
    let bytes = match axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return Json(json!({
                "ok": false,
                "error": "读取上游响应失败，请稍后重试。",
                "seconds": round2(seconds),
            }))
            .into_response()
        }
    };
    let data: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let answer = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if status.is_success() && !answer.is_empty() {
        // 连接测试也计入指标（source = test）：Python 的 MetricsMiddleware 同样统计。
        state.metrics.finish(
            "/admin/api/test",
            "test",
            status.as_u16(),
            true,
            started.elapsed().as_millis() as u64,
            "success",
        );
        return Json(json!({
            "ok": true,
            "status": status.as_u16(),
            "answer": answer,
            "seconds": round2(seconds),
            "usage": data.get("usage").cloned().unwrap_or(Value::Null),
        }))
        .into_response();
    }

    state.metrics.finish(
        "/admin/api/test",
        "test",
        status.as_u16(),
        false,
        started.elapsed().as_millis() as u64,
        "http_error",
    );

    // 上游的失败信息比一句笼统的「失败」有用得多（额度、模型权限都在里面）。
    let upstream_message = data
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let detail = if status.is_success() {
        "上游未返回有效回复，请检查登录凭据、账号额度或模型权限。".to_string()
    } else if upstream_message.is_empty() {
        format!(
            "调用失败（HTTP {}），请检查凭据是否有效，稍后重试。",
            status.as_u16()
        )
    } else {
        format!("调用失败（HTTP {}）：{upstream_message}", status.as_u16())
    };
    Json(json!({
        "ok": false,
        "status": status.as_u16(),
        "error": detail,
        "seconds": round2(seconds),
    }))
    .into_response()
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// 连接测试里构造「空入参」的客户端头：不伪造 UA，让上游看到的和真实链路一致。
fn incoming_headers(headers: &HeaderMap) -> std::collections::BTreeMap<String, String> {
    crate::profile::lower_headers(
        headers
            .iter()
            .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str(), value))),
    )
}
