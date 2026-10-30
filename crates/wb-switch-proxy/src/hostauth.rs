//! 来源识别：从连接信息拆出对端 IP，供数据面密钥白名单（`crate::keys`）判定。
//!
//! 曾经这里还有一整套「宿主令牌」体系（`HostAuth` / `X-Proxy-Host-Token`），
//! 用于管理面离开登录形态后的 HTTP 鉴权；管理面整体退出 HTTP（改走桌面端
//! 进程内通道，见 docs/reverse-proxy-internal-plane-plan.md 附录 C）后，
//! 令牌随之退役，本模块只剩对数据面有意义的 peer 拆包。

use std::net::{IpAddr, SocketAddr};

/// 从 `ConnectInfo` 里取来源 IP；缺失时返回 `None`（调用方据此保守处理）。
pub fn peer_ip(addr: Option<SocketAddr>) -> Option<IpAddr> {
    addr.map(|value| value.ip())
}

/// axum `ConnectInfo` 版本：数据面 handler 以 extractor 形式拿到它，
/// 这里统一拆出 IP，避免每个 handler 都写一遍 `.map(|ConnectInfo(addr)| addr.ip())`。
///
/// 用 `Option<ConnectInfo<..>>` 而不是裸的 `ConnectInfo`：路由单测走
/// `Router::oneshot` 时不经过真实监听，没有连接信息可注入，裸 extractor 会直接 500。
pub fn peer_from_connect(
    connect: Option<axum::extract::ConnectInfo<SocketAddr>>,
) -> Option<IpAddr> {
    peer_ip(connect.map(|axum::extract::ConnectInfo(addr)| addr))
}
