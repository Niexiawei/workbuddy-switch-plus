//! 独立运行的 Rust 反代（灰度对比 / 无桌面端场景）。
//!
//! 桌面端会把引擎托管在 Tauri 进程里；这个 bin 只是同一个 \`ProxyHandle\` 的
//! 命令行外壳，方便与 Python 版双端口对比。
//!
//! 用法：
//!   cargo run -p wb-switch-proxy --bin wb-proxy -- --port 8788 --data ../reverse-proxy/data

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
    if let Some(upstream) = arg("--upstream") {
        cfg.upstream_base = upstream;
    }
    std::fs::create_dir_all(&cfg.data_dir)?;
    // 默认只读（灰度期 Python 是唯一写者）；`WB_PROXY_WRITABLE=1` 时接管写入，
    // 必须配合独立的数据目录使用，否则会和 Python 互相覆盖。
    let writable = std::env::var("WB_PROXY_WRITABLE").map(|value| value == "1").unwrap_or(false);
    let store = if writable {
        StateStore::open_writable(cfg.state_path())?
    } else {
        StateStore::open(cfg.state_path())?
    };
    let handle = ProxyHandle::start(cfg, store).await?;
    println!("wb-proxy (rust) listening on {}", handle.base_url());
    println!("  state writable: {}", if writable { "yes" } else { "no (grey release: python is the writer)" });
    tokio::signal::ctrl_c().await?;
    handle.stop().await;
    Ok(())
}
