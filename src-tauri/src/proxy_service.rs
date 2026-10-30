//! 内置反代服务：生命周期、端口策略、进程内管理面通道。
//!
//! 取代原来的 `proxy_engine`（那个只能在用户点按钮时启动）。现在的契约是：
//!
//! - **随主程序启动**：`setup` 里 `autostart()`，不需要任何点击；
//! - **随主程序退出**：`RunEvent::Exit` 里 `stop()`，端口必须立刻释放；
//! - **端口**：默认 8787，用户可自定义（持久化在 `state.json` 的反代设置里），
//!   被占用时按 +1 逐个试（最多 20 个），偏移结果如实回报给界面；
//! - **管理面**：不在 HTTP 上（数据面是唯一 HTTP 面，见
//!   `docs/reverse-proxy-internal-plane-plan.md` 附录 C）。前端经
//!   `proxy_admin_request` 命令 → [`Self::admin_request`] → tower `oneshot`
//!   直接调用 admin router，进程内是它的唯一入口，天然可信。
//!
//! 状态机、句柄与 admin router 都放在 Tauri 托管状态里，命令与后台任务共用同一把锁
//! （取 `Arc<Router>` 用短锁，`oneshot` 在锁外 await）。

use std::sync::Arc;
use std::sync::Mutex;

use axum::Router as AxumRouter;
use serde_json::{json, Value};
use tower::ServiceExt as _;
use wb_switch_proxy::config::{ProxyConfig, DEFAULT_PORT};
use wb_switch_proxy::{ProxyHandle, StartError, StateStore};

/// 管理面响应体读入上限。request-logs / audit-logs 大分页的最大观测值再留 4 倍余量。
const MAX_ADMIN_BODY: usize = 8 * 1024 * 1024;

/// 服务的运行阶段。界面据此渲染「启动中 / 在跑 / 停了 / 起不来」。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    Starting,
    Running { port: u16, desired: u16 },
    Failed { reason: String },
}

impl Phase {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running { .. } => "running",
            Self::Failed { .. } => "failed",
        }
    }
}

struct Inner {
    handle: Option<ProxyHandle>,
    phase: Phase,
    /// 最后一次尝试的期望端口（配置里的值）。
    desired_port: u16,
    /// 监听地址。`127.0.0.1`（默认）或 `0.0.0.0`（对局域网开放）。
    /// 供 status() 在未运行时也如实回报；运行时以 handle 实际 bind 为准。
    host: String,
    /// 管理面子 router（P0 瘦身后的形态，已 state 化，可直接 oneshot）。
    /// 与 handle 同锁同生命周期：start 装配、stop 清空。
    admin_router: Option<Arc<AxumRouter>>,
}

pub struct ProxyService {
    inner: Mutex<Inner>,
}

