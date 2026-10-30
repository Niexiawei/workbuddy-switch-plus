//! /v1/models 的目录来源：远端 /v3/config 优先，失败退本机/种子。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use wb_switch_proxy::config::ProxyConfig;
use wb_switch_proxy::http::{router, AppState};
use wb_switch_proxy::state::StateStore;

async fn mock_upstream(body: &'static str) -> String {
    let app = Router::new().route(
        "/v3/config",
        get(move || async move {
            (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
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

fn app_with(upstream: String) -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("accounts.json"),
        r#"[{"id":"a1","uid":"u1","variant":"cn","domain":"www.codebuddy.cn","access_token":"token"}]"#,
    )
    .unwrap();
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        data_dir: root.clone(),
        accounts_path: root.join("accounts.json"),
        upstream_base: upstream,
        billing_base: "http://127.0.0.1:1".to_string(),
    };
    let store = StateStore::open(root.join("state.json")).unwrap();
    (dir, router(Arc::new(AppState::new(cfg, store))))
}

async fn models_json(app: Router) -> Value {
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn remote_catalogue_is_used_and_filtered() {
    let upstream = mock_upstream(
        r#"{"code":0,"data":{"models":[
            {"id":"glm-5.3","maxInputTokens":1000000,"supportsImages":true,"tags":["chat"]},
            {"id":"draw","tags":["text-to-image"]}
        ]}}"#,
    )
    .await;
    let (_dir, app) = app_with(upstream);
    let body = models_json(app).await;
    let data = body["data"].as_array().unwrap();
    let ids: Vec<&str> = data
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect();
    assert!(ids.contains(&"glm-5.3"));
    assert!(!ids.contains(&"draw"), "生成类模型应被过滤");
    let glm = data
        .iter()
        .find(|model| model["id"] == json!("glm-5.3"))
        .unwrap();
    assert_eq!(glm["source"], json!("catalogue"));
    assert_eq!(glm["context_window"], json!(1_000_000));
    assert_eq!(glm["input_modalities"], json!(["text", "image"]));
    // 种子补缺：目录里没有的 id 依然提供，来源标 seed。
    assert!(data.iter().any(|model| model["source"] == json!("seed")));
}

#[tokio::test]
async fn falls_back_to_seed_when_upstream_fails() {
    // 关闭的端口 = 远端不可用；本机产品配置在测试机上是否存在不影响断言：
    // 至少必须给出非空清单，且每项带上下文窗口。
    let (_dir, app) = app_with("http://127.0.0.1:1".to_string());
    let body = models_json(app).await;
    let data = body["data"].as_array().unwrap();
    assert!(!data.is_empty());
    assert!(data
        .iter()
        .all(|model| model["context_window"].as_i64().unwrap_or(0) > 0));
}
