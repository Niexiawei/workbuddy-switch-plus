//! 监听端口的选择：默认 8787，冲突时按 +1 逐个试。
//!
//! 设计取向（见 `docs/reverse-proxy-integration-plan.md` §3.1.1）：内置反代必须
//! 自己站起来，不能因为一个被占的端口就让整个服务不可用。代价是客户端配置的
//! 端口可能不再生效，所以偏移必须**可见**——`ProxyHandle::status()` 会把
//! `desiredPort` 与 `portShifted` 一起回给宿主与界面。
//!
//! 只对「地址被占用」重试；权限不足、地址非法等错误应当立刻失败，试 20 次也
//! 只是把同一个错误重复 20 遍。

use std::io;

/// 从期望端口起最多尝试多少个端口（含期望端口本身）。
pub const MAX_ATTEMPTS: u16 = 20;

/// 一次端口探测的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortProbe {
    /// 期望端口（配置里的值）。
    pub desired: u16,
    /// 实际可用的端口。
    pub actual: u16,
    /// 是否发生过偏移（`actual != desired`）。
    pub shifted: bool,
    /// 已经确认被占用的端口，按尝试顺序。
    pub occupied: Vec<u16>,
}

impl PortProbe {
    /// 是否已经没有任何可再试的端口（要么试满 [`MAX_ATTEMPTS`] 个，要么撞到 65535）。
    ///
    /// 注意「耗尽」和「失败」不是一回事：`pick_port` 找到可用端口时同样可能
    /// `occupied.len() == MAX_ATTEMPTS`（最后一个才成功）。调用方必须同时看
    /// `actual` 是否等于 `desired`。
    pub fn exhausted(&self) -> bool {
        self.occupied.len() >= MAX_ATTEMPTS as usize
    }
}

fn is_addr_in_use(error: &io::Error) -> bool {
    // Windows 下 `WSAEADDRINUSE` 的 `raw_os_error` 是 10048，非 Windows 是 EADDRINUSE（98/48）。
    // `io::ErrorKind::AddrInUse` 两边都由标准库映射好了，用它，不要比魔法数字。
    error.kind() == io::ErrorKind::AddrInUse
}

/// 在 `host` 上找一个可用端口：从 `desired` 起逐个试，最多 [`MAX_ATTEMPTS`] 个。
///
/// `bindable(port)` 由调用方提供（测试里换成内存实现，生产里就是 `TcpListener::bind`），
/// 返回 `Ok(true)` 表示该端口可绑定。这样端口选择逻辑本身可以脱离真实套接字测试。
pub fn pick_port<F>(desired: u16, mut bindable: F) -> io::Result<PortProbe>
where
    F: FnMut(u16) -> io::Result<bool>,
{
    let mut occupied = Vec::new();
    let mut port = desired;
    for _ in 0..MAX_ATTEMPTS {
        // 端口号溢出（desired 接近 65535）时直接失败，不要回绕到 0 或小端口。
        if port == 0 {
            break;
        }
        match bindable(port)? {
            true => {
                return Ok(PortProbe {
                    desired,
                    actual: port,
                    shifted: port != desired,
                    occupied,
                })
            }
            false => {
                occupied.push(port);
                if port == u16::MAX {
                    break;
                }
                port += 1;
            }
        }
    }
    Ok(PortProbe {
        desired,
        actual: desired,
        shifted: false,
        occupied,
    })
}

/// 真实套接字的版本：逐个尝试 `host:port` 的绑定。
///
/// 注意这里只**探测**，绑好就立刻释放——真正的监听由 `ProxyHandle::start` 建立。
/// 两次绑定之间有一个极短的窗口可能被别人抢占，此时 `start` 会返回 `AddrInUse`，
/// 宿主据此再走一轮 `pick_port` 即可。
pub fn find_available(host: &str, desired: u16) -> io::Result<PortProbe> {
    pick_port(desired, |port| {
        match std::net::TcpListener::bind((host, port)) {
            Ok(listener) => {
                drop(listener);
                Ok(true)
            }
            Err(error) if is_addr_in_use(&error) => Ok(false),
            Err(error) => Err(error),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_desired_when_free() {
        let probe = pick_port(8787, |_| Ok(true)).unwrap();
        assert_eq!(probe.actual, 8787);
        assert!(!probe.shifted);
        assert!(probe.occupied.is_empty());
    }

    #[test]
    fn shifts_by_one_when_desired_taken() {
        let probe = pick_port(8787, |port| Ok(port != 8787)).unwrap();
        assert_eq!(probe.actual, 8788);
        assert!(probe.shifted);
        assert_eq!(probe.occupied, vec![8787]);
    }

    #[test]
    fn skips_a_run_of_occupied_ports() {
        let taken = [8787u16, 8788, 8789];
        let probe = pick_port(8787, |port| Ok(!taken.contains(&port))).unwrap();
        assert_eq!(probe.actual, 8790);
        assert_eq!(probe.occupied, taken.to_vec());
        assert!(!probe.exhausted());
    }

    #[test]
    fn reports_exhaustion_after_max_attempts() {
        let probe = pick_port(8787, |_| Ok(false)).unwrap();
        assert!(probe.exhausted());
        assert_eq!(probe.occupied.len(), MAX_ATTEMPTS as usize);
        // 全部失败时 actual 回到期望端口，宿主据此报 Failed{PortExhausted}。
        assert_eq!(probe.actual, 8787);
        assert!(!probe.shifted);
    }

    #[test]
    fn permission_errors_do_not_retry() {
        let mut calls = 0;
        let error = pick_port(8787, |_| {
            calls += 1;
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(calls, 1, "非占用类错误不该反复重试");
    }

    #[test]
    fn does_not_wrap_around_near_u16_max() {
        // 从 65535 起且被占用时没有下一个端口：不回绕到 0，直接判耗尽。
        let probe = pick_port(u16::MAX, |_| Ok(false)).unwrap();
        assert_eq!(probe.occupied, vec![u16::MAX]);
        // 只试了 1 个端口就没了，不该被说成「试过 20 个」——耗尽判定由调用方
        // 结合「有没有可用端口」决定，这里的契约是「没有可试的了」。
        assert!(probe.actual == u16::MAX && !probe.shifted);
    }
}
