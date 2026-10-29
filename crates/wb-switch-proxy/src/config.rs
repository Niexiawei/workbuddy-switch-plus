//! 运行参数：监听地址、数据目录、账号库位置。
//!
//! 默认值与 Python 版一致（`127.0.0.1:8787`、`~/.wb-switch-plus/reverse-proxy`），
//! 这样灰度期两个引擎可以指向同一份数据目录做只读对比，也可以各指一份互不干扰。

use std::path::PathBuf;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8787;
pub const DEFAULT_UPSTREAM: &str = "https://copilot.tencent.com";
pub const DEFAULT_BILLING: &str = "https://www.codebuddy.cn";

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub host: String,
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
        let host = std::env::var("WB_SERVE_HOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_string());
        let port = std::env::var("WB_SERVE_PORT")
            .ok()
            .and_then(|value| value.trim().parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);
        let home = wb_switch_core::modules::config::home_dir();
        Self {
            host,
            port,
            data_dir: home.join(".wb-switch-plus").join("reverse-proxy"),
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

    pub fn chat_url(&self) -> String {
        format!("{}/v2/chat/completions", self.upstream_base.trim_end_matches('/'))
    }

    pub fn state_path(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }
}