impl Default for ProxyService {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyService {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                handle: None,
                phase: Phase::Stopped,
                desired_port: DEFAULT_PORT,
                host: "127.0.0.1".to_string(),
                admin_router: None,
            }),
        }
    }

    /// 给前端/命令用的状态快照。未启动时也要给出 `engine`，免得界面分不清谁来管。
    pub fn status(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        let (running, port, desired, shifted) = match &inner.phase {
            Phase::Running { port, desired } => (true, Some(*port), *desired, port != desired),
            _ => (false, None, inner.desired_port, false),
        };
        let mut status = json!({
            "engine": "rust",
            "running": running,
            "phase": inner.phase.as_str(),
            "desiredPort": desired,
            "portShifted": shifted,
            "host": inner.host,
            "lanExposed": inner.host != "127.0.0.1" && inner.host != "localhost" && inner.host != "::1",
        });
        if let Some(port) = port {
            status["port"] = json!(port);
            status["baseUrl"] = json!(format!("http://{}:{port}", inner.host));
            // 中文提示放在后端生成：界面不用再拼一遍，避免两处文案漂移。
            status["portNotice"] = if shifted {
                json!(format!("{desired} 已被占用，已在 {port} 运行"))
            } else {
                Value::Null
            };
        }
        if let Phase::Failed { reason } = &inner.phase {
            status["error"] = json!(reason);
        }
        status
    }

    /// 启动（幂等）。已在跑则直接返回当前状态。
    pub async fn start(&self) -> Result<Value, String> {
        if let Phase::Running { .. } = self.inner.lock().unwrap().phase {
            return Ok(self.status());
        }

        // 端口：环境变量 > state.json 里的用户设置 > 8787。
        // 监听地址：环境变量 > state.json 里的用户设置 > 127.0.0.1（P4，附录 D）。
        let mut cfg = ProxyConfig::for_user();
        let store = StateStore::open_writable(cfg.state_path())
            .map_err(|error| format!("无法读取 state.json：{error}"))?;
        if wb_switch_proxy::config::port_override().is_none() {
            let saved = saved_port(&store);
            cfg.port = saved.unwrap_or(cfg.port);
        }
        if std::env::var("WB_SERVE_HOST").map(|v| v.trim().is_empty()).unwrap_or(true) {
            let saved_host = saved_host(&store);
            if let Some(host) = saved_host {
                cfg.host = host;
            }
        }
        cfg.data_dir = cfg
            .state_path()
            .parent()
            .map(|path| path.to_path_buf())
            .unwrap_or(cfg.data_dir);
        std::fs::create_dir_all(&cfg.data_dir)
            .map_err(|error| format!("无法创建反代数据目录：{error}"))?;

        let desired = cfg.port;
        let host = cfg.host.clone();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.phase = Phase::Starting;
            inner.desired_port = desired;
            inner.host = host;
        }

        // 应用状态只有一份：数据面监听与管理面通道共用。
        let state = wb_switch_proxy::build_state(cfg.clone(), store);
        match ProxyHandle::start_with_state(cfg, state.clone()).await {
            Ok(handle) => {
                let port = handle.addr().port();
                // admin 子 router：不经 nest，URI 不带 /admin/api 前缀。
                let admin = Arc::new(wb_switch_proxy::admin::router().with_state(state));
                let mut inner = self.inner.lock().unwrap();
                inner.handle = Some(handle);
                inner.admin_router = Some(admin);
                inner.phase = Phase::Running { port, desired };
                drop(inner);
                Ok(self.status())
            }
            Err(error) => {
                let reason = match &error {
                    StartError::PortExhausted { desired, tried } => format!(
                        "端口 {desired} 起连续 {} 个都被占用，请在服务设置里换一个端口",
                        tried.len()
                    ),
                    StartError::Io(io) => format!("反代端口启动失败：{io}"),
                };
                let mut inner = self.inner.lock().unwrap();
                inner.phase = Phase::Failed { reason: reason.clone() };
                drop(inner);
                Err(reason)
            }
        }
    }

    /// 改监听地址与端口并立即重绑（一次停机 + 启动，与 set_port 同形但二合一）。
    ///
    /// host 只接受 `127.0.0.1`（只绑本机）与 `0.0.0.0`（对局域网开放）两个值——
    /// 不提供任意地址输入：暴露面要么收在本机、要么明确开到全网段，没有中间态。
    /// bind 失败（非端口占用类）直接 Failed，**不做 0.0.0.0 → 127.0.0.1 的静默降级**。
    pub async fn set_bind(&self, host: String, port: u16) -> Result<Value, String> {
        if port == 0 {
            return Err("端口必须在 1–65535 之间".to_string());
        }
        let host = match host.trim() {
            "127.0.0.1" | "localhost" | "::1" => "127.0.0.1".to_string(),
            "0.0.0.0" => "0.0.0.0".to_string(),
            other => return Err(format!("不支持的监听地址：{other}（只支持 127.0.0.1 / 0.0.0.0）")),
        };
        let mut cfg = ProxyConfig::for_user();
        let store = StateStore::open_writable(cfg.state_path())
            .map_err(|error| format!("无法读取 state.json：{error}"))?;
        store
            .update(|value| {
                let settings = value["settings"].as_object_mut().expect("settings 必须是对象");
                let proxy = settings
                    .entry("reverse_proxy".to_string())
                    .or_insert_with(|| json!({}));
                proxy["port"] = json!(port);
                proxy["host"] = json!(host);
            })
            .map_err(|error| format!("写入监听设置失败：{error}"))?;
        cfg.port = port;
        cfg.host = host.clone();

        // 先把旧监听关掉，否则它会占着自己原来的端口，导致新端口选择失败。
        self.stop().await;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.desired_port = port;
            inner.host = host;
        }
        self.start().await
    }

    /// 进程内管理面调用：`{method, path, body}` → `{status, body}`。
    ///
    /// 语义约定（见计划文档 §3.1 / P1.3）：
    /// - `Ok(reply)`：通道本身通了。`reply["status"] >= 400` 时由**前端**判 status
    ///   抛 `RpError`（keys 面板的 409 分支、白名单 403 文案分支零改动）；
    /// - `Err(message)`：通道级故障（服务未运行、请求构造失败、body 超限）。
    ///   Tauri 会把它变成 rejected promise，前端 catch 后抛 `RpError("...", 0)`，
    ///   与现有「连接失败」语义对齐。
    pub async fn admin_request(
        &self,
        method: String,
        path: String,
        body: Option<Value>,
    ) -> Result<Value, String> {
        // 短锁取 router，oneshot 在锁外 await——否则一个慢查询会卡死 status/stop。
        let router = {
            let inner = self.inner.lock().unwrap();
            match (&inner.admin_router, &inner.phase) {
                (Some(router), Phase::Running { .. }) => Some(router.clone()),
                _ => None,
            }
        };
        let Some(router) = router else {
            return Err("内置服务未运行".to_string());
        };

        let request = build_admin_request(method, path, body)?;
        // Arc<Router> 解一层：oneshot 消费 self，Clone 出来的 Router 才能被 move。
        let response = (*router).clone()
            .oneshot(request)
            .await
            .map_err(|error| format!("管理面调用失败：{error}"))?;
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(response.into_body(), MAX_ADMIN_BODY)
            .await
            .map_err(|error| format!("管理面响应读取失败：{error}"))?;
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(json!({ "status": status, "body": body }))
    }

    /// 改期望端口并立即重绑（停机 + 启动）。端口写进 `state.json`，下次启动沿用。
    pub async fn set_port(&self, port: u16) -> Result<Value, String> {
        if port == 0 {
            return Err("端口必须在 1–65535 之间".to_string());
        }
        let mut cfg = ProxyConfig::for_user();
        let store = StateStore::open_writable(cfg.state_path())
            .map_err(|error| format!("无法读取 state.json：{error}"))?;
        store
            .update(|value| {
                let settings = value["settings"].as_object_mut().expect("settings 必须是对象");
                let proxy = settings
                    .entry("reverse_proxy".to_string())
                    .or_insert_with(|| json!({}));
                proxy["port"] = json!(port);
            })
            .map_err(|error| format!("写入端口设置失败：{error}"))?;
        cfg.port = port;

        // 先把旧监听关掉，否则它会占着自己原来的端口，导致新端口选择失败。
        self.stop().await;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.desired_port = port;
        }
        self.start().await
    }

    /// 停机（幂等）。返回停止后的状态快照。
    ///
    /// 用户主动停掉后**不会**被自动拉起：自动启动只在进程启动时跑一次。
    /// 这是刻意的——否则用户点「停止」会立刻被后台任务重新启动，按钮等于骗人。
    pub async fn stop(&self) -> Value {
        let handle = {
            let mut inner = self.inner.lock().unwrap();
            let handle = inner.handle.take();
            if handle.is_some() {
                inner.phase = Phase::Stopped;
            }
            // admin router 与 handle 同生命周期：停机即失效，
            // 通道在 router 为 None 时如实报「服务未运行」。
            inner.admin_router = None;
            handle
        };
        if let Some(handle) = handle {
            handle.stop().await;
        }
        self.status()
    }

    /// 退出路径上的同步停机：`RunEvent::Exit` 不是 async 上下文。
    ///
    /// 用独立运行时 + 2 秒上限：端口释放是尽力而为，绝不能因为等它而卡住退出。
    pub fn stop_blocking(&self) {
        let handle = {
            let mut inner = self.inner.lock().unwrap();
            let handle = inner.handle.take();
            if handle.is_some() {
                inner.phase = Phase::Stopped;
            }
            inner.admin_router = None;
            handle
        };
        let Some(handle) = handle else { return };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let Ok(runtime) = runtime else {
            // 建不出运行时就只能放弃优雅关闭：进程退出时 OS 会回收监听。
            return;
        };
        let stopped = runtime.block_on(async move {
            tokio::time::timeout(std::time::Duration::from_secs(2), handle.stop())
                .await
                .is_ok()
        });
        if !stopped {
            eprintln!("[反代] 优雅停止超时（2s），端口将由进程退出回收");
        }
    }
}

