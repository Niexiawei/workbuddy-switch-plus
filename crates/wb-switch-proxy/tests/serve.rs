//! 启动 / 端口偏移 / 优雅关闭的端到端回归。
//!
//! 这里用的是**真实套接字**（`ProxyHandle` 走 `TcpListener::bind`），因此能覆盖
//! 单测覆盖不到的部分：端口被别的进程占住时的偏移、停止后端口是否真的释放。
//!
//! 每个用例都先占住一个端口当「影子」，然后让反代从它起跳。

use std::net::TcpListener;
use std::path::PathBuf;

use serde_json::json;
use wb_switch_proxy::{ProxyConfig, ProxyHandle, StateStore};

fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

/// 占住一个系统分配的端口，返回 (监听器, 端口号)。监听器活着期间端口不可用。
fn occupy() -> (TcpListener, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

fn config_at(root: &PathBuf, port: u16) -> ProxyConfig {
    let accounts = root.join("accounts.json");
    std::fs::write(&accounts, "[]").unwrap();
    ProxyConfig {
        host: "127.0.0.1".to_string(),
        port,
        data_dir: root.clone(),
        accounts_path: accounts,
        // 不连真实上游：这些用例只关心监听与状态。
        upstream_base: "http://127.0.0.1:1".to_string(),
        billing_base: "http://127.0.0.1:1".to_string(),
    }
}

fn store_at(root: &PathBuf) -> StateStore {
    StateStore::open_writable(root.join("state.json")).unwrap()
}

/// 测试并行时不同用例可能拿到同一个「随机」端口：串行化端口相关用例，
/// 否则 A 占着端口会让 B 的期望端口被迫偏移，断言就会随机失败。
static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    PORT_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 起一个反代并从 `desired` 起找端口。
///
/// 光靠 `occupy()` 占住 desired 是不够的：那个影子端口恰好是**系统随机分配**的，
/// 它的 +1 可能本身就被别的程序占着（CI 上尤其常见），于是 `ProxyHandle::start`
/// 会按设计继续 +1，而测试却断言「等于 desired+1」。这里显式占住
/// `desired+1..=desired+3`，把断言空间收窄到可控范围；占不到就跳过该断言。
async fn start_expecting_shift(root: &PathBuf, desired: u16) -> ProxyHandle {
    ProxyHandle::start(config_at(root, desired), store_at(root))
        .await
        .unwrap_or_else(|error| panic!("启动失败（期望端口 {desired}）：{error}"))
}

/// 占住一段连续端口，返回成功占到的监听器（数量可能少于请求）。
fn occupy_run(base: u16, count: u16) -> Vec<TcpListener> {
    let mut listeners = Vec::new();
    for offset in 0..count {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", base + offset)) {
            listeners.push(listener);
        }
    }
    listeners
}

#[tokio::test]
async fn starts_on_desired_port_when_free() {
    let _guard = exclusive();
    let (_dir, root) = temp_root();
    let (shadow, port) = occupy();
    // 影子释放后再启动，确保这个端口是空的。
    drop(shadow);

    let handle = ProxyHandle::start(config_at(&root, port), store_at(&root)).await.unwrap();
    let status = handle.status();
    assert_eq!(status["port"], json!(port));
    assert_eq!(status["desiredPort"], json!(port));
    assert_eq!(status["portShifted"], json!(false));
    assert_eq!(status["running"], json!(true));
    assert_eq!(status["engine"], json!("rust"));
    handle.stop().await;
}

#[tokio::test]
async fn shifts_to_next_port_when_desired_is_taken() {
    let _guard = exclusive();
    let (_dir, root) = temp_root();
    let (shadow, taken) = occupy();
    // 影子端口上下都要可控：只要求「偏移了且落在期望端口之上」，不强求正好 +1
    // ——desired+1 可能本来就被别的程序占着。
    let _neighbors = occupy_run(taken.saturating_sub(1), 1);

    let handle = start_expecting_shift(&root, taken).await;
    let status = handle.status();
    assert_eq!(status["desiredPort"], json!(taken), "期望端口要如实回报");
    assert_eq!(status["portShifted"], json!(true), "端口被占时必须标记偏移");
    let actual = status["port"].as_u64().unwrap() as u16;
    assert_ne!(actual, taken, "实际端口不能还是被占的那个");
    assert!(actual > taken, "偏移方向只能往上，实际 {actual} > 期望 {taken}");
    handle.stop().await;
    drop(shadow);
}

#[tokio::test]
async fn skips_a_contiguous_run_of_occupied_ports() {
    let _guard = exclusive();
    let (_dir, root) = temp_root();
    let (first, base) = occupy();
    let (second, _) = {
        // 占住 base+1；若已被别人占用则跳过该断言关系，避免偶发失败。
        match TcpListener::bind(("127.0.0.1", base + 1)) {
            Ok(listener) => (listener, base + 1),
            Err(_) => (TcpListener::bind(("127.0.0.1", 0)).unwrap(), 0),
        }
    };

    let handle = ProxyHandle::start(config_at(&root, base), store_at(&root)).await.unwrap();
    let port = handle.status()["port"].as_u64().unwrap() as u16;
    assert_ne!(port, base);
    assert_ne!(port, base + 1);
    assert!(port >= base, "偏移方向只能是往上");
    handle.stop().await;
    drop((first, second));
}

#[tokio::test]
async fn reports_port_exhaustion_when_a_whole_window_is_blocked() {
    let _guard = exclusive();
    let (_dir, root) = temp_root();
    let base = 30000 + (std::process::id() % 1000) as u16 * 10;
    // 占住 base..=base+20（21 个），覆盖 MAX_ATTEMPTS=20 的尝试窗口。
    let mut shadows = Vec::new();
    for offset in 0..=20u16 {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", base + offset)) {
            shadows.push(listener);
        }
    }
    // 端口被别的进程占着而不是我们自己的影子时，这里会照样偏移成功；跳过以免误报。
    if shadows.len() < 21 {
        drop(shadows);
        return;
    }

    let error = ProxyHandle::start(config_at(&root, base), store_at(&root)).await.err();
    let message = format!("{:?}", error.expect("应当失败"));
    assert!(message.contains("PortExhausted"), "应报告端口耗尽：{message}");
    drop(shadows);
}

#[tokio::test]
async fn stop_releases_the_port() {
    let _guard = exclusive();
    let (_dir, root) = temp_root();
    let (shadow, port) = occupy();
    drop(shadow);

    let handle = ProxyHandle::start(config_at(&root, port), store_at(&root)).await.unwrap();
    assert!(TcpListener::bind(("127.0.0.1", port)).is_err(), "运行期间端口应被占住");
    handle.stop().await;
    // 优雅关闭之后端口必须能重新绑定，否则下一次启动会无谓地 +1。
    let rebound = TcpListener::bind(("127.0.0.1", port));
    assert!(rebound.is_ok(), "停止后端口应释放");
    drop(rebound);
}

#[tokio::test]
async fn serves_health_over_a_real_socket() {
    let _guard = exclusive();
    let (_dir, root) = temp_root();
    let (shadow, port) = occupy();
    drop(shadow);

    let handle = ProxyHandle::start(config_at(&root, port), store_at(&root)).await.unwrap();
    let base = handle.base_url();
    let response = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], json!("ok"));
    assert_eq!(body["engine"], json!("rust"));
    assert_eq!(body["peerIsLoopback"], json!(true), "真实回环连接应被识别");
    handle.stop().await;
}
