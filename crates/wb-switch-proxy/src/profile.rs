//! 客户端指纹：区分 CodeBuddy CLI 与桌面端，并合并出站请求头。
//!
//! 与 Python 版 `core/client_profile.py` 一一对应：UA 版本段随 CLI 升级变化，
//! 不能硬编码；鉴权与身份头始终以账号为准，客户端不能覆盖。

use std::collections::BTreeMap;

pub const CLI: &str = "cli";
pub const DESKTOP: &str = "desktop";

/// CLI 原样透传给上游的头（小写）。顺序无关。
pub const CLI_HEADER_PASSTHROUGH: &[&str] = &[
    "user-agent",
    "x-product",
    "x-ide-type",
    "x-ide-name",
    "x-ide-version",
    "x-requested-with",
    "x-codebuddy-request",
];

/// CLI 独有、必须透传的 body 字段。
pub const CLI_EXTRA_BODY_KEYS: &[&str] = &["extra_vars", "cache_retention"];

/// 反代必须保留、客户端不允许覆盖的头。
const RESERVED: &[&str] = &[
    "authorization", "content-type", "accept", "x-user-id", "x-enterprise-id",
    "x-tenant-id", "x-domain",
];

/// 归一化外部传入的头名：小写 + 去空白。调用方不必自己转。
pub fn lower_headers<'a, I>(pairs: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    pairs
        .into_iter()
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect()
}

pub fn detect(headers: &BTreeMap<String, String>) -> &'static str {
    if headers.get("x-ide-type").map(|value| value.eq_ignore_ascii_case(CLI)) == Some(true) {
        return CLI;
    }
    if headers
        .get("user-agent")
        .map(|value| value.trim_start().to_ascii_lowercase().starts_with("cli/"))
        == Some(true)
    {
        return CLI;
    }
    DESKTOP
}

/// 按 profile 合成真正发往上游的头。
///
/// 桌面端保持旧行为（桌面 UA + 账号身份头）；CLI 流量把客户端自己的指纹头带上，
/// 但鉴权与身份头仍以账号为准——这几个字段决定「用哪个号、算谁的额度」。
pub fn merge_outbound_headers(
    mut base: BTreeMap<String, String>,
    incoming: &BTreeMap<String, String>,
    profile: &str,
) -> BTreeMap<String, String> {
    if profile != CLI {
        return base;
    }
    for name in CLI_HEADER_PASSTHROUGH {
        if RESERVED.contains(name) {
            continue;
        }
        if let Some(value) = incoming.get(*name) {
            if !value.is_empty() {
                base.insert((*name).to_string(), value.clone());
            }
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incoming() -> BTreeMap<String, String> {
        lower_headers([
            ("User-Agent", "CLI/2.177.1 CodeBuddy/2.177.1"),
            ("X-Product", "SaaS"),
            ("X-IDE-Type", "CLI"),
            ("X-IDE-Version", "2.177.1"),
            ("X-User-Id", "anonymous"),
            ("X-Domain", "evil.example"),
            ("Authorization", "Bearer client"),
        ])
    }

    #[test]
    fn detects_cli_by_header_or_ua() {
        assert_eq!(detect(&incoming()), CLI);
        let ua_only = lower_headers([("User-Agent", "cli/2.177.1")]);
        assert_eq!(detect(&ua_only), CLI);
        let desktop = lower_headers([("User-Agent", "WorkBuddy/5.5.4 WorkBuddy/5.5.4 CLI/2.137.1")]);
        assert_eq!(detect(&desktop), DESKTOP);
    }

    #[test]
    fn cli_passthrough_keeps_account_identity() {
        let base = lower_headers([
            ("user-agent", "codebuddy2openai/2.0"),
            ("authorization", "Bearer account"),
            ("x-user-id", "uid-1"),
            ("x-domain", "www.codebuddy.cn"),
        ]);
        let merged = merge_outbound_headers(base, &incoming(), CLI);
        assert_eq!(merged["user-agent"], "CLI/2.177.1 CodeBuddy/2.177.1");
        assert_eq!(merged["x-product"], "SaaS");
        assert_eq!(merged["authorization"], "Bearer account");
        assert_eq!(merged["x-user-id"], "uid-1");
        assert_eq!(merged["x-domain"], "www.codebuddy.cn");
        assert_eq!(merged.len(), 7, "不能出现重复 UA");
    }

    #[test]
    fn desktop_profile_is_untouched() {
        let base = lower_headers([("user-agent", "codebuddy2openai/2.0")]);
        let merged = merge_outbound_headers(base.clone(), &incoming(), DESKTOP);
        assert_eq!(merged, base);
    }
}
