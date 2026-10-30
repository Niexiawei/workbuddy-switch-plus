//! 管理面测试（进程内通道形态）。
//!
//! 管理面已不在 HTTP 上（见 docs/reverse-proxy-internal-plane-plan.md 附录 C），
//! 这里测两件事：
//! 1. **负向断言**：数据面 HTTP router 上不存在任何 `/admin/api/*` 路径（404）；
//! 2. **进程内调用**：直接 `oneshot` admin router，行为与原先带会话的 HTTP 面一致。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use wb_switch_proxy::config::ProxyConfig;
use wb_switch_proxy::http::{router, AppState};
use wb_switch_proxy::state::StateStore;

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
    let state = AppState::new(cfg, store);
    (dir, router(Arc::new(state)))
}

/// admin 子 router（进程内通道的形态：with_state 之后的 Router<()>）。
fn admin_app(state: Arc<AppState>) -> axum::Router {
    wb_switch_proxy::admin::router().with_state(state)
}

async fn call(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = builder
        .body(Body::from(
            body.map(|value| value.to_string()).unwrap_or_default(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

// ── 负向断言：管理面在 HTTP 上不存在 ──────────────────────────────────

#[tokio::test]
async fn admin_plane_is_gone_from_http() {
    let (_dir, app) = app();
    for uri in [
        "/admin/api/overview",
        "/admin/api/login",
        "/admin/api/keys",
        "/admin/api/users",
        "/admin/api/tokens",
    ] {
        let (status, _) = call(app.clone(), "GET", uri, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{uri} 应该 404：管理面不在 HTTP 上"
        );
    }
}

#[tokio::test]
async fn health_still_serves_data_plane() {
    let (_dir, app) = app();
    let (status, body) = call(app, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["engine"], json!("rust"));
    assert_eq!(
        value["capabilities"]["admin"],
        json!(false),
        "能力声明如实：管理面不在 HTTP 上"
    );
}

// ── 进程内调用：admin router 直接 oneshot ─────────────────────────────

#[tokio::test]
async fn in_process_overview_reports_state() {
    let (_dir, _http) = app();
    // 从完整 router 拿不到 state（它被 with_state 消费了），重建一份：
    // 与 http 同配置、同一 state.json —— 进程内通道本来就共享宿主持有的 AppState。
    let root = _dir.path().to_path_buf();
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        data_dir: root.clone(),
        accounts_path: root.join("accounts.json"),
        upstream_base: "http://127.0.0.1:1".to_string(),
        billing_base: "http://127.0.0.1:1".to_string(),
    };
    let store = StateStore::open_writable(root.join("state.json")).unwrap();
    let state = Arc::new(AppState::new(cfg, store));
    let app = admin_app(state);

    let (status, body) = call(app, "GET", "/overview", None).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(value["pool"]["routing"], json!("round_robin"));
    assert_eq!(value["keys"].as_array().unwrap().len(), 1);
    // admin 角色看得到明文密钥（进程内即宿主，无角色降级）。
    assert_eq!(value["keys"][0]["secret"], json!("sk-test"));
}

#[tokio::test]
async fn in_process_key_crud_round_trip() {
    let (_dir, _) = app();
    let root = _dir.path().to_path_buf();
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        data_dir: root.clone(),
        accounts_path: root.join("accounts.json"),
        upstream_base: "http://127.0.0.1:1".to_string(),
        billing_base: "http://127.0.0.1:1".to_string(),
    };
    let store = StateStore::open_writable(root.join("state.json")).unwrap();
    let app = admin_app(Arc::new(AppState::new(cfg, store)));

    // 创建
    let (status, created) = call(
        app.clone(),
        "POST",
        "/keys",
        Some(json!({"name": "新客户端", "realm": "cn", "quota": 1000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let created: Value = serde_json::from_str(&created).unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(created["key"].as_str().unwrap().starts_with("sk-wb-"));

    // 改：停用
    let (status, _) = call(
        app.clone(),
        "PATCH",
        &format!("/keys/{id}"),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 重置用量
    let (status, _) = call(
        app.clone(),
        "POST",
        &format!("/keys/{id}/reset-usage"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 删
    let (status, _) = call(app.clone(), "DELETE", &format!("/keys/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);

    // 只剩一枚时不允许删
    let (_, list) = call(app.clone(), "GET", "/keys", None).await;
    let remaining = serde_json::from_str::<Value>(&list).unwrap();
    let last_id = remaining[0]["id"].as_str().unwrap().to_string();
    let (status, _) = call(app, "DELETE", &format!("/keys/{last_id}"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn in_process_read_only_state_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("accounts.json"), "[]").unwrap();
    std::fs::write(root.join("state.json"), json!({"keys": {}}).to_string()).unwrap();
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        data_dir: root.clone(),
        accounts_path: root.join("accounts.json"),
        upstream_base: "http://127.0.0.1:1".to_string(),
        billing_base: "http://127.0.0.1:1".to_string(),
    };
    let store = StateStore::open(root.join("state.json")).unwrap(); // 只读
    let app = admin_app(Arc::new(AppState::new(cfg, store)));

    let (status, _) = call(app, "POST", "/keys", Some(json!({"name": "x"}))).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "灰度期只读，写操作必须明确拒绝"
    );
}

#[tokio::test]
async fn in_process_pool_settings_persist_and_validate() {
    let (_dir, _) = app();
    let root = _dir.path().to_path_buf();
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        data_dir: root.clone(),
        accounts_path: root.join("accounts.json"),
        upstream_base: "http://127.0.0.1:1".to_string(),
        billing_base: "http://127.0.0.1:1".to_string(),
    };
    let store = StateStore::open_writable(root.join("state.json")).unwrap();
    let app = admin_app(Arc::new(AppState::new(cfg, store)));

    let (status, body) = call(
        app.clone(),
        "PATCH",
        "/pool/settings",
        Some(json!({"routing": "least_used", "auto_checkin": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["pool"]["routing"], json!("least_used"));

    let (status, _) = call(
        app.clone(),
        "PATCH",
        "/pool/settings",
        Some(json!({"routing": "nonsense"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, overview) = call(app, "GET", "/overview", None).await;
    let overview: Value = serde_json::from_str(&overview).unwrap();
    assert_eq!(overview["pool"]["routing"], json!("least_used"));
}

#[tokio::test]
async fn in_process_groups_and_cli_key() {
    let (_dir, _) = app();
    let root = _dir.path().to_path_buf();
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        data_dir: root.clone(),
        accounts_path: root.join("accounts.json"),
        upstream_base: "http://127.0.0.1:1".to_string(),
        billing_base: "http://127.0.0.1:1".to_string(),
    };
    let store = StateStore::open_writable(root.join("state.json")).unwrap();
    let app = admin_app(Arc::new(AppState::new(cfg, store)));

    let (status, created) = call(
        app.clone(),
        "POST",
        "/groups",
        Some(json!({"name": "研发"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(created.contains("grp_"));

    let (status, groups) = call(app.clone(), "GET", "/groups", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(groups.contains("研发"));

    let (status, rotated) = call(app.clone(), "POST", "/cli/key/rotate", None).await;
    assert_eq!(status, StatusCode::OK);
    let rotated: Value = serde_json::from_str(&rotated).unwrap();
    assert!(rotated["key"].as_str().unwrap().starts_with("sk-wb-"));

    let (status, fetched) = call(app, "GET", "/cli/key", None).await;
    assert_eq!(status, StatusCode::OK);
    let fetched: Value = serde_json::from_str(&fetched).unwrap();
    assert_eq!(fetched["key"], rotated["key"]);
}

// ── 数据面回归（原先在 skeleton / chat 里依赖 HostAuth 的形状）────────

async fn mock_billing(payload: &'static str) -> String {
    use axum::routing::post;
    let app = axum::Router::new().route(
        "/v2/billing/meter/get-user-resource",
        post(move || async move {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                payload,
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn account_status_reads_billing_credits() {
    let billing = mock_billing(r#"{"code":0,"data":{"Response":{"Data":{"Accounts":[{"CapacityRemainPrecise":"12.5"}]}}}}"#).await;
    let (_dir, _) = app();
    let root = _dir.path().to_path_buf();
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        data_dir: root.clone(),
        accounts_path: root.join("accounts.json"),
        upstream_base: "http://127.0.0.1:1".to_string(),
        billing_base: billing,
    };
    let store = StateStore::open_writable(root.join("state.json")).unwrap();
    let app = admin_app(Arc::new(AppState::new(cfg, store)));

    let (status, body) = call(app, "POST", "/accounts/a1/actions/status", None).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["remaining"], json!(12.5));
}
