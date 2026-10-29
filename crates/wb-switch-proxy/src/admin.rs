//! 管理面（P5 第一阶段）：登录会话 + 只读概览 + 密钥列表。
//!
//! 目标是与 Python 的 \`/admin/api/*\` 同契约，但分阶段落地：本阶段只做
//! 「打开桌面端就能看到账号/密钥/模型/设置」所需的最小集合，写操作与
//! 76 条里的其余端点仍由 Python 服务提供（灰度期两个引擎并存）。
//!
//! 鉴权：ADMIN_KEY（≥20 字符）或管理令牌登录，会话存内存；cookie 与
//! \`X-Admin-Token\` 都支持。未配置 ADMIN_KEY 时管理面整体关闭（403），
//! 避免「默认没有口令」把管理接口裸奔。

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Json as JsonBody, Router};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::accounts;
use crate::catalog;
use crate::http::AppState;

const SESSION_TTL_SECONDS: u64 = 12 * 3600;
const COOKIE_NAME: &str = "workbuddy_admin";

struct Session {
    username: String,
    role: &'static str,
    csrf: String,
    expires: u64,
}

pub struct AdminState {
    key: Option<String>,
    sessions: Mutex<HashMap<String, Session>>,
}

impl AdminState {
    pub fn from_env() -> Self {
        let key = std::env::var("ADMIN_KEY")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| value.len() >= 20);
        Self { key, sessions: Mutex::new(HashMap::new()) }
    }

    pub fn with_key(key: &str) -> Self {
        Self { key: Some(key.to_string()), sessions: Mutex::new(HashMap::new()) }
    }

    fn verify_key(&self, candidate: &str) -> bool {
        match &self.key {
            Some(key) => key == candidate,
            None => false,
        }
    }
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
    let hex: String = digest.iter().take(16).map(|byte| format!("{byte:02x}")).collect();
    format!("{prefix}{hex}")
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"detail": message, "error": {"message": message, "type": "authentication_error"}})),
    )
        .into_response()
}

fn forbidden(message: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({"detail": message}))).into_response()
}

/// 从 cookie 或 \`X-Admin-Token\` 取会话。
fn session_of(state: &AdminState, headers: &HeaderMap) -> Option<(String, String, &'static str)> {
    let mut token = headers
        .get("x-admin-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if token.is_empty() {
        if let Some(cookie) = headers.get("cookie").and_then(|value| value.to_str().ok()) {
            for part in cookie.split(';') {
                let part = part.trim();
                if let Some(value) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
                    token = value.to_string();
                    break;
                }
            }
        }
    }
    if token.is_empty() {
        return None;
    }
    let mut sessions = state.sessions.lock().unwrap();
    sessions.retain(|_, session| session.expires > now());
    let session = sessions.get(&token)?;
    Some((token, session.csrf.clone(), session.role))
}

fn require_admin(state: &AdminState, headers: &HeaderMap) -> Result<(String, String, &'static str), Response> {
    session_of(state, headers).ok_or_else(|| unauthorized("请先登录管理后台"))
}

