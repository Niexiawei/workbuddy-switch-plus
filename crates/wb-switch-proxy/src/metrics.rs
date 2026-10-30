//! 请求指标（对齐 Python `admin/metrics.py` 的 `snapshot()` 契约）。
//!
//! 前端 `RpOverview.metrics` 消费这些字段；漏一个字段就是概览页白屏
//! （`m.recent.map` 在 `recent` 缺失时抛 TypeError，2026-10-30 实测踩到——
//! Rust 曾返回 `"metrics": {}` 空壳）。
//!
//! 与 Python 的差异：进程内存态，不落盘——重启清零，概览页文案已声明这一点。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const RECENT_MAX: usize = 100;

#[derive(Clone, Debug)]
struct RecentItem {
    time: i64,
    path: String,
    source: &'static str,
    status: u16,
    ok: bool,
    duration_ms: i64,
    outcome: &'static str,
}

#[derive(Default)]
struct Inner {
    total: u64,
    success: u64,
    http_success: u64,
    in_flight: u64,
    api_count: u64,
    test_count: u64,
    duration_sum_ms: u64,
    started_at: i64,
    recent: VecDeque<RecentItem>,
}

/// 线程安全的指标收集器。挂在 `AppState` 上，数据面 handler 在请求前后各调一次。
pub struct Metrics {
    inner: Mutex<Inner>,
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                started_at: now_seconds(),
                ..Inner::default()
            }),
        }
    }

    /// 请求开始（进上游前调用）。
    pub fn begin(&self) {
        self.inner.lock().unwrap().in_flight += 1;
    }

    /// 请求结束。`source`：`api`（数据面）或 `test`（管理面连接测试）。
    /// `outcome` 与前端 OUTCOMES 字典对齐：success / stream_error / interrupted / http_error。
    pub fn finish(
        &self,
        path: &str,
        source: &'static str,
        status: u16,
        ok: bool,
        duration_ms: u64,
        outcome: &'static str,
    ) {
        let mut inner = self.inner.lock().unwrap();
        inner.in_flight = inner.in_flight.saturating_sub(1);
        inner.total += 1;
        inner.duration_sum_ms += duration_ms;
        match source {
            "api" => inner.api_count += 1,
            _ => inner.test_count += 1,
        }
        if ok {
            inner.success += 1;
        }
        // HTTP 成功 = 2xx；流式错误/中断可能是 200 但没走完，单独记。
        if (200..300).contains(&status) && outcome == "success" {
            inner.http_success += 1;
        }
        inner.recent.push_front(RecentItem {
            time: now_seconds(),
            path: path.to_string(),
            source,
            status,
            ok,
            duration_ms: duration_ms as i64,
            outcome,
        });
        inner.recent.truncate(RECENT_MAX);
    }

    /// 与 Python `metrics.snapshot()` 逐字段对齐。
    pub fn snapshot(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap();
        let total = inner.total;
        let rate = |part: u64| {
            if total > 0 {
                serde_json::json!(round1(part as f64 * 100.0 / total as f64))
            } else {
                serde_json::Value::Null
            }
        };
        let avg = if total > 0 {
            serde_json::json!(inner.duration_sum_ms / total)
        } else {
            serde_json::Value::Null
        };
        let recent: Vec<serde_json::Value> = inner
            .recent
            .iter()
            .map(|item| {
                serde_json::json!({
                    "time": item.time,
                    "path": item.path,
                    "source": item.source,
                    "status": item.status,
                    "ok": item.ok,
                    "duration_ms": item.duration_ms,
                    "outcome": item.outcome,
                })
            })
            .collect();
        serde_json::json!({
            "started_at": inner.started_at,
            "completed": total,
            "in_flight": inner.in_flight,
            "succeeded": inner.success,
            "failed": total - inner.success,
            "success_rate": rate(inner.success),
            "http_success_rate": rate(inner.http_success),
            "avg_duration_ms": avg,
            "api_count": inner.api_count,
            "test_count": inner.test_count,
            "recent": recent,
        })
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_snapshot_matches_python_shape() {
        let metrics = Metrics::new();
        let snapshot = metrics.snapshot();
        // 逐字段核对：前端 RpMetrics 消费的字段一个都不能缺。
        for key in [
            "started_at",
            "completed",
            "in_flight",
            "succeeded",
            "failed",
            "success_rate",
            "http_success_rate",
            "avg_duration_ms",
            "api_count",
            "test_count",
            "recent",
        ] {
            assert!(snapshot.get(key).is_some(), "缺字段 {key}");
        }
        assert_eq!(snapshot["completed"], 0);
        assert!(
            snapshot["success_rate"].is_null(),
            "无请求时成功率应为 null"
        );
        assert!(snapshot["recent"].as_array().unwrap().is_empty());
    }

    #[test]
    fn finish_counts_api_and_test_separately() {
        let metrics = Metrics::new();
        metrics.begin();
        metrics.finish("/v1/chat/completions", "api", 200, true, 120, "success");
        metrics.finish("/admin/api/test", "test", 200, true, 500, "success");
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["completed"], 2);
        assert_eq!(snapshot["api_count"], 1);
        assert_eq!(snapshot["test_count"], 1);
        assert_eq!(snapshot["succeeded"], 2);
        assert_eq!(snapshot["in_flight"], 0);
    }

    #[test]
    fn recent_is_newest_first_and_capped() {
        let metrics = Metrics::new();
        for index in 0..150 {
            metrics.finish(&format!("/p{index}"), "api", 200, true, 10, "success");
        }
        let snapshot = metrics.snapshot();
        let recent = snapshot["recent"].as_array().unwrap();
        assert_eq!(recent.len(), 100, "recent 上限 100 条");
        assert_eq!(recent[0]["path"], "/p149", "最新在前");
        assert_eq!(recent[99]["path"], "/p50");
    }

    #[test]
    fn stream_error_counts_as_failed_but_not_http_success() {
        let metrics = Metrics::new();
        metrics.finish(
            "/v1/chat/completions",
            "api",
            200,
            false,
            300,
            "stream_error",
        );
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["succeeded"], 0);
        assert_eq!(snapshot["failed"], 1);
        // HTTP 200 但流式中断：不计入 HTTP 成功率。
        assert!(snapshot["http_success_rate"].as_f64().unwrap() == 0.0);
    }

    #[test]
    fn http_error_is_recorded() {
        let metrics = Metrics::new();
        metrics.finish("/v1/chat/completions", "api", 502, false, 40, "http_error");
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["failed"], 1);
        assert_eq!(snapshot["recent"][0]["status"], 502);
    }
}
