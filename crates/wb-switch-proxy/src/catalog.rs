//! 模型目录：上游 /v3/config + 本机产品配置 + 国内版种子。
//!
//! 与 Python 版 converter.get_model_catalog_for + model_metadata_entry 对齐：
//! 远端优先、本机补缺、种子兜底；能力字段只报目录明确声明的
//! （disabledMultimodal 优先于 supportsImages）。

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::accounts;
use crate::http::AppState;

/// 国内版种子：远端与本机都拿不到时的兜底（只有 id 与上下文窗口是已知的）。
pub const SEED_MODELS: &[(&str, i64)] = &[
    ("deepseek-v4-pro", 1_000_000),
    ("deepseek-v4-flash", 1_000_000),
    ("glm-5.3", 1_000_000),
    ("glm-5.3-flash", 1_000_000),
    ("minimax-m3", 512_000),
    ("kimi-k3-1", 1_000_000),
    ("hy3", 192_000),
    ("hunyuan-chat", 200_000),
];

const DEFAULT_CONTEXT_WINDOW: i64 = 200_000;
const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 32_768;
const TTL: Duration = Duration::from_secs(60);

const NON_CHAT_TAGS: &[&str] = &["text-to-image", "image-to-image", "text-to-video", "image-to-video"];
const NON_CHAT_ID_WORDS: &[&str] = &["completion", "rewrite", "jump", "codewise"];

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("catalog client")
    })
}

fn is_chat_model(entry: &Value) -> bool {
    let Some(id) = entry.get("id").and_then(Value::as_str) else { return false };
    if id.is_empty() {
        return false;
    }
    let tags = entry.get("tags").and_then(Value::as_array);
    if tags.is_some_and(|tags| {
        tags.iter().any(|tag| {
            let text = tag.as_str().unwrap_or("").to_ascii_lowercase();
            NON_CHAT_TAGS.iter().any(|needle| text.contains(needle))
        })
    }) {
        return false;
    }
    if entry.get("vendor").and_then(Value::as_str) == Some("tencent") {
        return false;
    }
    if id.starts_with("custom-local:") || entry.get("vendor").and_then(Value::as_str) == Some("Custom") {
        return false;
    }
    let lower = id.to_ascii_lowercase();
    !NON_CHAT_ID_WORDS.iter().any(|word| lower.contains(word))
}

fn context_window_of(entry: &Value) -> i64 {
    if let Some(value) = entry.get("maxInputTokens").and_then(Value::as_i64) {
        if value > 1_000 {
            return value;
        }
    }
    if let Some(value) = entry
        .get("contextWindow")
        .and_then(|context| context.get("defaultLength"))
        .and_then(Value::as_i64)
    {
        if value > 1_000 {
            return value;
        }
    }
    DEFAULT_CONTEXT_WINDOW
}

fn reasoning_efforts(entry: &Value) -> Vec<String> {
    if entry.get("supportsReasoning").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    entry
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("supportedEfforts"))
        .and_then(Value::as_array)
        .map(|levels| {
            levels.iter().filter_map(Value::as_str).filter(|level| !level.is_empty())
                .map(str::to_string).collect()
        })
        .unwrap_or_default()
}

/// 目录条目 → 模型对象（OpenAI 字段 + 能力扩展）。
pub fn metadata_of(entry: &Value, source: &str) -> Value {
    let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
    let supports_images = entry.get("supportsImages").and_then(Value::as_bool) == Some(true)
        && entry.get("disabledMultimodal").and_then(Value::as_bool) != Some(true);
    json!({
        "id": id,
        "object": "model",
        "created": 1_700_000_000i64,
        "owned_by": "codebuddy",
        "name": entry.get("name").and_then(Value::as_str).unwrap_or(id),
        "context_window": context_window_of(entry),
        "max_output_tokens": entry.get("maxOutputTokens").and_then(Value::as_i64)
            .filter(|value| *value > 0).unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        "input_modalities": if supports_images { json!(["text", "image"]) } else { json!(["text"]) },
        "supports_tool_call": entry.get("supportsToolCall").and_then(Value::as_bool).unwrap_or(true),
        "reasoning_efforts": reasoning_efforts(entry),
        "tags": entry.get("tags").cloned().unwrap_or_else(|| json!([])),
        "source": source,
    })
}

fn models_array(payload: &Value) -> Option<&Vec<Value>> {
    if let Some(array) = payload.as_array() {
        return Some(array);
    }
    payload
        .get("models")
        .and_then(Value::as_array)
        .or_else(|| payload.get("data").and_then(|data| data.get("models")).and_then(Value::as_array))
        .or_else(|| payload.get("data").and_then(Value::as_array))
}

/// 解析上游 /v3/config（也兼容 /console/.../models 的信封）。
pub fn parse_catalogue(text: &str) -> Vec<Value> {
    let Ok(value) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    let payload = value.get("data").unwrap_or(&value);
    models_array(payload)
        .map(|items| items.iter().filter(|entry| is_chat_model(entry)).cloned().collect())
        .unwrap_or_default()
}