pub fn router() -> Router<std::sync::Arc<AppState>> {
    Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/session", get(session_info))
        .route("/me", get(session_info))
        .route("/sessions/revoke", post(revoke_sessions))
        .route("/login-options", get(login_options))
        .route("/overview", get(overview))
        .route("/pool/summary", get(pool_summary))
        .route("/keys", get(list_keys).post(create_key))
        .route("/keys/check-models", post(check_models))
        .route("/keys/:id", axum::routing::patch(update_key).delete(delete_key))
        .route("/keys/:id/reset-usage", post(reset_usage))
        .route("/models", get(models))
        .route("/audit-logs", get(audit_logs))
        .route("/pool/settings", axum::routing::patch(pool_settings))
        .route("/accounts/:aid/actions/:action", post(account_action))
        .route("/pool/actions/:action", post(pool_action))
        .route("/pool/checkin-logs", get(checkin_logs))
        .route("/pool/checkin-logs/summary", get(checkin_logs_summary))
        .route("/pool/checkin-logs/clear", post(clear_checkin_logs))
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/:gid", axum::routing::patch(update_group).delete(delete_group))
        .route("/egress", get(egress_view))
        .route("/egress/lines", post(create_egress_line))
        .route("/egress/lines/:lid", axum::routing::patch(update_egress_line).delete(delete_egress_line))
        .route("/egress/assign", post(assign_egress))
        .route("/users", get(list_users).post(create_user))
        .route("/users/:username", axum::routing::patch(update_user).delete(delete_user))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/:tid", axum::routing::patch(update_token).delete(delete_token))
        .route("/security/config", get(security_config).post(save_security_config))
        .route("/security/rules", get(list_ip_rules).post(create_ip_rule))
        .route("/security/rules/:rid", axum::routing::delete(delete_ip_rule))
        .route("/security/logs", get(ip_logs))
        .route("/security/logs/clear", post(clear_ip_logs))
        .route("/audit-logs/clear", post(clear_audit_logs))
        .route("/request-logs", get(request_logs))
        .route("/usage/stats", get(usage_stats))
        .route("/model-aliases", get(list_aliases).post(save_alias))
        .route("/accounts/:aid", axum::routing::patch(patch_account).delete(delete_account))
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
        .route("/test", post(chat_test_stub))
}

async fn login_options() -> Json<Value> {
    // 与 Python 同名字段：桌面端托管时 legacy_key 往往为真。
    Json(json!({"legacy_key": true, "tokens": false}))
}

async fn login(State(state): State<std::sync::Arc<AppState>>, JsonBody(body): JsonBody<Value>) -> Response {
    let admin_key = match &state.admin.key {
        Some(_) => state.admin.key.clone().unwrap_or_default(),
        None => return forbidden("未配置 ADMIN_KEY：Rust 管理面未启用"),
    };
    let candidate = body.get("key").and_then(Value::as_str).unwrap_or("");
    if !admin_key.is_empty() && !state.admin.verify_key(candidate) {
        return unauthorized("管理密钥不正确");
    }
    let token = random_id("adm_");
    let csrf = random_id("csrf_");
    {
        let mut sessions = state.admin.sessions.lock().unwrap();
        sessions.insert(token.clone(), Session {
            username: "admin".to_string(),
            role: "admin",
            csrf: csrf.clone(),
            expires: now() + SESSION_TTL_SECONDS,
        });
    }
    let cookie = format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_TTL_SECONDS}");
    let mut response = Json(json!({"csrf": csrf, "token": token, "username": "admin", "role": "admin"})).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append("set-cookie", value);
    }
    response
}

async fn logout(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some((token, _, _)) = session_of(&state.admin, &headers) {
        state.admin.sessions.lock().unwrap().remove(&token);
    }
    Json(json!({"ok": true})).into_response()
}

async fn session_info(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    match require_admin(&state.admin, &headers) {
        Ok((_, csrf, role)) => Json(json!({
            "csrf": csrf, "username": "admin", "role": role, "via": "session",
            "version": env!("CARGO_PKG_VERSION"), "dev": cfg!(debug_assertions),
        })).into_response(),
        Err(response) => response,
    }
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

async fn overview(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    let (_, _, role) = match require_admin(&state.admin, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let reveal = role == "admin";
    let rows = state.pool.rows(&accounts::list_at(&state.cfg.accounts_path));
    let keys = keys_of(&state, reveal).await;
    let models: Vec<String> = catalog::load(&state).await
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    Json(json!({
        "accounts": rows,
        "pool": state.store.get("pool").unwrap_or_else(|| json!({})),
        "metrics": {},
        "keys": keys,
        "models": models,
        "default_model": models.first().cloned().unwrap_or_default(),
        "checkin_logs": [],
        "request_logs": [],
        "groups": state.store.get("groups").unwrap_or_else(|| json!({})),
        "uptime": state.started_at.elapsed().as_secs(),
        "runtime": {"session": state.started_at.elapsed().as_secs(), "total": state.started_at.elapsed().as_secs()},
        "events": [],
    })).into_response()
}

async fn pool_summary(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let accounts = state.pool.rows(&accounts::list_at(&state.cfg.accounts_path));
    Json(json!({
        "accounts": accounts,
        "pool": state.store.get("pool").unwrap_or_else(|| json!({})),
    })).into_response()
}

async fn list_keys(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    let (_, _, role) = match require_admin(&state.admin, &headers) {
        Ok(value) => value,
        Err(response) => return response,
    };
    Json(json!(keys_of(&state, role == "admin").await)).into_response()
}

async fn models(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    Json(json!(catalog::load(&state).await)).into_response()
}

async fn settings(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    Json(state.store.get("settings").unwrap_or_else(|| json!({}))).into_response()
}

async fn audit_logs(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let rows = state.store.get("audit_logs").unwrap_or_else(|| json!([]));
    Json(rows).into_response()
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
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
    (status, Json(json!({"detail": format!("状态写入失败：{error}")}))).into_response()
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
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(name) = clean_name(body.get("name")) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "密钥名称不能只有空格"}))).into_response();
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
        "max_ips": 0,
        "ip_allowlist": [],
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
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "最多保存 100 枚 API Key"}))).into_response();
    }
    Json(json!({"id": kid, "key": token})).into_response()
}

