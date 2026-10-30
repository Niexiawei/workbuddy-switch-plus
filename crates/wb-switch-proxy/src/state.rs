//! `state.json` 的只读打开与默认值补齐。
//!
//! P1 只读：Python 仍是唯一写者（双跑期），Rust 读同一份文件、补齐缺失键后**不落盘**。
//! 这样两边都能跑，又不会出现两个进程同时写同一个 JSON 的经典丢数据。
//!
//! 写能力用 `open_writable` 显式开启（测试与 P4 之后的单写者阶段使用）。

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// state.json 必须存在的键。与 Python 版逐字一致（见 docs/reverse-proxy-contract.md）。
pub const STATE_KEYS: &[&str] = &[
    "keys", "users", "api_tokens", "audit_logs", "security", "ip_rules", "ip_access_logs",
    "ip_last_allowed", "api_key_ips", "account_status", "account_overrides", "session_bindings",
    "active", "accounts", "pool", "groups", "egress", "alerts", "usage_stats", "checkin_logs",
    "travel_states", "runtime_since", "runtime_total_sec", "runtime_sessions", "model_aliases",
    "secret", "settings",
];

/// 计数类键的默认值是数字，日志类键是**数组**（Python 侧为 deque/list，前端
/// 对它们直接 `.map`），其余是对象。默认值给错会让下游 `as_object()` 拿到 None，
/// 或者前端拿到 `{}` 对它 `.map` 直接崩（2026-10-30 概览/账号/IP 安全/密钥页
/// 白屏的根因）。
fn default_for(key: &str) -> Value {
    match key {
        "runtime_since" | "runtime_total_sec" | "runtime_sessions" => json!(0),
        // 日志类：Python 侧是数组（pool.py / security.py / server.py 里均为 []）。
        // request_logs 是运行时内存（pool.request_logs），不落 state.json，故不在表内。
        "checkin_logs" | "audit_logs" | "ip_access_logs" => json!([]),
        _ => json!({}),
    }
}

pub struct StateStore {
    path: PathBuf,
    data: std::sync::RwLock<Value>,
    writable: bool,
}

/// 日志类键在 state.json 里必须是**数组**。早前版本的默认值表把它们写成 `{}`，
/// 已经落盘的文件需要就地纠正——只补缺不纠型的话，坏类型会一直留着。
const ARRAY_KEYS: &[&str] = &["checkin_logs", "audit_logs", "ip_access_logs"];

impl StateStore {
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        Self::load(path.into(), false)
    }

    pub fn open_writable(path: impl Into<PathBuf>) -> io::Result<Self> {
        Self::load(path.into(), true)
    }

    fn load(path: PathBuf, writable: bool) -> io::Result<Self> {
        let mut data = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({})),
            Err(error) if error.kind() == io::ErrorKind::NotFound => json!({}),
            Err(error) => return Err(error),
        };
        if !data.is_object() {
            data = json!({});
        }
        let object: &mut Map<String, Value> = data.as_object_mut().expect("object");
        for key in STATE_KEYS {
            object.entry((*key).to_string()).or_insert_with(|| default_for(key));
        }
        Self::normalize(object);
        Ok(Self { path, data: std::sync::RwLock::new(data), writable })
    }

    /// 把类型不对的键纠正为契约形态（幂等，只在加载与写前重读时跑）。
    fn normalize(object: &mut Map<String, Value>) {
        for key in ARRAY_KEYS {
            if let Some(value) = object.get_mut(*key) {
                if !value.is_array() {
                    *value = json!([]);
                }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    /// 返回当前状态的快照（克隆）。调用方不能长期持有，避免阻塞写锁。
    pub fn snapshot(&self) -> Value {
        self.data.read().unwrap().clone()
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.data.read().unwrap().get(key).cloned()
    }

    /// 原子地改一处状态并落盘。只读实例（灰度期）返回 PermissionDenied。
    pub fn update<F>(&self, apply: F) -> io::Result<()>
    where
        F: FnOnce(&mut Value),
    {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Rust 引擎当前是只读模式（灰度期由 Python 写状态）",
            ));
        }
        let mut guard = self.data.write().unwrap();
        // 写前先与磁盘对齐：灰度期 Python 服务可能还在写同一份 state.json，
        // 而这里是「整份内存 → 整份落盘」。不重读就会把对方刚写进去的
        // account_status / 密钥一次性覆盖掉（经典丢数据）。
        // 每次写都重读的代价可以接受：写操作只有密钥 CRUD、池设置、账号动作这些低频路径。
        self.reload_from_disk(&mut guard);
        apply(&mut guard);
        let content = serde_json::to_string_pretty(&*guard)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        wb_switch_core::modules::config::atomic_write(&self.path, &content)
    }

    /// 写回磁盘。只读实例会拒绝——这是有意的：灰度期 Python 是唯一写者。
    pub fn save(&self) -> io::Result<()> {
        self.update(|_| {})
    }

    /// 把磁盘上的最新内容读进内存；文件不存在或损坏时保持当前内存值。
    ///
    /// 只补齐缺失键、不丢已有键：磁盘上被别的进程删掉的键会跟着消失，这是本意——
    /// 删除也是对方的写入意图。
    fn reload_from_disk(&self, guard: &mut Value) {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return };
        let Ok(mut disk) = serde_json::from_str::<Value>(&text) else { return };
        let Some(object) = disk.as_object_mut() else { return };
        for key in STATE_KEYS {
            object.entry((*key).to_string()).or_insert_with(|| default_for(key));
        }
        Self::normalize(object);
        *guard = disk;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_keys_default_to_empty_arrays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        // 不写文件：全走默认值。日志类键必须是数组，否则前端 .map 崩。
        let store = StateStore::open(&path).unwrap();
        for key in ARRAY_KEYS {
            assert!(
                store.get(key).unwrap().is_array(),
                "默认值应为数组：{key}"
            );
        }
        // 对照：普通键仍是对象。
        assert!(store.get("keys").unwrap().is_object());
    }

    #[test]
    fn legacy_object_typed_log_keys_are_corrected_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        // 模拟早前版本落盘的坏数据：日志键被写成 {}。
        std::fs::write(
            &path,
            json!({"checkin_logs": {}, "audit_logs": {}, "ip_access_logs": {}, "request_logs": {}}).to_string(),
        )
        .unwrap();
        let store = StateStore::open_writable(&path).unwrap();
        for key in ARRAY_KEYS {
            assert!(
                store.get(key).unwrap().is_array(),
                "加载时应纠偏为数组：{key}"
            );
        }
        // 纠偏要落盘：下次读取（包括 Python 侧）看到的也是正确类型。
        store.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let disk: Value = serde_json::from_str(&text).unwrap();
        for key in ARRAY_KEYS {
            assert!(disk[key].is_array(), "落盘后应为数组：{key}");
        }
    }

    #[test]
    fn real_array_content_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            json!({"audit_logs": [{"time": 1, "action": "x"}]}).to_string(),
        )
        .unwrap();
        let store = StateStore::open(&path).unwrap();
        let logs = store.get("audit_logs").unwrap();
        assert_eq!(logs.as_array().unwrap().len(), 1, "已有内容不能被清掉");
        assert_eq!(logs[0]["action"], "x");
    }
}
