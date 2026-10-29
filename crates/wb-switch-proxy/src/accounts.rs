//! 账号库的只读视图。
//!
//! 账号的增删、登录、刷新全部由 `wb-switch-core` 与桌面端负责；反代只读，
//! 与 Python 版的 `switch_store` 同一边界（`WB_SWITCH_WRITE_BACK=1` 才回写，
//! 那属于 token 刷新路径，P4 再谈）。

use std::path::Path;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct AccountView {
    pub id: String,
    pub uid: String,
    pub nickname: String,
    /// `cn` / `ai`
    pub variant: String,
    pub domain: String,
    pub has_token: bool,
    /// 主项目账号库里已被标记为「需重新登录」（refresh token 失效 / 缺少 refresh token）。
    ///
    /// 该标记由桌面端的 `refresh` 模块写进 `accounts.json`，是主项目对「这个账号还能不能
    /// 用」的权威判断。反代必须跟着走：只看 `access_token` 非空就选号，会把桌面端已经判定
    /// 失效的账号继续派活 —— 用户看到的就是「账户管理里写着需重新登录，反代页却一切正常」。
    pub needs_relogin: bool,
}

pub fn list_at(path: &Path) -> Vec<AccountView> {
    wb_switch_core::modules::account::load_accounts_at(path)
        .iter()
        .filter_map(from_value)
        .collect()
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

fn from_value(value: &Value) -> Option<AccountView> {
    let uid = text(value, "uid");
    let id = text(value, "id");
    if uid.is_empty() && id.is_empty() {
        return None;
    }
    let domain = text(value, "domain");
    let variant = match text(value, "variant").as_str() {
        "ai" => "ai".to_string(),
        "cn" => "cn".to_string(),
        _ if domain.ends_with("workbuddy.ai") => "ai".to_string(),
        _ => "cn".to_string(),
    };
    let has_token = value
        .get("access_token")
        .map(|token| match token {
            Value::String(text) => !text.trim().is_empty(),
            Value::Null => false,
            _ => true,
        })
        .unwrap_or(false);
    Some(AccountView {
        id: if id.is_empty() { uid.clone() } else { id },
        uid,
        nickname: text(value, "nickname"),
        variant,
        domain,
        has_token,
        needs_relogin: value.get("needs_relogin").and_then(Value::as_bool) == Some(true),
    })
}

/// 取第一个有明文 token 的账号（池化之前的「单账号」策略）。
/// 加密信封（$wbEncrypted）跳过：解密属 P4，目录调用可以先退到本机/种子。
pub fn first_with_token(path: &Path) -> Option<(AccountView, String)> {
    let views = list_at(path);
    let raw = wb_switch_core::modules::account::load_accounts_at(path);
    for view in views {
        if !view.has_token {
            continue;
        }
        let record = raw.iter().find(|row| {
            row.get("uid").and_then(Value::as_str) == Some(view.uid.as_str())
                || row.get("id").and_then(Value::as_str) == Some(view.id.as_str())
        })?;
        if let Some(Value::String(token)) = record.get("access_token") {
            let token = token.trim();
            if !token.is_empty() {
                return Some((view, token.to_string()));
            }
        }
    }
    None
}
