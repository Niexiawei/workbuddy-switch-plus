//! 管理面 P5 第一阶段：登录会话 + 只读概览。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use wb_switch_proxy::admin::AdminState;
use wb_switch_proxy::config::ProxyConfig;
use wb_switch_proxy::http::{router, AppState};
use wb_switch_proxy::state::StateStore;

const KEY: &str = "test-admin-key-0123456789";

fn app() -> (tempfile::TempDir, axum::Router) {
    app_with_billing("http://127.0.0.1:1")
}

fn app_with_billing(billing_base: &str) -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("accounts.json"),
        r#"[{"id":"a1","uid":"u1","nickname":"测试号","variant":"cn","domain":"www.codebuddy.cn","access_token":"t"}]"#,
    )
    .unwrap();
    std::fs::write(
        root.join("state.json"),
        json!({
            "keys": {"k1": {"name": "k", "hash": "x", "secret": "sk-test", "enabled": true, "realm": "cn"}},
            "pool": {"routing": "round_robin"},
            "audit_logs": [{"action": "login"}],
        })
        .to_string(),
    )
    .unwrap();
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        data_dir: root.clone(),
        accounts_path: root.join("accounts.json"),
        upstream_base: "http://127.0.0.1:1".to_string(),
        billing_base: billing_base.to_string(),
    };
    let store = StateStore::open_writable(root.join("state.json")).unwrap();
    let state = AppState::new(cfg, store).with_admin(Arc::new(AdminState::with_key(KEY)));
    (dir, router(Arc::new(state)))
}

