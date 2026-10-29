//! WB-Switch-Plus 内置反代（Rust 版骨架）。
//!
//! 目标形态见 `docs/reverse-proxy-rust-migration-plan.md`：反代不再是独立 Python
//! 进程，而是桌面端进程内的一个 axum 监听；webui 通过 BFF 转发到它。
//!
//! P1 只落地「骨架 + 只读状态 + 两个只读端点」：
//!   · `/health`（含账号数、数据目录、是否可写）
//!   · `/v1/models`（先用国内版种子清单，真实目录在 P2 接）
//! 数据面与鉴权在后续阶段补，未实现的路由返回 501 而不是伪装成功。

pub mod accounts;
pub mod admin;
pub mod billing;
pub mod anthropic;
pub mod catalog;
pub mod chat;
pub mod config;
pub mod desensitize;
pub mod errors;
pub mod http;
pub mod keys;
pub mod pool;
pub mod profile;
pub mod projection;
pub mod prompt;
pub mod responses;
pub mod state;
pub mod wire;

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub use config::ProxyConfig;
pub use state::StateStore;

/// 运行中的反代句柄。持有监听地址与优雅退出通道。
pub struct ProxyHandle {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl ProxyHandle {
    /// 在 `cfg.host:cfg.port` 上启动监听；port = 0 时由系统分配（测试用）。
    pub async fn start(cfg: ProxyConfig, store: StateStore) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port)).await?;
        let addr = listener.local_addr()?;
        let app = http::router(Arc::new(http::AppState::new(cfg, store)));
        let (tx, rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(Self { addr, shutdown: Some(tx), join: Some(join) })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "running": self.shutdown.is_some(),
            "baseUrl": self.base_url(),
            "port": self.addr.port(),
            "engine": "rust",
        })
    }

    /// 优雅停止：先关监听，再等任务结束。
    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}
