use crate::config::Config;
use chrono::{DateTime, Local};
use serde::Deserialize;

/// 归一化后的 5 小时 / 每周 Token 额度窗口
#[derive(Debug, Clone)]
pub struct TokenWindow {
    pub label: String,
    pub used_pct: f64,
    /// 本窗口已被使用（API 已返回 nextResetTime）
    pub activated: bool,
    pub next_reset: Option<DateTime<Local>>,
}

/// MCP 月度调用额度（TIME_LIMIT）
#[derive(Debug, Clone)]
pub struct McpLimit {
    pub used: i64,
    pub total: i64,
    pub used_pct: f64,
    pub next_reset: Option<DateTime<Local>>,
    pub details: Vec<(String, i64)>,
}

#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    pub level: String,
    pub windows: Vec<TokenWindow>,
    pub mcp: Option<McpLimit>,
    pub fetched_at: DateTime<Local>,
}

impl QuotaSnapshot {
    /// 5 小时窗口（多个 TOKENS_LIMIT 中 nextResetTime 最早的那个）
    pub fn five_hour(&self) -> Option<&TokenWindow> {
        self.windows.first()
    }

    /// 全部窗口均未激活（可用于决定「激活」按钮是否高亮提示）
    pub fn needs_activation(&self) -> bool {
        !self.windows.is_empty() && self.windows.iter().all(|w| !w.activated)
    }
}

#[derive(Debug, Deserialize)]
struct RawResp {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Option<RawData>,
    #[serde(default)]
    success: bool,
}

#[derive(Debug, Deserialize, Default)]
struct RawData {
    #[serde(default)]
    limits: Vec<RawLimit>,
    #[serde(default)]
    level: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawLimit {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    percentage: Option<f64>,
    #[serde(default)]
    usage: Option<i64>,
    #[serde(default)]
    current_value: Option<i64>,
    #[serde(default)]
    next_reset_time: Option<i64>,
    #[serde(default)]
    usage_details: Option<Vec<UsageDetail>>,
}

#[derive(Debug, Deserialize)]
struct UsageDetail {
    #[serde(default, rename = "modelCode")]
    model_code: String,
    #[serde(default)]
    usage: i64,
}

fn ms_to_local(ms: i64) -> Option<DateTime<Local>> {
    chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.with_timezone(&Local))
}

pub fn fetch_quota(
    client: &reqwest::blocking::Client,
    cfg: &Config,
) -> Result<QuotaSnapshot, String> {
    let resp = client
        .get(cfg.monitor_url())
        .header("Authorization", cfg.api_key.trim())
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .map_err(|e| format!("网络错误: {e}"))?;

    let status = resp.status();
    let body = resp.text().map_err(|e| format!("读取响应失败: {e}"))?;
    if !status.is_success() {
        let msg = extract_msg(&body).unwrap_or_else(|| status.to_string());
        return Err(format!("HTTP {status}: {msg}"));
    }

    let parsed: RawResp = serde_json::from_str(&body).map_err(|e| format!("解析失败: {e}"))?;
    if !parsed.success && parsed.data.is_none() {
        return Err(if parsed.msg.is_empty() {
            format!("code {}", parsed.code)
        } else {
            parsed.msg
        });
    }

    let data = parsed.data.unwrap_or_default();
    let mut token_limits: Vec<&RawLimit> = data
        .limits
        .iter()
        .filter(|l| l.kind == "TOKENS_LIMIT")
        .collect();
    token_limits.sort_by_key(|l| l.next_reset_time.unwrap_or(i64::MAX));

    let labels = ["5小时额度", "每周额度", "额度窗口"];
    let windows: Vec<TokenWindow> = token_limits
        .iter()
        .enumerate()
        .map(|(i, l)| TokenWindow {
            label: labels.get(i).copied().unwrap_or(labels[2]).to_string(),
            used_pct: l.percentage.unwrap_or(0.0),
            activated: l.next_reset_time.is_some(),
            next_reset: l.next_reset_time.and_then(ms_to_local),
        })
        .collect();

    let mcp = data
        .limits
        .iter()
        .find(|l| l.kind == "TIME_LIMIT")
        .map(|l| McpLimit {
            used: l.current_value.unwrap_or(0),
            total: l.usage.unwrap_or(0),
            used_pct: l.percentage.unwrap_or(0.0),
            next_reset: l.next_reset_time.and_then(ms_to_local),
            details: l
                .usage_details
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .filter(|d| d.usage > 0)
                .map(|d| (d.model_code.clone(), d.usage))
                .collect(),
        });

    Ok(QuotaSnapshot {
        level: data.level,
        windows,
        mcp,
        fetched_at: Local::now(),
    })
}

/// 发送一条最小请求（"1"）以激活 5 小时额度窗口的统计，
/// 使 nextResetTime 立即可查。
pub fn activate(client: &reqwest::blocking::Client, cfg: &Config) -> Result<(), String> {
    let body = serde_json::json!({
        "model": cfg.model,
        "messages": [{ "role": "user", "content": "1" }],
        "max_tokens": cfg.max_tokens,
        "stream": false,
    });

    let resp = client
        .post(cfg.chat_url())
        .bearer_auth(cfg.api_key.trim())
        .timeout(std::time::Duration::from_secs(60))
        .json(&body)
        .send()
        .map_err(|e| format!("网络错误: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        let msg = extract_msg(&text).unwrap_or_else(|| text.chars().take(120).collect());
        return Err(format!("HTTP {status}: {msg}"));
    }
    Ok(())
}

fn extract_msg(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("msg")
                .or_else(|| v.get("error"))
                .and_then(|m| m.as_str().map(String::from))
        })
}
