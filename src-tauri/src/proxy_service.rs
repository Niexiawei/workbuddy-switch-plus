//! 内置反代服务：生命周期、端口策略、宿主令牌。
//!
//! 取代原来的 `proxy_engine`（那个只能在用户点按钮时启动）。现在的契约是：
//!
//! - **随主程序启动**：`setup` 里 `autostart()`，不需要任何点击；
//! - **随主程序退出**：`RunEvent::Exit` 里 `stop()`，端口必须立刻释放；
//! - **端口**：默认 8787，用户可自定义（持久化在 `state.json` 的反代设置里），
//!   被占用时按 +1 逐个试（最多 20 个），偏移结果如实回报给界面；
//! - **鉴权**：启动时生成一次性宿主令牌（只在内存），前端经 `proxy_admin_token`
//!   取用，避免第二次登录。
//!
//! 状态机与句柄都放在 Tauri 托管状态里，命令与后台任务共用同一把锁。

use std::sync::Mutex;

use serde_json::{json, Value};
use wb_switch_proxy::config::{ProxyConfig, DEFAULT_PORT};
use wb_switch_proxy::hostauth::HostAuth;
use wb_switch_proxy::{ProxyHandle, StartError, StateStore};

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
    /// 宿主令牌。随进程生命周期存在，退出即失效。
    token: String,
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
                token: HostAuth::random_token(),
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
            "hostToken": inner.token,
        });
        if let Some(port) = port {
            status["port"] = json!(port);
            status["baseUrl"] = json!(format!("http://127.0.0.1:{port}"));
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

    /// 宿主令牌：前端每次请求前取（有缓存、过期重取）。
    pub fn host_token(&self) -> String {
        self.inner.lock().unwrap().token.clone()
    }

    /// 启动（幂等）。已在跑则直接返回当前状态。
    pub async fn start(&self) -> Result<Value, String> {
        if let Phase::Running { .. } = self.inner.lock().unwrap().phase {
            return Ok(self.status());
        }

        // 端口：环境变量 > state.json 里的用户设置 > 8787。
        let mut cfg = ProxyConfig::for_user();
        let store = StateStore::open_writable(cfg.state_path())
            .map_err(|error| format!("无法读取 state.json：{error}"))?;
        if wb_switch_proxy::config::port_override().is_none() {
            let saved = saved_port(&store);
            cfg.port = saved.unwrap_or(cfg.port);
        }
        cfg.data_dir = cfg
            .state_path()
            .parent()
            .map(|path| path.to_path_buf())
            .unwrap_or(cfg.data_dir);
        std::fs::create_dir_all(&cfg.data_dir)
            .map_err(|error| format!("无法创建反代数据目录：{error}"))?;

        let desired = cfg.port;
        let token = self.host_token();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.phase = Phase::Starting;
            inner.desired_port = desired;
        }

        let auth = HostAuth::with_token(token);
        match ProxyHandle::start_with_auth(cfg, store, auth).await {
            Ok(handle) => {
                let port = handle.addr().port();
                let mut inner = self.inner.lock().unwrap();
                inner.handle = Some(handle);
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn fresh_service_is_stopped_and_has_token() {
        let service = ProxyService::new();
        let status = service.status();
        assert_eq!(status["running"], json!(false));
        assert_eq!(status["phase"], json!("stopped"));
        assert_eq!(status["engine"], json!("rust"));
        assert_eq!(status["desiredPort"], json!(DEFAULT_PORT));
        let token = status["hostToken"].as_str().unwrap();
        assert!(token.starts_with("wbh_"), "宿主令牌要有可辨认的前缀");
        assert_eq!(service.host_token(), token);
    }

    #[test]
    fn token_is_stable_across_status_reads() {
        let service = ProxyService::new();
        let first = service.status()["hostToken"].as_str().unwrap().to_string();
        let second = service.status()["hostToken"].as_str().unwrap().to_string();
        assert_eq!(first, second, "令牌在进程内必须稳定，否则前端缓存会失效");
    }

    #[test]
    fn stop_without_start_is_noop() {
        let service = ProxyService::new();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let status = runtime.block_on(service.stop());
        assert_eq!(status["running"], json!(false));
    }
}
