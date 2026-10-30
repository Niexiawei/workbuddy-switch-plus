//! 独立运行的反代测试入口（仅本地验证用，**不随安装包分发**）。
//!
//! 用途：在不启动完整桌面端的情况下，用 curl 验证反代的**数据面**。
//! 管理面不在 HTTP 上（进程内通道，见 docs/reverse-proxy-internal-plane-plan.md
//! 附录 C），因此这里只挂数据面 router，也不再有任何令牌。
//!
//! 用法：
//! ```text
//! cargo build -p wb-switch-proxy --bin wb-proxy-dev --release
//! target/release/wb-proxy-dev.exe --port 8790 --data D:/tmp/wb-proxy-test
//! ```

use std::path::PathBuf;

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
    if let Some(host) = arg("--host") {
        cfg.host = host;
    }
    std::fs::create_dir_all(&cfg.data_dir)?;

    // 独立测试默认**可写**：数据目录由 --data 隔离，不会碰到桌面端的状态。
    let store = StateStore::open_writable(cfg.state_path())?;
    let handle = ProxyHandle::start(cfg, store).await?;

    println!(
        "wb-proxy-dev (data plane only) listening on {}",
        handle.base_url()
    );
    println!(
        "  data dir : {}",
        arg("--data").unwrap_or_else(|| "(default)".into())
    );
    println!("  curl 例:");
    println!("    curl {}/health", handle.base_url());
    println!(
        "    curl -H \"Authorization: Bearer <网关密钥>\" {}/v1/models",
        handle.base_url()
    );
    tokio::signal::ctrl_c().await?;
    handle.stop().await;
    Ok(())
}