fn cached_product_config() -> Option<PathBuf> {
    // 客户端从服务端拉取的动态配置：取版本号最大的那个（同版本取 mtime 最新）。
    let root = std::env::var_os("WORKBUDDY_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| wb_switch_core::modules::config::home_dir().join(".workbuddy"));
    let cache_dir = root.join("cache");
    let mut best: Option<(u64, std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(cache_dir).ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("acc-product-config-v") || !name.ends_with(".json") {
            continue;
        }
        let version = name
            .trim_start_matches("acc-product-config-v")
            .trim_end_matches(".json")
            .parse::<u64>()
            .unwrap_or(0);
        let modified = entry.metadata().and_then(|meta| meta.modified()).unwrap_or(std::time::UNIX_EPOCH);
        let path = entry.path();
        let better = match &best {
            None => true,
            Some((best_version, best_time, _)) => (version, modified) > (*best_version, *best_time),
        };
        if better {
            best = Some((version, modified, path));
        }
    }
    best.map(|(_, _, path)| path)
}

fn candidate_product_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(value) = std::env::var("ACC_PRODUCT_CONFIG_PATH") {
        let path = PathBuf::from(value);
        if path.is_file() {
            paths.push(path);
        }
    }
    if let Some(path) = cached_product_config() {
        paths.push(path);
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let path = PathBuf::from(local)
            .join("Programs").join("WorkBuddy").join("resources")
            .join("app.asar.unpacked").join("cli").join("product.json");
        if path.is_file() {
            paths.push(path);
        }
    }
    let home = wb_switch_core::modules::config::home_dir();
    let cached = home.join(".codebuddy").join("product.json");
    if cached.is_file() {
        paths.push(cached);
    }
    paths
}

/// 本机产品配置兜底（远端拿不到时用）。
pub fn local_entries() -> Vec<Value> {
    for path in candidate_product_paths() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let entries = parse_catalogue(&text);
            if !entries.is_empty() {
                return entries;
            }
        }
    }
    Vec::new()
}

async fn remote_entries(state: &AppState) -> Vec<Value> {
    let Some((view, token)) = accounts::first_with_token(&state.cfg.accounts_path) else {
        return Vec::new();
    };
    let base = state.cfg.upstream_base.trim_end_matches('/');
    // 两个端点都要打：/console 只返回「个人版可用」，/v3/config 更全；
    // 只取第一个成功的端点会少掉一批模型（Python 版是合并而不是短路）。
    let mut merged: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in ["/console/enterprises/personal/models", "/v3/config"] {
        let response = client()
            .get(format!("{base}{path}"))
            .header("authorization", format!("Bearer {token}"))
            .header("accept", "application/json")
            .header("x-domain", if view.domain.is_empty() { "www.codebuddy.cn".to_string() } else { view.domain.clone() })
            .timeout(Duration::from_secs(15))
            .send()
            .await;
        let Ok(response) = response else { continue };
        if !response.status().is_success() {
            continue;
        }
        let Ok(text) = response.text().await else { continue };
        for entry in parse_catalogue(&text) {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                if seen.insert(id.to_string()) {
                    merged.push(entry);
                }
            }
        }
    }
    merged
}

/// 目录主入口：缓存 60s；远端 → 本机 → 种子。
pub async fn load(state: &AppState) -> Vec<Value> {
    {
        let cache = state.catalog.lock().unwrap();
        if let Some((at, models)) = cache.as_ref() {
            if at.elapsed() < TTL {
                return models.clone();
            }
        }
    }

    // 合并策略与 Python 一致：远端优先，本机补缺，种子再补缺。
    // 不是「远端成功就不要种子」——种子覆盖的是目录没列全时的兜底，两者并存。
    let mut models: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in remote_entries(state).await {
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            if seen.insert(id.to_string()) {
                models.push(metadata_of(&entry, "catalogue"));
            }
        }
    }
    for entry in local_entries() {
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            if seen.insert(id.to_string()) {
                models.push(metadata_of(&entry, "catalogue"));
            }
        }
    }
    for (id, context) in SEED_MODELS {
        if !seen.insert((*id).to_string()) {
            continue;
        }
        models.push(json!({
            "id": id,
            "object": "model",
            "created": 1_700_000_000i64,
            "owned_by": "codebuddy",
            "name": id,
            "context_window": context,
            "max_output_tokens": DEFAULT_MAX_OUTPUT_TOKENS,
            "input_modalities": ["text"],
            "supports_tool_call": true,
            "reasoning_efforts": [],
            "tags": [],
            "source": "seed",
        }));
    }

    let mut cache = state.catalog.lock().unwrap();
    *cache = Some((Instant::now(), models.clone()));
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_envelope_and_filters_non_chat() {
        let text = r#"{"code":0,"data":{"models":[
            {"id":"glm-5.3","maxInputTokens":1000000,"supportsImages":true,"tags":["chat"]},
            {"id":"draw","tags":["text-to-image"]},
            {"id":"completion-x","vendor":"tencent"},
            {"id":"my-local","vendor":"Custom"}
        ]}}"#;
        let entries = parse_catalogue(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "glm-5.3");
    }

    #[test]
    fn metadata_reports_capabilities() {
        let entry = json!({
            "id": "glm-5.3", "supportsImages": true, "maxInputTokens": 1_000_000,
            "maxOutputTokens": 65_536, "supportsReasoning": true,
            "reasoning": {"supportedEfforts": ["low", "high"]}
        });
        let model = metadata_of(&entry, "catalogue");
        assert_eq!(model["input_modalities"], json!(["text", "image"]));
        assert_eq!(model["context_window"], json!(1_000_000));
        assert_eq!(model["reasoning_efforts"], json!(["low", "high"]));
    }

    #[test]
    fn disabled_multimodal_wins() {
        let entry = json!({"id": "x", "supportsImages": true, "disabledMultimodal": true});
        assert_eq!(metadata_of(&entry, "catalogue")["input_modalities"], json!(["text"]));
    }
}
