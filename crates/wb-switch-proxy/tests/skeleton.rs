//! P1 骨架的回归：状态默认值 / 只读保护 / 账号视图 / 两个只读端点。

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use wb_switch_proxy::accounts;
use wb_switch_proxy::config::ProxyConfig;
use wb_switch_proxy::http::{router, AppState};
use wb_switch_proxy::state::{StateStore, STATE_KEYS};

fn temp_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

#[test]
fn missing_state_file_gets_every_default_key() {
    let (_dir, root) = temp_dir();
    let store = StateStore::open(root.join("state.json")).unwrap();
    for key in STATE_KEYS {
        assert!(store.get(key).is_some(), "缺少默认键 {key}");
    }
    assert!(store.get("keys").unwrap().is_object());
    assert_eq!(store.get("runtime_since").unwrap(), json!(0));
    assert!(!store.writable());
}

#[test]
fn unknown_keys_survive_and_writable_save_round_trips() {
    let (_dir, root) = temp_dir();
    let path = root.join("state.json");
    std::fs::write(&path, r#"{"future_field":{"keep":true},"keys":{"k1":{"name":"x"}}}"#).unwrap();

    let store = StateStore::open_writable(&path).unwrap();
    assert_eq!(store.get("future_field").unwrap()["keep"], json!(true));
    store.save().unwrap();

    let reloaded: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(reloaded["future_field"]["keep"], json!(true));
    assert_eq!(reloaded["keys"]["k1"]["name"], json!("x"));
    assert!(reloaded["users"].is_object(), "保存时必须补齐默认键");
}

#[test]
fn readonly_store_refuses_to_save() {
    let (_dir, root) = temp_dir();
    let store = StateStore::open(root.join("state.json")).unwrap();
    let error = store.save().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn account_view_derives_variant_and_token_presence() {
    let (_dir, root) = temp_dir();
    let path = root.join("accounts.json");
    std::fs::write(&path, r#"[
        {"id":"a1","uid":"u1","nickname":"CN 号","variant":"cn","domain":"www.codebuddy.cn","access_token":"t"},
        {"uid":"u2","domain":"www.workbuddy.ai"}
    ]"#).unwrap();

    let rows = accounts::list_at(&path);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "a1");
    assert_eq!(rows[0].variant, "cn");
    assert!(rows[0].has_token);
    assert_eq!(rows[1].id, "u2", "没有 id 时用 uid 兜底");
    assert_eq!(rows[1].variant, "ai", "domain 兜底判档位");
    assert!(!rows[1].has_token);
}

fn app_with_temp_state() -> (tempfile::TempDir, axum::Router) {
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
    let store = StateStore::open(root.join("state.json")).unwrap();
    let app = router(std::sync::Arc::new(AppState::new(cfg, store)));
    (dir, app)
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn health_reports_skeleton_capabilities() {
    let (_dir, app) = app_with_temp_state();
    let response = app
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["status"], json!("ok"));
    assert_eq!(body["engine"], json!("rust"));
    assert_eq!(body["accounts"], json!(1));
    assert_eq!(body["capabilities"]["chat"], json!(true), "chat 已迁移，应如实声明");
    assert_eq!(body["capabilities"]["admin"], json!(false), "管理面仍在 Python");
}

#[tokio::test]
async fn models_returns_seed_catalogue() {
    let (_dir, app) = app_with_temp_state();
    let response = app
        .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let data = body["data"].as_array().unwrap();
    assert!(!data.is_empty());
    // 远端不可达时可能退到本机产品配置；两种来源都是允许的，但不能为空。
    let source = data[0]["source"].as_str().unwrap();
    assert!(source == "seed" || source == "catalogue", "未知来源 {source}");
    assert!(data[0]["context_window"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn chat_without_account_returns_503() {
    let (_dir, app) = app_with_temp_state();
    let second = app.clone();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"messages":[]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(response).await;
    assert_eq!(body["error"]["type"], json!("no_account"));

    // /v1/responses 已迁移；没有账号时同样应给 503 而不是 501。
    let response = second
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"input":"hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
