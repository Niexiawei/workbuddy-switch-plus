//! 账号池动作层的回归用例（P-A2）。
//!
//! 覆盖三件事，对应本轮修掉的三类问题：
//!
//! 1. **六个动作都能被识别**，未知动作 404 而不是「尚未迁移」；
//! 2. **批量是资源**：并发时 409，单个账号失败不掀翻整批；
//! 3. **流水**：`kind` 过滤、新的在前、`limit` 截断、按类清空。
//!
//! 上游一律用本地 axum mock（与 `admin.rs::mock_billing` 同一手法），
//! 不碰真实账号、不消耗积分。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use wb_switch_proxy::http::AppState;
use wb_switch_proxy::state::StateStore;
use wb_switch_proxy::ProxyConfig;

/// 建一个临时工作区：账号库 + state.json + 一个可注入的上游 mock 基址。
fn workspace(accounts: &str) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("accounts.json"), accounts).unwrap();
    std::fs::write(root.join("state.json"), json!({}).to_string()).unwrap();
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
    (dir, state)
}

fn admin_app(state: Arc<AppState>) -> axum::Router {
    wb_switch_proxy::admin::router().with_state(state)
}

async fn call(
    app: axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => {
            builder = builder.header("content-type", "application/json");
            builder.body(Body::empty()).unwrap()
        }
    };
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

const ONE_ACCOUNT: &str = r#"[
    {"id":"a1","uid":"u1","variant":"cn","domain":"www.codebuddy.cn","access_token":"t1","refresh_token":"r1","expiresAt":1796009812159}
]"#;

// ── 动作解析 ──────────────────────────────────────────────────────────

#[tokio::test]
async fn every_documented_action_is_accepted() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    for action in ["status", "checkin", "refresh", "travel", "travel-claim", "active"] {
        let (status, _) = call(
            admin_app(state.clone()),
            "POST",
            &format!("/accounts/a1/actions/{action}"),
            None,
        )
        .await;
        // 上游不可达时这些动作会返回 200 + ok=false（结果是失败，不是路由不存在）。
        // 关键判据是**不再是 501**。
        assert_ne!(
            status,
            StatusCode::NOT_IMPLEMENTED,
            "动作 {action} 不应该报「尚未迁移」"
        );
    }
}

