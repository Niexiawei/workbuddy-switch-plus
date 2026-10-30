//! 单账号 chat 透传的端到端用例：真实 axum mock 上游 + 真实 router。

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use wb_switch_proxy::config::ProxyConfig;
use wb_switch_proxy::http::{router, AppState};
use wb_switch_proxy::state::StateStore;

const SSE_OK: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"pong\"}}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n\
data: [DONE]\n\n";

const SSE_INCOMPLETE: &str =
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"half\"}}]}\n\n";

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    upstream: String,
}

async fn mock_upstream(status: u16, body: &'static str) -> String {
    let app = Router::new().route(
        "/v2/chat/completions",
        post(move || async move {
            (
                StatusCode::from_u16(status).unwrap(),
                [(header::CONTENT_TYPE, "text/event-stream")],
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

fn fixture(upstream: String) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("accounts.json"),
        r#"[{"id":"a1","uid":"u1","nickname":"测试","variant":"cn","domain":"www.codebuddy.cn","access_token":"account-token"}]"#,
    )
    .unwrap();
    Fixture {
        _dir: dir,
        root,
        upstream,
    }
}

fn app(fixture: &Fixture) -> Router {
    let cfg = ProxyConfig {
        host: "127.0.0.1".to_string(),
        port: 0,
        data_dir: fixture.root.clone(),
        accounts_path: fixture.root.join("accounts.json"),
        upstream_base: fixture.upstream.clone(),
        billing_base: "http://127.0.0.1:1".to_string(),
    };
    let store = StateStore::open(fixture.root.join("state.json")).unwrap();
    router(Arc::new(AppState::new(cfg, store)))
}

async fn post_json(app: Router, uri: &str, body: serde_json::Value) -> (StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn post_chat(app: Router, stream: bool) -> (StatusCode, String) {
    let body = json!({
        "model": "deepseek-v4-flash",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": stream,
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn non_stream_aggregates_sse_into_completion() {
    let upstream = mock_upstream(200, SSE_OK).await;
    let fixture = fixture(upstream);
    let (status, text) = post_chat(app(&fixture), false).await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], json!("pong"));
    assert_eq!(body["choices"][0]["finish_reason"], json!("stop"));
    assert_eq!(body["usage"]["total_tokens"], json!(4));
}

#[tokio::test]
async fn stream_passes_frames_through() {
    let upstream = mock_upstream(200, SSE_OK).await;
    let fixture = fixture(upstream);
    let (status, text) = post_chat(app(&fixture), true).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("\"content\":\"pong\""), "{text}");
    assert!(text.contains("[DONE]"));
}

#[tokio::test]
async fn incomplete_upstream_stream_appends_error_frame() {
    let upstream = mock_upstream(200, SSE_INCOMPLETE).await;
    let fixture = fixture(upstream);
    let (status, text) = post_chat(app(&fixture), true).await;
    assert_eq!(status, StatusCode::OK, "流已经开始，HTTP 状态无法再改");
    assert!(text.contains("half"));
    assert!(
        text.contains("incomplete_stream"),
        "半截回答必须显式报错: {text}"
    );
}

#[tokio::test]
async fn upstream_error_is_mapped_and_explained() {
    let upstream = mock_upstream(400, "{\"code\":11128,\"msg\":\"channel check\"}").await;
    let fixture = fixture(upstream);
    // 这个 mock 不返回 SSE 头，但 converter 只按状态码分支，足够验证映射。
    let (status, text) = post_chat(app(&fixture), false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["wb_kind"], json!("bad_request"));
    assert!(body["error"]["message"].as_str().unwrap().contains("11128"));
}

#[tokio::test]
async fn anthropic_route_emits_message_events() {
    let upstream = mock_upstream(200, SSE_OK).await;
    let fixture = fixture(upstream);
    let body = json!({
        "model": "deepseek-v4-flash",
        "max_tokens": 16,
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let (status, text) = post_json(app(&fixture), "/v1/messages", body).await;
    assert_eq!(status, StatusCode::OK);
    for name in ["message_start", "content_block_delta", "message_stop"] {
        assert!(
            text.contains(&format!("event: {name}")),
            "缺少 {name}: {text}"
        );
    }
    assert!(text.contains("\"text\":\"pong\""));
}

#[tokio::test]
async fn responses_route_emits_responses_events() {
    let upstream = mock_upstream(200, SSE_OK).await;
    let fixture = fixture(upstream);
    let body = json!({"model": "deepseek-v4-flash", "input": "hi", "stream": true});
    let (status, text) = post_json(app(&fixture), "/v1/responses", body).await;
    assert_eq!(status, StatusCode::OK);
    for name in [
        "response.created",
        "response.output_text.delta",
        "response.completed",
    ] {
        assert!(
            text.contains(&format!("event: {name}")),
            "缺少 {name}: {text}"
        );
    }
    assert!(text.contains("\"delta\":\"pong\""));
}
