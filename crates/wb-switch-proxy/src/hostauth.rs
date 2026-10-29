//! 宿主令牌：桌面端进程内生成的短期凭据，让管理面在**不要求二次登录**的前提下
//! 仍然拒绝同机的其它程序。
//!
//! 背景（见 `docs/reverse-proxy-integration-plan.md` §3.3）：管理面原本只认
//! `ADMIN_KEY` 口令登录，桌面端用户已经登录过主程序，不该再输一次密码；
//! 但又不能「凡是来自 127.0.0.1 就放行」——同机任何进程都能访问回环地址。
//!
//! 做法：宿主启动反代时生成一个随机令牌（只在内存里），前端通过 Tauri `invoke`
//! 取到它并放在 `X-Proxy-Host-Token` 头里；服务端比对成功后按 `admin` 角色处理，
//! 不建会话、不下 cookie。
//!
//! 令牌**不落盘、不写日志**；进程退出即作废。

use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

pub const HOST_TOKEN_HEADER: &str = "x-proxy-host-token";

/// 宿主令牌的校验器。`None` 表示宿主没有启用令牌（例如独立测试或自建网关）。
#[derive(Clone, Debug, Default)]
pub struct HostAuth {
    token: Option<String>,
}

impl HostAuth {
    pub fn disabled() -> Self {
        Self { token: None }
    }

    pub fn with_token(token: impl Into<String>) -> Self {
        let token = token.into();
        Self {
            token: if token.trim().is_empty() { None } else { Some(token) },
        }
    }

    /// 从环境变量读（开发 / 排障用；桌面端走 `with_token`）。
    /// 太短的令牌直接忽略——那多半是误配，等于把管理面敞开。
    pub fn from_env() -> Self {
        match std::env::var("WB_PROXY_HOST_TOKEN") {
            Ok(value) if value.trim().len() >= 16 => Self::with_token(value.trim()),
            _ => Self::disabled(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.token.is_some()
    }

    /// 生成一个随机令牌（`wbh_` 前缀，便于在日志/抓包里认出来）。
    ///
    /// 用 `getrandom` 走系统熵源；`Sha256` 只是为了把它规整成定长十六进制，
    /// 不是拿时间当种子。
    pub fn random_token() -> String {
        let mut bytes = [0u8; 32];
        // 读不到系统熵源时不应该静默降级成弱随机：那会让管理面变成「谁都能猜」。
        // `getrandom` 在支持的平台上几乎不会失败，真失败了就让调用方看到 panic
        // 也比留一个可预测的令牌强。
        getrandom::getrandom(&mut bytes).expect("无法从系统熵源读取随机数");
        let digest = Sha256::digest(bytes);
        format!("wbh_{}", hex(&digest))
    }

    /// 校验请求是否来自宿主：头匹配 + 来源必须是回环地址。
    ///
    /// 来源判断刻意**要求**回环：局域网开放（`0.0.0.0`）时，外部机器即使猜中
    /// 令牌也不该以 admin 身份进管理面。
    pub fn authorize(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> bool {
        let Some(expected) = self.token.as_deref() else {
            return false;
        };
        let Some(candidate) = headers.get(HOST_TOKEN_HEADER).and_then(|value| value.to_str().ok())
        else {
            return false;
        };
        if !constant_time_eq(expected.trim(), candidate.trim()) {
            return false;
        }
        match peer {
            Some(ip) => ip.is_loopback(),
            // 拿不到来源（例如测试里没注入 ConnectInfo）时按「不在回环」处理：
            // 宁可多要一次登录，也不要在来源不明时给 admin。
            None => false,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 定长比较：不因为前缀匹配提前返回，避免用耗时侧信道逐字节猜令牌。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for index in 0..a.len() {
        diff |= a[index] ^ b[index];
    }
    diff == 0
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(HOST_TOKEN_HEADER, token.parse().unwrap());
        headers
    }

    fn loopback() -> Option<IpAddr> {
        Some(IpAddr::from([127, 0, 0, 1]))
    }

    #[test]
    fn accepts_matching_token_from_loopback() {
        let auth = HostAuth::with_token("wbh_abcdef0123456789");
        assert!(auth.authorize(&headers("wbh_abcdef0123456789"), loopback()));
    }

    #[test]
    fn rejects_wrong_token() {
        let auth = HostAuth::with_token("wbh_abcdef0123456789");
        assert!(!auth.authorize(&headers("wbh_0000000000000000"), loopback()));
    }

    #[test]
    fn rejects_non_loopback_even_with_correct_token() {
        let auth = HostAuth::with_token("wbh_abcdef0123456789");
        let remote = Some(IpAddr::from([192, 168, 1, 10]));
        assert!(!auth.authorize(&headers("wbh_abcdef0123456789"), remote));
    }

    #[test]
    fn rejects_when_peer_unknown() {
        let auth = HostAuth::with_token("wbh_abcdef0123456789");
        assert!(!auth.authorize(&headers("wbh_abcdef0123456789"), None));
    }

    #[test]
    fn rejects_when_disabled() {
        let auth = HostAuth::disabled();
        assert!(!auth.authorize(&headers(""), loopback()));
        assert!(!auth.enabled());
    }

    #[test]
    fn rejects_missing_header() {
        let auth = HostAuth::with_token("wbh_abcdef0123456789");
        assert!(!auth.authorize(&HeaderMap::new(), loopback()));
    }

    #[test]
    fn empty_token_is_treated_as_disabled() {
        assert!(!HostAuth::with_token("   ").enabled());
    }

    #[test]
    fn random_tokens_are_unique_and_prefixed() {
        let first = HostAuth::random_token();
        let second = HostAuth::random_token();
        assert!(first.starts_with("wbh_"));
        assert_ne!(first, second);
        assert_eq!(first.len(), 4 + 64);
    }

    #[test]
    fn constant_time_eq_matches_std_equality() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }
}