async fn call(app: axum::Router, method: &str, uri: &str, body: Option<Value>, token: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(token) = token {
        builder = builder.header("x-admin-token", token);
    }
    let request = builder.body(Body::from(body.map(|value| value.to_string()).unwrap_or_default())).unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn login_rejects_wrong_key_and_accepts_correct_one() {
    let (_dir, app) = app();
    let (status, _) = call(app.clone(), "POST", "/admin/api/login", Some(json!({"key": "wrong"})), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = call(app, "POST", "/admin/api/login", Some(json!({"key": KEY})), None).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert!(value["token"].as_str().unwrap().starts_with("adm_"));
    assert!(value["csrf"].as_str().unwrap().starts_with("csrf_"));
}

#[tokio::test]
async fn overview_requires_session_and_reports_state() {
    let (_dir, app) = app();
    let (status, _) = call(app.clone(), "GET", "/admin/api/overview", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (_, body) = call(app.clone(), "POST", "/admin/api/login", Some(json!({"key": KEY})), None).await;
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();

    let (status, body) = call(app, "GET", "/admin/api/overview", None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(value["pool"]["routing"], json!("round_robin"));
    assert!(value["keys"].as_array().unwrap().len() == 1);
}

#[tokio::test]
async fn admin_sees_key_secret_and_audit_logs() {
    let (_dir, app) = app();
    let (_, body) = call(app.clone(), "POST", "/admin/api/login", Some(json!({"key": KEY})), None).await;
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();

    let (_, keys) = call(app.clone(), "GET", "/admin/api/keys", None, Some(&token)).await;
    let keys: Value = serde_json::from_str(&keys).unwrap();
    assert_eq!(keys[0]["secret"], json!("sk-test"), "管理员应能看到明文密钥");

    let (status, logs) = call(app, "GET", "/admin/api/audit-logs", None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(logs.contains("login"));
}

#[tokio::test]
async fn key_crud_round_trip() {
    let (_dir, app) = app();
    let (_, body) = call(app.clone(), "POST", "/admin/api/login", Some(json!({"key": KEY})), None).await;
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();

    // 创建
    let (status, created) = call(
        app.clone(), "POST", "/admin/api/keys",
        Some(json!({"name": "新客户端", "realm": "cn", "quota": 1000})), Some(&token),
    ).await;
    assert_eq!(status, StatusCode::OK);
    let created: Value = serde_json::from_str(&created).unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(created["key"].as_str().unwrap().starts_with("sk-wb-"));

    // 列表里能看到（管理员看得到明文）
    let (_, list) = call(app.clone(), "GET", "/admin/api/keys", None, Some(&token)).await;
    let list: Value = serde_json::from_str(&list).unwrap();
    assert!(list.as_array().unwrap().iter().any(|row| row["id"] == json!(id)));

    // 改：停用
    let (status, _) = call(
        app.clone(), "PATCH", &format!("/admin/api/keys/{id}"),
        Some(json!({"enabled": false})), Some(&token),
    ).await;
    assert_eq!(status, StatusCode::OK);

    // 重置用量
    let (status, _) = call(
        app.clone(), "POST", &format!("/admin/api/keys/{id}/reset-usage"), None, Some(&token),
    ).await;
    assert_eq!(status, StatusCode::OK);

    // 删
    let (status, _) = call(app.clone(), "DELETE", &format!("/admin/api/keys/{id}"), None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);

    // 只剩一枚时不允许删
    let (_, list) = call(app.clone(), "GET", "/admin/api/keys", None, Some(&token)).await;
    let remaining = serde_json::from_str::<Value>(&list).unwrap();
    let last_id = remaining[0]["id"].as_str().unwrap().to_string();
    let (status, _) = call(app, "DELETE", &format!("/admin/api/keys/{last_id}"), None, Some(&token)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn read_only_state_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("accounts.json"), "[]").unwrap();
    std::fs::write(root.join("state.json"), json!({"keys": {}}).to_string()).unwrap();
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(), port: 0, data_dir: root.clone(),
        accounts_path: root.join("accounts.json"), upstream_base: "http://127.0.0.1:1".to_string(),
        billing_base: "http://127.0.0.1:1".to_string(),
    };
    let store = StateStore::open(root.join("state.json")).unwrap();   // 只读
    let state = AppState::new(cfg, store).with_admin(Arc::new(AdminState::with_key(KEY)));
    let app = router(Arc::new(state));

    let (_, body) = call(app.clone(), "POST", "/admin/api/login", Some(json!({"key": KEY})), None).await;
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();
    let (status, _) = call(app, "POST", "/admin/api/keys", Some(json!({"name": "x"})), Some(&token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "灰度期只读，写操作必须明确拒绝");
}

async fn mock_billing(payload: &'static str) -> String {
    use axum::routing::post;
    let app = axum::Router::new().route(
        "/v2/billing/meter/get-user-resource",
        post(move || async move { ([(axum::http::header::CONTENT_TYPE, "application/json")], payload) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn pool_settings_persist_and_validate() {
    let (_dir, app) = app();
    let (_, body) = call(app.clone(), "POST", "/admin/api/login", Some(json!({"key": KEY})), None).await;
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();

    let (status, body) = call(
        app.clone(), "PATCH", "/admin/api/pool/settings",
        Some(json!({"routing": "least_used", "auto_checkin": false})), Some(&token),
    ).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["pool"]["routing"], json!("least_used"));

    let (status, _) = call(
        app.clone(), "PATCH", "/admin/api/pool/settings",
        Some(json!({"routing": "nonsense"})), Some(&token),
    ).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, overview) = call(app, "GET", "/admin/api/overview", None, Some(&token)).await;
    let overview: Value = serde_json::from_str(&overview).unwrap();
    assert_eq!(overview["pool"]["routing"], json!("least_used"));
}

#[tokio::test]
async fn account_status_reads_billing_credits() {
    let billing = mock_billing(r#"{"code":0,"data":{"Response":{"Data":{"Accounts":[{"CapacityRemainPrecise":"12.5"}]}}}}"#).await;
    let (_dir, app) = app_with_billing(&billing);
    let (_, body) = call(app.clone(), "POST", "/admin/api/login", Some(json!({"key": KEY})), None).await;
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();

    let (status, body) = call(
        app, "POST", "/admin/api/accounts/a1/actions/status", None, Some(&token),
    ).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["remaining"], json!(12.5));
}

#[tokio::test]
async fn p5_smoke_groups_settings_cli_key() {
    let (_dir, app) = app();
    let (_, body) = call(app.clone(), "POST", "/admin/api/login", Some(json!({"key": KEY})), None).await;
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();

    let (status, created) = call(app.clone(), "POST", "/admin/api/groups", Some(json!({"name": "研发"})), Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(created.contains("grp_"));

    let (status, groups) = call(app.clone(), "GET", "/admin/api/groups", None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(groups.contains("研发"));

    let (status, _) = call(app.clone(), "PUT", "/admin/api/settings", Some(json!({"site_name": "测试站"})), Some(&token)).await;
    assert_eq!(status, StatusCode::OK);

    let (status, rotated) = call(app.clone(), "POST", "/admin/api/cli/key/rotate", None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    let rotated: Value = serde_json::from_str(&rotated).unwrap();
    assert!(rotated["key"].as_str().unwrap().starts_with("sk-wb-"));

    let (status, fetched) = call(app, "GET", "/admin/api/cli/key", None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    let fetched: Value = serde_json::from_str(&fetched).unwrap();
    assert_eq!(fetched["key"], rotated["key"]);
}
