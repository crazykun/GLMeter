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
    /// 用量重置卡余额（独立接口，获取失败为 None，不影响主数据）
    pub resets: Option<ResetCards>,
}

/// 一张重置卡 = 一次对应的用量重置机会
#[derive(Debug, Clone)]
pub struct ResetRecord {
    pub record_id: i64,
    /// 服务端时间字符串，如 "2026-10-01 23:59:59"（同格式可直接字典序比较）
    pub expire_time: String,
    pub available: bool,
}

/// 账号下的重置卡余额（官网「用量重置额度」）
#[derive(Debug, Clone, Default)]
pub struct ResetCards {
    pub five_hour: Vec<ResetRecord>,
    pub week: Vec<ResetRecord>,
}

impl ResetCards {
    fn list(&self, week: bool) -> &[ResetRecord] {
        if week {
            &self.week
        } else {
            &self.five_hour
        }
    }

    /// 可用张数
    pub fn available_count(&self, week: bool) -> usize {
        self.list(week).iter().filter(|r| r.available).count()
    }

    /// 最早过期的可用卡（官网对即将过期的卡标「优先」，使用时同样优先消耗）
    pub fn pick(&self, week: bool) -> Option<&ResetRecord> {
        self.list(week)
            .iter()
            .filter(|r| r.available)
            .min_by(|a, b| a.expire_time.cmp(&b.expire_time))
    }
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
        .bearer_auth(cfg.api_key.trim())
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .map_err(|e| format!("网络错误: {e}"))?;

    let status = resp.status();
    let body = resp.text().map_err(|e| format!("读取响应失败: {e}"))?;
    if !status.is_success() {
        let msg = extract_msg(&body).unwrap_or_else(|| status.to_string());
        return Err(format!("HTTP {status}: {msg}"));
    }

    parse_quota(&body)
}

/// 响应体 → QuotaSnapshot（纯函数，便于单测）
pub fn parse_quota(body: &str) -> Result<QuotaSnapshot, String> {
    let parsed: RawResp = serde_json::from_str(body).map_err(|e| format!("解析失败: {e}"))?;
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
        resets: None,
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

/// 解析控制台风格的响应：HTTP 恒为 200，业务结果在 code/success 里
fn parse_biz_response(body: &str) -> Result<serde_json::Value, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("解析失败: {e}"))?;
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 200 || !v.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
        return Err(v
            .get("msg")
            .and_then(|m| m.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("code {code}")));
    }
    Ok(v)
}

/// 重置卡余额（官网个人用量页同款接口，API Key 鉴权可用）
pub fn fetch_reset_cards(
    client: &reqwest::blocking::Client,
    cfg: &Config,
) -> Result<ResetCards, String> {
    let url = format!(
        "{}/api/biz/customer-package-reset/list?targetType=PERSONAL",
        cfg.base_url.trim_end_matches('/')
    );
    let resp = client
        .get(url)
        .bearer_auth(cfg.api_key.trim())
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .map_err(|e| format!("网络错误: {e}"))?;
    let status = resp.status();
    let body = resp.text().map_err(|e| format!("读取响应失败: {e}"))?;
    if !status.is_success() {
        let msg = extract_msg(&body).unwrap_or_else(|| status.to_string());
        return Err(format!("HTTP {status}: {msg}"));
    }
    parse_reset_cards(&body)
}

