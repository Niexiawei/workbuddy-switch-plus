//! 运行参数：监听地址、数据目录、账号库位置。
//!
//! 默认值与 Python 版一致（`127.0.0.1:8787`、`~/.wb-switch-plus/reverse-proxy`），
//! 这样桌面端接管后老用户的客户端配置不用改。
//!
//! 端口的优先级见 `port_override()`：环境变量 > 宿主传入（`state.json` 里的用户设置）
//! > 默认 8787。实际监听端口可能因占用而偏移，那由 `crate::ports` 决定，
//! `ProxyConfig.port` 只表示**期望端口**。

use std::path::PathBuf;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8787;
pub const DEFAULT_UPSTREAM: &str = "https://copilot.tencent.com";
pub const DEFAULT_BILLING: &str = "https://www.codebuddy.cn";

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub host: String,
    /// **期望**端口。实际监听端口见 `ProxyHandle::addr()`（可能因占用而 +1）。
    pub port: u16,
    /// state.json / settings.key 所在目录。
    pub data_dir: PathBuf,
    /// 账号库（只读）。
    pub accounts_path: PathBuf,
    /// 上游基址。默认国内版；测试与自建网关可覆盖。
    pub upstream_base: String,
    /// billing 域基址（积分 / 签到 / 刷新），与 chat 域不同。
    pub billing_base: String,
}

impl ProxyConfig {
    /// 按当前用户环境推导默认配置。
    pub fn for_user() -> Self {
        Self {
            host: std::env::var("WB_SERVE_HOST")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_HOST.to_string()),
            port: port_override().unwrap_or(DEFAULT_PORT),
            data_dir: wb_switch_core::modules::config::home_dir()
                .join(".wb-switch-plus")
                .join("reverse-proxy"),
            accounts_path: wb_switch_core::modules::config::accounts_file(),
            billing_base: std::env::var("WB_PROXY_BILLING")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_BILLING.to_string()),
            upstream_base: std::env::var("WB_PROXY_UPSTREAM")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_UPSTREAM.to_string()),
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    pub fn with_data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = dir.into();
        self
    }

    pub fn with_accounts(mut self, path: impl Into<PathBuf>) -> Self {
        self.accounts_path = path.into();
        self
    }

    pub fn with_upstream(mut self, base: impl Into<String>) -> Self {
        self.upstream_base = base.into();
        self
    }

    /// 覆盖 billing 域（积分 / 签到 / 刷新 / 活跃上报）。
    ///
    /// 任务层必须走这里而不是另建一份 `for_user()`：否则测试注入的 mock 地址、
    /// 以及用户自建网关的配置，都会在动作路径上被悄悄丢掉。
    pub fn with_billing_base(mut self, base: impl Into<String>) -> Self {
        self.billing_base = base.into();
        self
    }

    pub fn chat_url(&self) -> String {
        format!(
            "{}/v2/chat/completions",
            self.upstream_base.trim_end_matches('/')
        )
    }

    pub fn state_path(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }
}

/// 环境变量里的端口覆盖（开发与排障用）。`0` 与非法值一律忽略——端口 0 会让系统
/// 随机分配，那对「客户端要写死一个地址」的场景毫无意义。
pub fn port_override() -> Option<u16> {
    std::env::var("WB_SERVE_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|value| *value > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        assert_eq!(DEFAULT_HOST, "127.0.0.1");
        assert_eq!(DEFAULT_PORT, 8787);
    }

    #[test]
    fn state_path_is_inside_data_dir() {
        let cfg = ProxyConfig::for_user().with_data_dir("/tmp/wb-proxy-test");
        assert!(cfg.state_path().ends_with("state.json"));
        assert!(cfg.state_path().starts_with("/tmp/wb-proxy-test"));
    }

    #[test]
    fn builder_overrides_port_and_host() {
        let cfg = ProxyConfig::for_user().with_port(9000).with_host("0.0.0.0");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.host, "0.0.0.0");
    }
}
