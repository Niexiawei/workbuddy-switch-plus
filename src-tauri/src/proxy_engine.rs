//! 进程内反代引擎（P1 骨架）。
//!
//! 与旧的 `reverse_proxy::ReverseProxy`（外部 exe 启动器）并存：
//! 旧的那个管 Python exe，这个管进程内 Rust axum。灰度期由前端的「引擎」开关选择用哪个。
//!
//! 生命周期放在 Tauri 托管状态里：启动后句柄常驻，退出时由 `stop` 优雅关闭。

use std::sync::Mutex;

use serde_json::{json, Value};
use wb_switch_proxy::{ProxyConfig, ProxyHandle, StateStore};

#[derive(Default)]
pub struct ProxyEngine {
    inner: Mutex<Option<ProxyHandle>>,
}

impl ProxyEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// 前端直接展示的状态；未启动时也要给出 engine，免得界面上两个引擎分不清。
    pub fn status(&self) -> Value {
        let guard = self.inner.lock().unwrap();
        match guard.as_ref() {
            Some(handle) => {
                let mut status = handle.status();
                status["engine"] = json!("rust");
                status
            }
            None => json!({ "running": false, "engine": "rust" }),
        }
    }

    pub async fn start(&self, port: Option<u16>) -> Result<Value, String> {
        if self.inner.lock().unwrap().is_some() {
            return Ok(self.status());
        }
        let mut cfg = ProxyConfig::for_user();
        if let Some(port) = port {
            cfg.port = port;
        }
        std::fs::create_dir_all(&cfg.data_dir)
            .map_err(|error| format!("无法创建反代数据目录：{error}"))?;
        // 灰度期只读打开：Python 仍是唯一写者。
        let store = StateStore::open(cfg.state_path())
            .map_err(|error| format!("无法读取 state.json：{error}"))?;
        let handle = ProxyHandle::start(cfg, store)
            .await
            .map_err(|error| format!("反代端口启动失败（可能被占用）：{error}"))?;
        let status = handle.status();
        *self.inner.lock().unwrap() = Some(handle);
        Ok(status)
    }

    pub async fn stop(&self) -> Result<Value, String> {
        let handle = { self.inner.lock().unwrap().take() };
        if let Some(handle) = handle {
            handle.stop().await;
        }
        Ok(json!({ "running": false, "engine": "rust" }))
    }
}