async fn update_key(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let mut missing = false;
    let result = state.store.update(|value| {
        let Some(keys) = value["keys"].as_object_mut() else { missing = true; return };
        let Some(item) = keys.get_mut(&id) else { missing = true; return };
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
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let mut missing = false;
    let mut last = false;
    let result = state.store.update(|value| {
        let Some(keys) = value["keys"].as_object_mut() else { missing = true; return };
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
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "请先创建一枚新密钥，再撤销最后一枚密钥"}))).into_response();
    }
    Json(json!({"ok": true})).into_response()
}

async fn reset_usage(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let mut missing = false;
    let result = state.store.update(|value| {
        let Some(keys) = value["keys"].as_object_mut() else { missing = true; return };
        let Some(item) = keys.get_mut(&id) else { missing = true; return };
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
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
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
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let unknown: Vec<&String> = requested.iter().filter(|model| !known.contains(model)).collect();
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

fn plain_token(record: &Value, key: &str) -> Option<String> {
    match record.get(key) {
        Some(Value::String(token)) if !token.trim().is_empty() => Some(token.trim().to_string()),
        _ => None,
    }
}

async fn pool_settings(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    if let Some(routing) = body.get("routing").and_then(Value::as_str) {
        if !matches!(routing, "round_robin" | "manual" | "least_used") {
            return (StatusCode::BAD_REQUEST, Json(json!({"detail": "未知的调度方式"}))).into_response();
        }
    }
    let allowed = [
        "routing", "auto_checkin", "checkin_time", "auto_travel", "travel_time",
        "auto_keepalive", "keepalive_hours", "auto_active", "active_time", "active_reports",
        "spread", "spread_minutes", "default_weight", "default_max_concurrency",
        "default_min_interval", "max_total_concurrency", "retry_enabled",
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
    axum::extract::Path((aid, action)): axum::extract::Path<(String, String)>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(record) = account_record(&state, &aid) else {
        return (StatusCode::NOT_FOUND, Json(json!({"detail": "账号不存在"}))).into_response();
    };
    let Some(token) = plain_token(&record, "access_token") else {
        return (StatusCode::NOT_IMPLEMENTED, Json(json!({"detail": "该账号 token 是加密信封，Rust 引擎暂不支持（P4）"}))).into_response();
    };
    let result = match action.as_str() {
        "status" => crate::billing::credits(&state.cfg, &token).await.map(|value| json!({"ok": true, "id": aid, "message": "积分状态已更新", "remaining": value["remaining"]})),
        "checkin" => crate::billing::checkin(&state.cfg, &token).await.map(|value| json!({"ok": true, "id": aid, "message": value["status"], "result": value})),
        "refresh" => match plain_token(&record, "refresh_token") {
            Some(refresh) => crate::billing::refresh_token(&state.cfg, &refresh).await.map(|value| json!({"ok": true, "id": aid, "message": "登录凭据已刷新", "result": value})),
            None => Err("账号没有 refresh_token".to_string()),
        },
        other => {
            return (StatusCode::NOT_IMPLEMENTED, Json(json!({"detail": format!("动作 {other} 尚未迁移")}))).into_response();
        }
    };
    match result {
        Ok(payload) => Json(payload).into_response(),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({"detail": error}))).into_response(),
    }
}

async fn pool_action(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(action): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    if action != "status" {
        return (StatusCode::NOT_IMPLEMENTED, Json(json!({"detail": format!("批量动作 {action} 尚未迁移")}))).into_response();
    }
    let mut results = Vec::new();
    for row in accounts::list_at(&state.cfg.accounts_path) {
        let Some(record) = account_record(&state, &row.id) else { continue };
        let Some(token) = plain_token(&record, "access_token") else { continue };
        match crate::billing::credits(&state.cfg, &token).await {
            Ok(value) => results.push(json!({"id": row.id, "ok": true, "message": "积分状态已更新", "remaining": value["remaining"]})),
            Err(error) => results.push(json!({"id": row.id, "ok": false, "message": error})),
        }
    }
    Json(json!({"results": results})).into_response()
}

// ── 其余管理端点（P5 收口）────────────────────────────────────────────────

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn pbkdf2_hash(password: &str) -> String {
    // 不引入 rand：用时间 + pid + 计数器派生 16 字节盐，足够避免同口令同哈希。
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tick = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let mut seed = Sha256::new();
    seed.update(format!("{nanos}:{}:{tick}", std::process::id()).as_bytes());
    let digest = seed.finalize();
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&digest[..16]);
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, 260_000, &mut out);
    ["pbkdf2_sha256$260000$", &hex_of(&salt), "$", &hex_of(&out)].concat()
}

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