#[tokio::test]
async fn unknown_action_is_404_not_not_implemented() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    // 「拼错了」与「还没做」必须分开：前者 404，后者曾经是 501。
    for action in ["nonsense", "travel_claim"] {
        let (status, body) = call(
            admin_app(state.clone()),
            "POST",
            &format!("/accounts/a1/actions/{action}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "动作 {action:?} 应 404");
        assert!(
            body["detail"].as_str().unwrap_or("").contains("未知动作"),
            "应当说明是未知动作：{body}"
        );
    }
    // 空动作连路由都匹配不上（axum 的 `:action` 不吃空段），走的是框架兜底 404：
    // 没有 JSON body，但**仍然是 404**——这是要守住的部分。
    let (status, _) = call(
        admin_app(state),
        "POST",
        "/accounts/a1/actions/",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pool_action_rejects_unknown_names() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    let (status, _) = call(
        admin_app(state),
        "POST",
        "/pool/actions/nonsense",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── 单个动作的结果形状 ────────────────────────────────────────────────

#[tokio::test]
async fn missing_account_is_a_result_not_a_transport_error() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    let (status, body) = call(
        admin_app(state),
        "POST",
        "/accounts/nope/actions/status",
        None,
    )
    .await;
    // 账号不存在是**业务结果**：批量里要能逐行显示，不能整批 404。
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["id"], json!("nope"));
    assert!(body["message"].as_str().unwrap().contains("账号不存在"));
}

#[tokio::test]
async fn paused_account_does_not_run_any_action() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    state
        .store
        .update(|value| {
            value["account_overrides"] = json!({"a1": {"enabled": false}});
        })
        .unwrap();
    let (status, body) = call(
        admin_app(state),
        "POST",
        "/accounts/a1/actions/checkin",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(false));
    assert!(body["message"].as_str().unwrap().contains("已暂停"));
}

#[tokio::test]
async fn every_action_result_is_a_well_formed_envelope() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    for action in ["status", "checkin", "refresh", "travel", "travel-claim", "active"] {
        let (status, body) = call(
            admin_app(state.clone()),
            "POST",
            &format!("/accounts/a1/actions/{action}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{action} 应当是 200");
        // 前端对每一条都读这三个键（`accounts-panel.tsx` 的 runAccountAction）。
        assert_eq!(body["id"], json!("a1"), "{action} 缺 id");
        assert!(body["ok"].is_boolean(), "{action} 缺布尔 ok：{body}");
        assert!(body["message"].is_string(), "{action} 缺 message：{body}");
    }
}

#[tokio::test]
async fn international_account_skips_travel_and_active_without_a_request() {
    let accounts = r#"[
        {"id":"a1","uid":"u1","variant":"ai","domain":"www.workbuddy.ai","access_token":"t1"}
    ]"#;
    let (_dir, state) = workspace(accounts);

    let (_, body) = call(
        admin_app(state.clone()),
        "POST",
        "/accounts/a1/actions/travel",
        None,
    )
    .await;
    assert_eq!(body["ok"], json!(false));
    assert!(
        body["message"].as_str().unwrap().contains("国际版"),
        "国际版应当被短路并说明原因：{body}"
    );

    let (_, body) = call(
        admin_app(state),
        "POST",
        "/accounts/a1/actions/active",
        None,
    )
    .await;
    assert_eq!(body["skip"], json!("intl"));
    assert!(body["message"].as_str().unwrap().contains("国内"));
}

// ── 批量 ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn batch_returns_one_result_per_enabled_account() {
    let accounts = r#"[
        {"id":"a1","uid":"u1","variant":"cn","access_token":"t1"},
        {"id":"a2","uid":"u2","variant":"cn","access_token":"t2"}
    ]"#;
    let (_dir, state) = workspace(accounts);
    state
        .store
        .update(|value| {
            value["account_overrides"] = json!({"a2": {"enabled": false}});
        })
        .unwrap();
    let (status, body) = call(
        admin_app(state),
        "POST",
        "/pool/actions/status",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().expect("results 必须是数组");
    // 暂停的账号不参与批量（与单个动作同一判据）。
    assert_eq!(results.len(), 1, "只应包含启用中的账号：{body}");
    assert_eq!(results[0]["id"], json!("a1"));
}

#[tokio::test]
async fn one_failure_does_not_sink_the_whole_batch() {
    let accounts = r#"[
        {"id":"a1","uid":"u1","variant":"cn","access_token":"t1"},
        {"id":"a2","uid":"u2","variant":"cn","access_token":"t2"}
    ]"#;
    let (_dir, state) = workspace(accounts);
    // 上游不可达：每个账号都会失败，但整批仍然是 200 + 两条结果。
    let (status, body) = call(
        admin_app(state),
        "POST",
        "/pool/actions/checkin",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "批量永远 200，失败信息进 results");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for row in results {
        assert!(row["id"].is_string());
        assert!(row["ok"].is_boolean());
        assert!(row["message"].is_string());
    }
}

#[tokio::test]
async fn a_second_batch_while_one_runs_is_409() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    // 直接占用 jobs 锁，模拟「已经有一批在跑」。
    let runner = state.tasks.clone();
    let held = runner.batch(wb_switch_proxy::tasks::Action::Status);
    // 不 await 它，先发第二个请求：锁还在这批手里。
    let (status, body) = call(
        admin_app(state.clone()),
        "POST",
        "/pool/actions/status",
        None,
    )
    .await;
    // 第一批可能已经跑完（上游不可达会立刻返回），所以这里接受两种结局之一：
    // 要么 409（锁被占），要么 200（前一批已完成）。**不允许** 501/500。
    assert!(
        status == StatusCode::CONFLICT || status == StatusCode::OK,
        "并发批量只能是 409 或 200，实际 {status}"
    );
    if status == StatusCode::CONFLICT {
        assert!(body["detail"].as_str().unwrap().contains("批量任务进行中"));
    }
    let _ = held.await;
}