/// list 响应 → ResetCards（纯函数，便于单测）
fn parse_reset_cards(body: &str) -> Result<ResetCards, String> {
    let v = parse_biz_response(body)?;
    let data = v.get("data").ok_or("响应缺少 data")?;
    let records = |key: &str| -> Vec<ResetRecord> {
        data.get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        Some(ResetRecord {
                            record_id: r.get("recordId")?.as_i64()?,
                            expire_time: r
                                .get("expireTime")
                                .and_then(|e| e.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            available: r
                                .get("available")
                                .and_then(|a| a.as_bool())
                                .unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(ResetCards {
        five_hour: records("fiveHourResets"),
        week: records("weekResets"),
    })
}

/// 使用一张重置卡：week=false 重置 5 小时额度，true 重置周额度（同步重置 5h）。
/// `request_id` 幂等键，避免网络重试导致重复消耗
pub fn use_reset_card(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    week: bool,
    record_id: i64,
    request_id: &str,
) -> Result<(), String> {
    let body = serde_json::json!({
        "targetType": "PERSONAL",
        "resetType": if week { "WEEK" } else { "FIVE_HOUR" },
        "recordId": record_id,
        "requestId": request_id,
    });
    let url = format!(
        "{}/api/biz/customer-package-reset/use",
        cfg.base_url.trim_end_matches('/')
    );
    let resp = client
        .post(url)
        .bearer_auth(cfg.api_key.trim())
        .timeout(std::time::Duration::from_secs(30))
        .json(&body)
        .send()
        .map_err(|e| format!("网络错误: {e}"))?;
    let status = resp.status();
    let body = resp.text().map_err(|e| format!("读取响应失败: {e}"))?;
    if !status.is_success() {
        let msg = extract_msg(&body).unwrap_or_else(|| status.to_string());
        return Err(format!("HTTP {status}: {msg}"));
    }
    parse_biz_response(&body).map(|_| ())
}

static REQUEST_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 幂等请求 ID：uuid v4 格式（无系统随机源时以时间+进程信息兜底）
pub fn new_request_id() -> String {
    use std::sync::atomic::Ordering;
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut b = [0u8; 16];
    #[cfg(unix)]
    {
        use std::io::Read;
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut b))
            .is_err()
        {
            fallback_bytes(&mut b);
        }
    }
    #[cfg(not(unix))]
    fallback_bytes(&mut b);

    #[allow(unused)]
    fn fallback_bytes(b: &mut [u8; 16]) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed) as u128;
        let mut h: u128 = nanos ^ ((std::process::id() as u128) << 96) ^ (seq << 64);
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        *b = h.to_le_bytes();
    }

    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_sorted_by_reset_labeled() {
        let body = r#"{
            "success": true,
            "data": { "level": "lite", "limits": [
                { "type": "TOKENS_LIMIT", "percentage": 10.0 },
                { "type": "TOKENS_LIMIT", "percentage": 40.0, "nextResetTime": 1767225600000 }
            ] }
        }"#;
        let s = parse_quota(body).unwrap();
        assert_eq!(s.level, "lite");
        assert_eq!(s.windows.len(), 2);
        // 有 nextResetTime 的窗口排最前 → five_hour 命中它
        let w5 = s.five_hour().unwrap();
        assert!(w5.activated);
        assert!((w5.used_pct - 40.0).abs() < 1e-9);
        assert_eq!(w5.label, "5小时额度");
        assert_eq!(s.windows[1].label, "每周额度");
        assert!(!s.windows[1].activated);
        assert!(s.windows[1].next_reset.is_none());
        // 任一窗口已激活 → 无需激活提示
        assert!(!s.needs_activation());
    }

    #[test]
    fn all_unactivated_needs_activation() {
        let body = r#"{ "success": true, "data": { "limits": [
            { "type": "TOKENS_LIMIT", "percentage": 0.0 }
        ] } }"#;
        let s = parse_quota(body).unwrap();
        assert!(s.needs_activation());
        assert!(s.five_hour().unwrap().next_reset.is_none());
        // 无窗口时不算「需要激活」
        let empty = parse_quota(r#"{ "success": true, "data": {} }"#).unwrap();
        assert!(!empty.needs_activation());
    }

    #[test]
    fn mcp_parsed_and_zero_usage_filtered() {
        let body = r#"{ "success": true, "data": { "limits": [
            { "type": "TIME_LIMIT", "percentage": 25.0, "usage": 100, "currentValue": 25,
              "nextResetTime": 1767225600000,
              "usageDetails": [
                  { "modelCode": "mcp-a", "usage": 5 },
                  { "modelCode": "mcp-b", "usage": 0 }
              ] }
        ] } }"#;
        let s = parse_quota(body).unwrap();
        let mcp = s.mcp.expect("应有 TIME_LIMIT");
        assert_eq!((mcp.used, mcp.total), (25, 100));
        assert_eq!(mcp.details, vec![("mcp-a".to_string(), 5)]);
        assert!(mcp.next_reset.is_some());
    }

    #[test]
    fn error_responses() {
        let e = parse_quota(r#"{ "code": 401, "msg": "invalid api key", "success": false }"#)
            .unwrap_err();
        assert_eq!(e, "invalid api key");
        // success=false 且无 msg → 回退到 code
        let e2 = parse_quota(r#"{ "code": 500, "success": false }"#).unwrap_err();
        assert_eq!(e2, "code 500");
        // 非 JSON body（如网关 502 页面）
        assert!(parse_quota("<html>502</html>").is_err());
    }

    #[test]
    fn extract_msg_variants() {
        assert_eq!(extract_msg(r#"{"msg":"boom"}"#), Some("boom".into()));
        assert_eq!(extract_msg(r#"{"error":"bad"}"#), Some("bad".into()));
        assert_eq!(extract_msg("not json"), None);
        assert_eq!(extract_msg(r#"{"msg":42}"#), None);
    }

    #[test]
    fn reset_cards_parsed_and_pick_earliest() {
        let body = r#"{ "code": 200, "msg": "操作成功", "success": true, "data": {
            "fiveHourResets": [
                { "recordId": 11, "expireTime": "2026-09-18 21:08:08", "available": false },
                { "recordId": 12, "expireTime": "2026-10-18 18:53:07", "available": true },
                { "recordId": 13, "expireTime": "2026-10-01 23:59:59", "available": true }
            ],
            "weekResets": [
                { "recordId": 21, "expireTime": "2026-10-01 23:59:59", "available": true }
            ] } }"#;
        let r = parse_reset_cards(body).unwrap();
        assert_eq!(r.available_count(false), 2);
        assert_eq!(r.available_count(true), 1);
        // 优先消耗最早过期的可用卡（过期卡不可用）
        assert_eq!(r.pick(false).unwrap().record_id, 13);
        assert_eq!(r.pick(true).unwrap().record_id, 21);
        // 业务错误按 Err 处理
        let err = parse_reset_cards(
            r#"{ "code": 400, "msg": "不支持的重置对象类型: X", "success": false }"#,
        )
        .unwrap_err();
        assert!(err.contains("不支持"));
    }

    #[test]
    fn request_id_is_uuid_v4_shape() {
        let id = new_request_id();
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        // 版本 4 与 RFC4122 变体位
        assert!(id.starts_with(|_| true) && parts[2].starts_with('4'));
        assert!(matches!(
            parts[3].chars().next().unwrap(),
            '8' | '9' | 'a' | 'b'
        ));
    }
}