async fn checkin_logs(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    Json(state.store.get("checkin_logs").unwrap_or_else(|| json!([]))).into_response()
}

async fn checkin_logs_summary(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let logs = state.store.get("checkin_logs").unwrap_or_else(|| json!([]));
    let mut total = 0;
    let mut failed = 0;
    if let Some(rows) = logs.as_array() {
        total = rows.len();
        failed = rows.iter().filter(|row| row.get("ok").and_then(Value::as_bool) == Some(false)).count();
    }
    Json(json!({"total": total, "failed": failed, "ok": total - failed})).into_response()
}

async fn clear_checkin_logs(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    match state.store.update(|value| value["checkin_logs"] = json!([])) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn list_groups(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let value = state.store.get("groups").unwrap_or_else(|| json!({}));
    let rows: Vec<Value> = value
        .as_object()
        .map(|groups| groups.iter().map(|(id, row)| {
            let mut row = row.clone();
            row["id"] = json!(id);
            row
        }).collect())
        .unwrap_or_default();
    Json(json!(rows)).into_response()
}

async fn create_group(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(name) = clean_name(body.get("name")) else { return bad_request("分组名称不能为空") };
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
    axum::extract::Path(gid): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let mut missing = false;
    let result = state.store.update(|value| {
        let Some(groups) = value["groups"].as_object_mut() else { missing = true; return };
        let Some(group) = groups.get_mut(&gid) else { missing = true; return };
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
    axum::extract::Path(gid): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    match state.store.update(|value| {
        value["groups"].as_object_mut().map(|groups| groups.remove(&gid));
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn egress_view(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let egress = state.store.get("egress").unwrap_or_else(|| json!({"lines": {}, "bindings": {}}));
    let lines: Vec<Value> = egress
        .get("lines")
        .and_then(Value::as_object)
        .map(|lines| lines.iter().map(|(id, row)| {
            let mut row = row.clone();
            row["id"] = json!(id);
            row
        }).collect())
        .unwrap_or_default();
    Json(json!({"lines": lines, "bindings": egress.get("bindings").cloned().unwrap_or_else(|| json!({}))})).into_response()
}

async fn create_egress_line(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(name) = clean_name(body.get("name")) else { return bad_request("线路名称不能为空") };
    let Some(url) = body.get("url").and_then(Value::as_str).filter(|value| value.starts_with("http")) else {
        return bad_request("线路地址必须是 http(s) URL");
    };
    let id = random_id("line_");
    let entry = json!({"name": name, "url": url, "enabled": true, "created": now()});
    let id_cloned = id.clone();
    match state.store.update(|value| {
        let lines = value["egress"]["lines"].as_object_mut().expect("egress.lines 必须是对象");
        lines.insert(id_cloned.clone(), entry.clone());
    }) {
        Ok(()) => Json(json!({"ok": true, "id": id})).into_response(),
        Err(error) => write_failed(error),
    }
}

async fn update_egress_line(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(lid): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let mut missing = false;
    let result = state.store.update(|value| {
        let Some(lines) = value["egress"]["lines"].as_object_mut() else { missing = true; return };
        let Some(line) = lines.get_mut(&lid) else { missing = true; return };
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
    axum::extract::Path(lid): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    match state.store.update(|value| {
        value["egress"]["lines"].as_object_mut().map(|lines| lines.remove(&lid));
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
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
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
        let bindings = value["egress"]["bindings"].as_object_mut().expect("bindings 必须是对象");
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

async fn list_users(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let value = state.store.get("users").unwrap_or_else(|| json!({}));
    let rows: Vec<Value> = value
        .as_object()
        .map(|users| users.iter().map(|(username, row)| json!({
            "username": username,
            "role": row.get("role").cloned().unwrap_or_else(|| json!("admin")),
            "password_set": row.get("pwd_hash").is_some(),
        })).collect())
        .unwrap_or_default();
    Json(json!(rows)).into_response()
}

async fn create_user(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(username) = clean_name(body.get("username")) else { return bad_request("用户名不能为空") };
    let password = body.get("password").and_then(Value::as_str).unwrap_or("");
    if password.chars().count() < 8 {
        return bad_request("口令至少 8 位");
    }
    let role = match body.get("role").and_then(Value::as_str).unwrap_or("admin") {
        "admin" => "admin",
        "readonly" => "readonly",
        _ => return bad_request("角色只能是 admin 或 readonly"),
    };
    let hash = pbkdf2_hash(password);
    let username_cloned = username.clone();
    match state.store.update(|value| {
        let users = value["users"].as_object_mut().expect("users 必须是对象");
        users.insert(username_cloned.clone(), json!({
            "username": username_cloned, "role": role, "pwd_hash": hash, "sv": 0,
        }));
    }) {
        Ok(()) => Json(json!({"ok": true, "username": username})).into_response(),
        Err(error) => write_failed(error),
    }
}

async fn update_user(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(username): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let password = body.get("password").and_then(Value::as_str).map(pbkdf2_hash);
    let mut missing = false;
    let result = state.store.update(|value| {
        let Some(users) = value["users"].as_object_mut() else { missing = true; return };
        let Some(user) = users.get_mut(&username) else { missing = true; return };
        if let Some(role) = body.get("role").and_then(Value::as_str) {
            if matches!(role, "admin" | "readonly") {
                user["role"] = json!(role);
            }
        }
        if let Some(hash) = &password {
            user["pwd_hash"] = json!(hash);
        }
        user["sv"] = json!(user.get("sv").and_then(Value::as_i64).unwrap_or(0) + 1);
    });
    match result {
        Err(error) => write_failed(error),
        Ok(()) if missing => not_found("用户不存在"),
        Ok(()) => ok(),
    }
}

async fn delete_user(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let mut last = false;
    let result = state.store.update(|value| {
        let Some(users) = value["users"].as_object_mut() else { return };
        if users.len() <= 1 && users.contains_key(&username) {
            last = true;
            return;
        }
        users.remove(&username);
    });
    match result {
        Err(error) => write_failed(error),
        Ok(()) if last => bad_request("不能删除最后一个后台账号"),
        Ok(()) => ok(),
    }
}

async fn list_tokens(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let value = state.store.get("api_tokens").unwrap_or_else(|| json!({}));
    let rows: Vec<Value> = value
        .as_object()
        .map(|tokens| tokens.iter().map(|(id, row)| json!({
            "id": id,
            "name": row.get("name").cloned().unwrap_or_else(|| json!("")),
            "scope": row.get("scope").cloned().unwrap_or_else(|| json!("readonly")),
            "enabled": row.get("enabled").cloned().unwrap_or_else(|| json!(true)),
            "expires_at": row.get("expires_at").cloned().unwrap_or(Value::Null),
            "created": row.get("created").cloned().unwrap_or(Value::Null),
        })).collect())
        .unwrap_or_default();
    Json(json!(rows)).into_response()
}

async fn create_token(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(name) = clean_name(body.get("name")) else { return bad_request("令牌名称不能为空") };
    let scope = match body.get("scope").and_then(Value::as_str).unwrap_or("readonly") {
        "admin" => "admin",
        "readonly" => "readonly",
        _ => return bad_request("作用域只能是 admin 或 readonly"),
    };
    let raw = format!("wbt_{}", random_id(""));
    let id = random_id("tok_");
    let entry = json!({
        "name": name, "hash": hash_token(&raw), "hint": format!("…{}", &raw[raw.len() - 4..]),
        "scope": scope, "enabled": true,
        "expires_at": body.get("expires_at").cloned().unwrap_or(Value::Null),
        "created": now(),
    });
    let id_cloned = id.clone();
    match state.store.update(|value| {
        let tokens = value["api_tokens"].as_object_mut().expect("api_tokens 必须是对象");
        tokens.insert(id_cloned.clone(), entry.clone());
    }) {
        Ok(()) => Json(json!({"ok": true, "id": id, "token": raw})).into_response(),
        Err(error) => write_failed(error),
    }
}

async fn update_token(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(tid): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let mut missing = false;
    let result = state.store.update(|value| {
        let Some(tokens) = value["api_tokens"].as_object_mut() else { missing = true; return };
        let Some(token) = tokens.get_mut(&tid) else { missing = true; return };
        for key in ["name", "scope", "enabled", "expires_at"] {
            if let Some(item) = body.get(key) {
                token[key] = item.clone();
            }
        }
    });
    match result {
        Err(error) => write_failed(error),
        Ok(()) if missing => not_found("令牌不存在"),
        Ok(()) => ok(),
    }
}

async fn delete_token(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(tid): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    match state.store.update(|value| {
        value["api_tokens"].as_object_mut().map(|tokens| tokens.remove(&tid));
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn security_config(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    Json(state.store.get("security").unwrap_or_else(|| json!({}))).into_response()
}

async fn save_security_config(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    match state.store.update(|value| {
        let security = value["security"].as_object_mut().expect("security 必须是对象");
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

async fn list_ip_rules(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let value = state.store.get("ip_rules").unwrap_or_else(|| json!({}));
    let rows: Vec<Value> = value
        .as_object()
        .map(|rules| rules.iter().map(|(id, row)| {
            let mut row = row.clone();
            row["id"] = json!(id);
            row
        }).collect())
        .unwrap_or_default();
    Json(json!(rows)).into_response()
}

async fn create_ip_rule(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(cidr) = clean_name(body.get("cidr")) else { return bad_request("缺少 CIDR") };
    let action = match body.get("action").and_then(Value::as_str).unwrap_or("block") {
        "block" | "allow" => body.get("action").and_then(Value::as_str).unwrap_or("block"),
        _ => return bad_request("动作只能是 block 或 allow"),
    };
    let id = random_id("ip_");
    let entry = json!({"cidr": cidr, "action": action, "note": body.get("note").cloned().unwrap_or_else(|| json!("")), "created": now()});
    let id_cloned = id.clone();
    match state.store.update(|value| {
        let rules = value["ip_rules"].as_object_mut().expect("ip_rules 必须是对象");
        rules.insert(id_cloned.clone(), entry.clone());
    }) {
        Ok(()) => Json(json!({"ok": true, "id": id})).into_response(),
        Err(error) => write_failed(error),
    }
}

async fn delete_ip_rule(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(rid): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    match state.store.update(|value| {
        value["ip_rules"].as_object_mut().map(|rules| rules.remove(&rid));
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn ip_logs(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    Json(state.store.get("ip_access_logs").unwrap_or_else(|| json!([]))).into_response()
}

async fn clear_ip_logs(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    match state.store.update(|value| value["ip_access_logs"] = json!([])) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn clear_audit_logs(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    match state.store.update(|value| value["audit_logs"] = json!([])) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn request_logs(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    // Rust 引擎暂不落请求日志（Python 落 state.request_logs）；先返回空表，
    // SPA 的日志页会显示「暂无记录」，不会报错。
    Json(state.store.get("request_logs").unwrap_or_else(|| json!([]))).into_response()
}

async fn usage_stats(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
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

async fn list_aliases(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let value = state.store.get("model_aliases").unwrap_or_else(|| json!({}));
    let rows: Vec<Value> = value
        .as_object()
        .map(|aliases| aliases.iter().map(|(alias, model)| json!({"alias": alias, "model": model})).collect())
        .unwrap_or_default();
    Json(json!(rows)).into_response()
}

async fn save_alias(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(alias) = clean_name(body.get("alias")) else { return bad_request("缺少别名") };
    let Some(model) = clean_name(body.get("model")) else { return bad_request("缺少目标模型") };
    match state.store.update(|value| {
        let aliases = value["model_aliases"].as_object_mut().expect("model_aliases 必须是对象");
        aliases.insert(alias.clone(), json!(model));
    }) {
        Ok(()) => ok(),
        Err(error) => write_failed(error),
    }
}

async fn patch_account(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Path(aid): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(record) = account_record(&state, &aid) else { return not_found("账号不存在") };
    let uid = record.get("uid").and_then(Value::as_str).unwrap_or(&aid).to_string();
    match state.store.update(|value| {
        if !value["account_overrides"].is_object() {
            value["account_overrides"] = json!({});
        }
        let overrides = value["account_overrides"].as_object_mut().expect("overrides 必须是对象");
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
    axum::extract::Path(aid): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(record) = account_record(&state, &aid) else { return not_found("账号不存在") };
    let uid = record.get("uid").and_then(Value::as_str).unwrap_or(&aid).to_string();
    match state.store.update(|value| {
        let overrides = value["account_overrides"].as_object_mut().expect("overrides 必须是对象");
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
    axum::extract::Path(aid): axum::extract::Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let Some(record) = account_record(&state, &aid) else { return not_found("账号不存在") };
    let uid = record.get("uid").and_then(Value::as_str).unwrap_or(&aid).to_string();
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

async fn credits_snapshot(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let status = state.store.get("account_status").unwrap_or_else(|| json!({}));
    Json(json!({"accounts": status})).into_response()
}

async fn export_key(axum::extract::Path(_id): axum::extract::Path<String>) -> Response {
    still_python("密钥导出（含加密格式）")
}

async fn save_settings(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    match state.store.update(|value| {
        let settings = value["settings"].as_object_mut().expect("settings 必须是对象");
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

async fn alerts(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let value = state.store.get("alerts").unwrap_or_else(|| json!({"enabled": false, "password_set": false}));
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        object.remove("password");
        object.insert("password_set".to_string(), json!(object.get("password_hash").is_some()));
    }
    Json(value).into_response()
}

async fn save_alerts(
    State(state): State<std::sync::Arc<AppState>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Value>,
) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
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

async fn cli_key_get(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
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

async fn cli_key_rotate(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
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
        Ok(()) => Json(json!({"id": kid, "key": token, "realm": "cn", "created": true})).into_response(),
        Err(error) => write_failed(error),
    }
}

async fn cli_key_revoke(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_admin(&state.admin, &headers) {
        return response;
    }
    let mut last = false;
    let mut revoked = 0;
    let result = state.store.update(|value| {
        let Some(keys) = value["keys"].as_object_mut() else { return };
        let cli_ids: Vec<String> = keys.iter()
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

async fn chat_test_stub() -> Response {
    still_python("连接测试（会真实调用上游模型）")
}

async fn revoke_sessions(State(state): State<std::sync::Arc<AppState>>, headers: HeaderMap) -> Response {
    let Ok((keep, _, _)) = require_admin(&state.admin, &headers) else {
        return unauthorized("请先登录管理后台");
    };
    let mut sessions = state.admin.sessions.lock().unwrap();
    let before = sessions.len();
    sessions.retain(|token, _| token == &keep);
    Json(json!({"ok": true, "revoked": before.saturating_sub(1)})).into_response()
}