/// 构造发往 admin router 的请求。
///
/// path 是 `rp.*` 语义的相对路径（如 `overview`、`keys/k1`），**不带** `/admin/api`
/// 前缀——admin router 未经 nest，前缀从此不存在，而不是换了个地方拼。
fn build_admin_request(
    method: String,
    path: String,
    body: Option<Value>,
) -> Result<axum::http::Request<axum::body::Body>, String> {
    use axum::http::Method;

    // path 清洗（纵深防御；路由匹配是第二道闸）。
    // 允许查询串：`rp.*` 的大量端点用 `?limit=` / `?days=` 传参
    // （request-logs / usage/stats / security/logs 等），`?` `&` `=` 是合法成分；
    // 越权防护靠「只允许相对路径 + 路由匹配」，不靠拦查询串。
    let trimmed = path.trim().trim_start_matches('/');
    if trimmed.is_empty()
        || trimmed.contains("..")
        || trimmed.contains("//")
        || !trimmed.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false)
        || !trimmed.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | '=' | '&' | '?'))
    {
        return Err(format!("非法的管理面路径：{path}"));
    }
    let uri = format!("/{trimmed}");
    let http_method = Method::from_bytes(method.to_uppercase().as_bytes())
        .map_err(|error| format!("非法的 HTTP 方法 {method}：{error}"))?;
    let mut builder = axum::http::Request::builder().method(http_method).uri(uri);
    let payload = match body {
        Some(value) if !value.is_null() => {
            builder = builder.header("content-type", "application/json");
            serde_json::to_vec(&value)
                .map_err(|error| format!("请求体序列化失败：{error}"))?
        }
        _ => Vec::new(),
    };
    builder
        .body(axum::body::Body::from(payload))
        .map_err(|error| format!("请求构造失败：{error}"))
}

