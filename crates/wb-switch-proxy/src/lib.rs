//! WB-Switch-Plus 内置反代。
//!
//! 这是主项目进程内的 axum 监听：由 Tauri 桌面端在启动时拉起（`ProxyService`），
//! 随主进程退出而关闭，不存在独立的 exe 或第二个宿主。
//!
//! 数据面（唯一 HTTP 面）：`/health`、`/v1/models`、4 条对话接口、`/v1/usage`。
//! 管理面：不在 HTTP 上，`crate::admin::router()` 由桌面端进程内调用
//! （`ProxyService::admin_request` → tower `oneshot`）。
//!
//! 端口：`ProxyConfig.port` 是**期望**端口，实际端口由 [`ports`] 在冲突时 +1 决定，
//! 两者都通过 [`ProxyHandle::status`] 回给宿主。

pub mod accounts;
pub mod admin;
pub mod anthropic;
pub mod billing;
pub mod catalog;
pub mod chat;
pub mod config;
pub mod desensitize;
pub mod errors;
pub mod hostauth;
pub mod http;
pub mod keys;
pub mod metrics;
pub mod pool;
pub mod ports;
pub mod profile;
pub mod projection;
pub mod prompt;
pub mod responses;
pub mod state;
pub mod usage;
pub mod wire;

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub use config::ProxyConfig;
pub use state::StateStore;

/// 一次启动尝试的失败原因：宿主需要区分「端口全被占」与「别的问题」，
/// 前者给出改端口入口，后者按普通故障展示。
#[derive(Debug)]
pub enum StartError {
    /// 期望端口起连续 20 个都被占用。
    PortExhausted { desired: u16, tried: Vec<u16> },
    /// 其它 I/O 错误（权限、地址非法……）。
    Io(std::io::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PortExhausted { desired, tried } => write!(
                f,
                "端口 {desired} 起连续 {} 个端口都被占用，请手动指定端口",
                tried.len()
            ),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StartError {}

impl From<std::io::Error> for StartError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// 组装应用状态（数据面 handler 与管理面进程内通道共用同一份）。
///
/// 唯一的 `AppState` 构造点：字段增减只改这里，保证两条入口面前
/// 是同一份状态、同一份契约。
pub fn build_state(cfg: ProxyConfig, store: StateStore) -> Arc<http::AppState> {
    Arc::new(http::AppState::new(cfg, store))
}

/// 运行中的反代句柄。持有监听地址与优雅退出通道。
pub struct ProxyHandle {
    addr: SocketAddr,
    /// 期望端口。与 `addr.port()` 不同即说明启动时发生过偏移。
    desired_port: u16,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
    /// 应用状态。宿主（ProxyService）另持一份给管理面进程内通道用；
    /// `stop()` 消费 self 后这份 Arc 仍可存活（无监听、无后台任务，残留无害）。
    state: Arc<http::AppState>,
}

impl ProxyHandle {
    /// 在 `cfg.host:cfg.port` 上启动监听；端口被占用时按 +1 逐个重试（最多 20 个）。
    ///
    /// `port = 0` 是特例：交给系统随机分配（测试用），不做偏移。
    pub async fn start(cfg: ProxyConfig, store: StateStore) -> Result<Self, StartError> {
        let state = build_state(cfg.clone(), store);
        Self::start_with_state(cfg, state).await
    }

    /// 同上，但复用外部已构建的 `AppState`（桌面端要另持一份给管理面通道）。
    pub async fn start_with_state(
        cfg: ProxyConfig,
        state: Arc<crate::http::AppState>,
    ) -> Result<Self, StartError> {
        let desired = cfg.port;
        if desired == 0 {
            return Self::bind(cfg, state, 0, 0).await;
        }
        let probe = ports::pick_port(desired, |port| bindable(&cfg.host, port))?;
        if probe.exhausted() && probe.actual == desired {
            return Err(StartError::PortExhausted {
                desired,
                tried: probe.occupied,
            });
        }
        Self::bind(cfg, state, probe.actual, desired).await
    }

    /// 直接在指定端口上绑定（不做偏移）。EADDRINUSE 原样返回给调用方，
    /// 由它决定要不要再走一轮偏移。
    async fn bind(
        cfg: ProxyConfig,
        state: Arc<crate::http::AppState>,
        port: u16,
        desired_port: u16,
    ) -> Result<Self, StartError> {
        let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), port)).await?;
        let addr = listener.local_addr()?;
        let app = http::router(state.clone());
        let (tx, rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
        });
        Ok(Self {
            addr,
            desired_port: if desired_port == 0 {
                addr.port()
            } else {
                desired_port
            },
            shutdown: Some(tx),
            join: Some(join),
            state,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 应用状态快照（桌面端构建管理面通道时用）。
    pub fn state(&self) -> &Arc<http::AppState> {
        &self.state
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 期望端口（配置里的值）。界面据此显示「8787 被占用，已在 8788 运行」。
    pub fn desired_port(&self) -> u16 {
        self.desired_port
    }

    /// 给状态接口用的一份快照。`engine` 恒为 `rust`（Python 版另有 `python`）。
    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "running": self.shutdown.is_some(),
            "baseUrl": self.base_url(),
            "port": self.addr.port(),
            "desiredPort": self.desired_port,
            "portShifted": self.addr.port() != self.desired_port,
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

/// 探测用的「先绑再放」：`ports::pick_port` 需要同步的可用性判断。
fn bindable(host: &str, port: u16) -> std::io::Result<bool> {
    match std::net::TcpListener::bind((host, port)) {
        Ok(listener) => {
            drop(listener);
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Ok(false),
        Err(error) => Err(error),
    }
}
