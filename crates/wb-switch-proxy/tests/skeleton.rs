//! 路由层面的回归：health / models / usage。
//!
//! 迁移说明：`/health` 现在会读 `ConnectInfo`（用于报告 `peerIsLoopback`），
//! 所以路由测试必须像真实监听那样注入连接信息——`Router::oneshot` 直接调用
//! 会让 extractor 失败并返回 500，那是测试接线问题，不是端点问题。
//!
//! 端到端（真实 bind、端口偏移）见 `tests/serve.rs`。

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use wb_switch_proxy::config::ProxyConfig;
use wb_switch_proxy::http::{router, AppState};
use wb_switch_proxy::state::StateStore;

fn temp_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn app_with(keys: Value, writable: bool) -> (tempfile::TempDir, Router) {
    let (dir, root) = temp_dir();
    let accounts_path = root.join("accounts.json");
    std::fs::write(&accounts_path, r#"[{"id":"a1","uid":"u1","variant":"cn"}]"#).unwrap();
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        data_dir: root.clone(),
        accounts_path,
        upstream_base: "http://127.0.0.1:1".to_string(),
        billing_base: "http://127.0.0.1:1".to_string(),
    };
    let path = root.join("state.json");
    std::fs::write(&path, json!({ "keys": keys }).to_string()).unwrap();
    let store = if writable {
        StateStore::open_writable(path).unwrap()
    } else {
        StateStore::open(path).unwrap()
    };
    let app = router(std::sync::Arc::new(AppState::new(cfg, store)));
    (dir, app)
}

fn loopback_request(uri: &str) -> Request<Body> {
    let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50000))));
    request
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn health_reports_capabilities_and_peer() {
    let (_dir, app) = app_with(json!({}), false);
    let response = app.oneshot(loopback_request("/health")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["status"], json!("ok"));
    assert_eq!(body["engine"], json!("rust"));
    assert_eq!(body["accounts"], json!(1));
    assert_eq!(body["capabilities"]["chat"], json!(true));
    assert_eq!(body["capabilities"]["admin"], json!(false));
    assert_eq!(body["capabilities"]["usage"], json!(true));
    // IP 白名单已在 keys::ip_allowed 里执行（I4），能力声明同步为 true。
    assert_eq!(body["capabilities"]["ipAllowlist"], json!(true));
    assert_eq!(
        body["stateWritable"],
        json!(false),
        "只读实例应如实报告不可写"
    );
    assert_eq!(body["peerIsLoopback"], json!(true));
}

#[tokio::test]
async fn models_requires_no_key_on_fresh_install() {
    let (_dir, app) = app_with(json!({}), false);
    let response = app.oneshot(loopback_request("/v1/models")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "没有密钥时不应拦截");
    let body = body_json(response).await;
    assert_eq!(body["object"], json!("list"));
    assert!(
        !body["data"].as_array().unwrap().is_empty(),
        "种子清单不应为空"
    );
}

#[tokio::test]
async fn gateway_usage_rejects_missing_key_when_keys_exist() {
    let keys = json!({
        "k1": {"hash": sha256_hex("secret"), "name": "k", "enabled": true, "quota": 100}
    });
    let (_dir, app) = app_with(keys, false);
    let response = app.oneshot(loopback_request("/v1/usage")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], json!("missing"));
}

#[tokio::test]
async fn gateway_usage_returns_quota_for_valid_key() {
    let keys = json!({
        "k1": {
            "hash": sha256_hex("secret"),
            "name": "测试",
            "enabled": true,
            "quota": 100,
            "used_tokens": 40,
            "models": ["glm-5.3"]
        }
    });
    let (_dir, app) = app_with(keys, false);
    let request = Request::builder()
        .uri("/v1/usage")
        .header("authorization", "Bearer secret")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["object"], json!("usage"));
    assert_eq!(body["tokens"]["limit"], json!(100));
    assert_eq!(body["tokens"]["used"], json!(40));
    assert_eq!(body["tokens"]["remaining"], json!(60));
    assert_eq!(body["key"]["name"], json!("测试"));
}