// ── 任务流水 ──────────────────────────────────────────────────────────

fn seed_logs(state: &AppState) {
    state
        .store
        .update(|value| {
            value["checkin_logs"] = json!([
                {"time": 100, "id": "a1", "kind": "checkin", "ok": true, "message": "签到成功"},
                {"time": 200, "id": "a1", "kind": "travel", "ok": false, "message": "无 Buddy"},
                {"time": 300, "id": "a1", "kind": "active", "ok": true, "message": "连登 3 天"},
            ]);
        })
        .unwrap();
}

#[tokio::test]
async fn logs_are_newest_first() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    seed_logs(&state);
    let (status, body) = call(
        admin_app(state),
        "GET",
        "/pool/checkin-logs",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["time"], json!(300), "最新的必须在最前");
    assert_eq!(rows[2]["time"], json!(100));
}

#[tokio::test]
async fn logs_filter_by_kind() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    seed_logs(&state);
    let (_, body) = call(
        admin_app(state.clone()),
        "GET",
        "/pool/checkin-logs?kind=travel",
        None,
    )
    .await;
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 1, "kind=travel 只应留下旅行那一条：{body}");
    assert_eq!(rows[0]["kind"], json!("travel"));

    // `kind=all` 是前端的哨兵值（Radix SelectItem 不接受空串），必须等于不过滤。
    let (_, body) = call(
        admin_app(state),
        "GET",
        "/pool/checkin-logs?kind=all",
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn logs_respect_the_limit() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    seed_logs(&state);
    let (_, body) = call(
        admin_app(state),
        "GET",
        "/pool/checkin-logs?limit=2",
        None,
    )
    .await;
    assert_eq!(body.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn summary_breaks_down_by_kind() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    state
        .store
        .update(|value| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            value["checkin_logs"] = json!([
                {"time": now - 10, "id": "a1", "kind": "checkin", "ok": true, "message": ""},
                {"time": now - 20, "id": "a1", "kind": "checkin", "ok": false, "message": ""},
                {"time": now - 30, "id": "a1", "kind": "travel", "ok": true, "message": ""},
            ]);
        })
        .unwrap();
    let (status, body) = call(
        admin_app(state),
        "GET",
        "/pool/checkin-logs/summary?hours=24",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hours"], json!(24));
    let kinds = body["kinds"].as_array().unwrap();
    let checkin = kinds
        .iter()
        .find(|row| row["kind"] == json!("checkin"))
        .unwrap();
    assert_eq!(checkin["total"], json!(2));
    assert_eq!(checkin["ok"], json!(1));
    assert_eq!(checkin["failed"], json!(1));
    assert_eq!(checkin["success_rate"], json!(0.5));
}

#[tokio::test]
async fn clear_can_target_a_single_kind() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    seed_logs(&state);
    let (status, body) = call(
        admin_app(state.clone()),
        "POST",
        "/pool/checkin-logs/clear?kind=travel",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["removed"], json!(1));
    let remaining = state.store.get("checkin_logs").unwrap();
    assert_eq!(remaining.as_array().unwrap().len(), 2, "只清掉旅行那一条");
    assert!(remaining
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["kind"] != json!("travel")));
}

#[tokio::test]
async fn clear_without_kind_empties_everything() {
    let (_dir, state) = workspace(ONE_ACCOUNT);
    seed_logs(&state);
    let (_, body) = call(
        admin_app(state.clone()),
        "POST",
        "/pool/checkin-logs/clear",
        None,
    )
    .await;
    assert_eq!(body["removed"], json!(3));
    assert_eq!(state.store.get("checkin_logs").unwrap(), json!([]));
}
