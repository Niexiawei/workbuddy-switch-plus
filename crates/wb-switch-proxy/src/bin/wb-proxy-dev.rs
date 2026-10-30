//! 独立运行的反代测试入口（仅本地验证用，**不随安装包分发**）。
//!
//! 用途：在不启动完整桌面端的情况下，用 curl 验证反代的数据面与管理面。
//! 与正式形态的区别：这里由命令行参数直接给定端口与数据目录，不读
//! `state.json` 的端口设置，也不做自动启动。
//!
//! 用法：
//! ```text
//! cargo build -p wb-switch-proxy --bin wb-proxy-dev --release
//! target/release/wb-proxy-dev.exe --port 8790 --data D:/tmp/wb-proxy-test
//! ```

use std::path::PathBuf;

use wb_switch_proxy::hostauth::HostAuth;
use wb_switch_proxy::{ProxyConfig, ProxyHandle, StateStore};

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(item) = args.next() {
        if item == name {
            return args.next();
        }
        if let Some(value) = item.strip_prefix(&format!("{name}=")) {
            return Some(value.to_string());
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = ProxyConfig::for_user();
    if let Some(port) = arg("--port").and_then(|value| value.parse::<u16>().ok()) {
        cfg.port = port;
    }
    if let Some(data) = arg("--data") {
        cfg.data_dir = PathBuf::from(data);
    }
    if let Some(accounts) = arg("--accounts") {
        cfg.accounts_path = PathBuf::from(accounts);
    }
    std::fs::create_dir_all(&cfg.data_dir)?;

    // 独立测试默认**可写**：数据目录由 --data 隔离，不会碰到桌面端的状态。
    let store = StateStore::open_writable(cfg.state_path())?;

    // 宿主令牌：测试时打印出来，方便 curl -H X-Proxy-Host-Token。
    let token = HostAuth::random_token();
    let handle = ProxyHandle::start_with_auth(cfg, store, HostAuth::with_token(&token)).await?;

    println!("wb-proxy-dev listening on {}", handle.base_url());
    println!("  data dir : {}", arg("--data").unwrap_or_else(|| "(default)".into()));
    println!("  host token: {token}");
    println!("  curl 例:");
    println!("    curl {}/health", handle.base_url());
    println!(
        "    curl -H \"X-Proxy-Host-Token: {token}\" {}/admin/api/session",
        handle.base_url()
    );
    tokio::signal::ctrl_c().await?;
    handle.stop().await;
    Ok(())
}