/// `state.json` 里保存的用户端口设置：`settings.reverse_proxy.port`。
fn saved_port(store: &StateStore) -> Option<u16> {
    store
        .get("settings")
        .and_then(|settings| settings.get("reverse_proxy").cloned())
        .and_then(|proxy| proxy.get("port").cloned())
        .and_then(|port| port.as_u64())
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port > 0)
}

/// `state.json` 里保存的监听地址：`settings.reverse_proxy.host`。
/// 只接受两个白名单值，其余（含任意地址）一律忽略——暴露面没有中间态。
fn saved_host(store: &StateStore) -> Option<String> {
    store
        .get("settings")
        .and_then(|settings| settings.get("reverse_proxy").cloned())
        .and_then(|proxy| proxy.get("host").cloned())
        .and_then(|host| host.as_str().map(str::to_string))
        .filter(|host| host == "127.0.0.1" || host == "0.0.0.0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    /// 端口设置的读取：只看 `settings.reverse_proxy.port`，不接受 0 与越界值。
    fn port_of(value: Value) -> Option<u16> {
        value
            .get("settings")
            .and_then(|settings| settings.get("reverse_proxy").cloned())
            .and_then(|proxy| proxy.get("port").cloned())
            .and_then(|port| port.as_u64())
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port > 0)
    }

    #[test]
    fn reads_saved_port_from_settings() {
        let value = json!({"settings": {"reverse_proxy": {"port": 9000}}});
        assert_eq!(port_of(value), Some(9000));
    }

    #[test]
    fn ignores_missing_or_invalid_port() {
        assert_eq!(port_of(json!({})), None);
        assert_eq!(port_of(json!({"settings": {}})), None);
        assert_eq!(port_of(json!({"settings": {"reverse_proxy": {"port": 0}}})), None);
        assert_eq!(port_of(json!({"settings": {"reverse_proxy": {"port": 70000}}})), None);
        assert_eq!(port_of(json!({"settings": {"reverse_proxy": {"port": "8787"}}})), None);
    }

    #[test]
    fn reads_saved_host_from_settings() {
        let store = |body: Value| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            std::fs::write(&path, body.to_string()).unwrap();
            let store = StateStore::open_writable(&path).unwrap();
            std::mem::forget(dir); // 测试进程短命，泄漏 TempDir 保 store 可读
            store
        };
        // 白名单内：原样采纳。
        for host in ["127.0.0.1", "0.0.0.0"] {
            assert_eq!(
                saved_host(&store(json!({"settings": {"reverse_proxy": {"host": host}}}))),
                Some(host.to_string())
            );
        }
        // 白名单外（任意地址 / 空值 / 非字符串 / 缺失）：忽略，回落 127.0.0.1。
        for bad in ["192.168.1.5", "", "example.com"] {
            assert_eq!(
                saved_host(&store(json!({"settings": {"reverse_proxy": {"host": bad}}}))),
                None,
                "host={bad:?} 应被忽略"
            );
        }
        assert_eq!(
            saved_host(&store(json!({"settings": {"reverse_proxy": {"host": 1}}}))),
            None,
            "非字符串应被忽略"
        );
        assert_eq!(saved_host(&store(json!({}))), None);
    }

    #[test]
    fn fresh_service_is_stopped() {
        let service = ProxyService::new();
        let status = service.status();
        assert_eq!(status["running"], json!(false));
        assert_eq!(status["phase"], json!("stopped"));
        assert_eq!(status["engine"], json!("rust"));
        assert_eq!(status["desiredPort"], json!(DEFAULT_PORT));
        // hostToken 已随管理面 HTTP 退役（计划文档附录 C），不再出现在状态里。
        assert!(status.get("hostToken").is_none());
    }

    #[test]
    fn stop_without_start_is_noop() {
        let service = ProxyService::new();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let status = runtime.block_on(service.stop());
        assert_eq!(status["running"], json!(false));
    }

    // ── admin_request 通道 ────────────────────────────────────────────

    fn temp_state() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(
            root.join("accounts.json"),
            r#"[{"id":"a1","uid":"u1","variant":"cn"}]"#,
        )
        .unwrap();
        std::fs::write(
            root.join("state.json"),
            json!({"keys": {"k1": {"name": "k", "hash": "x", "secret": "sk-test", "enabled": true, "realm": "cn"}}}).to_string(),
        )
        .unwrap();
        let store = StateStore::open_writable(root.join("state.json")).unwrap();
        // dir 泄漏给测试进程持有（TempDir drop 即清理，测试进程短命无所谓）。
        std::mem::forget(dir);
        (tempfile::tempdir().unwrap(), store)
    }

    fn running_service() -> (ProxyService, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(
            root.join("accounts.json"),
            r#"[{"id":"a1","uid":"u1","variant":"cn"}]"#,
        )
        .unwrap();
        std::fs::write(
            root.join("state.json"),
            json!({"keys": {"k1": {"name": "k", "hash": "x", "secret": "sk-test", "enabled": true, "realm": "cn"}}}).to_string(),
        )
        .unwrap();
        let service = ProxyService::new();
        // 直接装 router（不真 bind 端口）：通道行为与 handle 生命周期无关。
        let cfg = ProxyConfig::for_user();
        let store = StateStore::open_writable(cfg.state_path()).unwrap_or_else(|_| StateStore::open_writable(root.join("state.json")).unwrap());
        let state = wb_switch_proxy::build_state(cfg, store);
        let admin = Arc::new(wb_switch_proxy::admin::router().with_state(state));
        {
            let mut inner = service.inner.lock().unwrap();
            inner.admin_router = Some(admin);
            inner.phase = Phase::Running { port: 8787, desired: 8787 };
        }
        std::mem::forget(dir);
        (service, root)
    }

    #[tokio::test]
    async fn admin_request_returns_200_for_overview() {
        let (service, _root) = running_service();
        let reply = service.admin_request("GET".into(), "overview".into(), None).await.unwrap();
        assert_eq!(reply["status"], json!(200));
        assert_eq!(reply["body"]["keys"].as_array().map(|rows| rows.len()), Some(1));
    }

    #[tokio::test]
    async fn admin_request_passes_through_4xx() {
        let (service, _root) = running_service();
        let reply = service.admin_request("GET".into(), "no/such/path".into(), None).await.unwrap();
        assert_eq!(reply["status"], json!(404), "4xx 由前端判 status，Rust 侧不走 Err");
    }

    #[tokio::test]
    async fn admin_request_fails_when_not_running() {
        let service = ProxyService::new();
        let error = service.admin_request("GET".into(), "overview".into(), None).await.unwrap_err();
        assert!(error.contains("未运行"), "通道级故障才 Err：{error}");
    }

    #[tokio::test]
    async fn admin_request_rejects_bad_path() {
        let (service, _root) = running_service();
        for bad in ["../escape", "a//b", "", "ok path with space"] {
            assert!(service.admin_request("GET".into(), bad.into(), None).await.is_err(), "{bad} 应被清洗拦截");
        }
    }

    #[tokio::test]
    async fn admin_request_sends_json_body() {
        let (service, _root) = running_service();
        // pool/settings 校验非法值 → BAD_REQUEST，证明 body 已到达 handler。
        let reply = service
            .admin_request("PATCH".into(), "pool/settings".into(), Some(json!({"routing": "nonsense"})))
            .await
            .unwrap();
        assert_eq!(reply["status"], json!(400));
    }

    #[tokio::test]
    async fn stop_clears_admin_router() {
        let (service, _root) = running_service();
        service.stop().await;
        let error = service.admin_request("GET".into(), "overview".into(), None).await.unwrap_err();
        assert!(error.contains("未运行"));
    }

    /// 带查询串的 path 是合法成分（request-logs?limit=… 等）。
    #[test]
    fn build_request_allows_query_string() {
        let get = build_admin_request("GET".into(), "request-logs?limit=5000".into(), None).unwrap();
        assert_eq!(get.uri(), "/request-logs?limit=5000");
        let get2 = build_admin_request("GET".into(), "usage/stats?days=30&hours=24".into(), None).unwrap();
        assert_eq!(get2.uri(), "/usage/stats?days=30&hours=24");
    }

    /// build_admin_request 的形状单测（不依赖运行态）。
    #[test]
    fn build_request_shapes() {
        let get = build_admin_request("get".into(), "/overview".into(), None).unwrap();
        assert_eq!(get.method(), "GET");
        assert_eq!(get.uri(), "/overview");

        let post = build_admin_request("POST".into(), "keys".into(), Some(json!({"name": "x"}))).unwrap();
        assert_eq!(post.uri(), "/keys");
        assert_eq!(post.headers().get("content-type").unwrap(), "application/json");
    }

    /// 保留：ServiceExt 引用消费（oneshot 已在 admin_request 内使用，
    /// 这里防止 cfg(test) 下 unused import 警告波动）。
    #[allow(dead_code)]
    fn _service_ext_marker<R>(router: AxumRouter, request: axum::http::Request<axum::body::Body>) -> impl std::future::Future<Output = ()> {
        async move {
            let _ = tower::Service::<axum::http::Request<axum::body::Body>>::call(&mut router.clone(), request).await;
        }
    }
}
