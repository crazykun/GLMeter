//! 额度与重置卡提醒：
//! - 额度耗尽提醒：5 小时 / 每周 Token 窗口 + MCP 月额度的剩余跌破配置阈值
//!   （默认 20% / 10% / 5% / 1%）时各提醒一次，额度重置后重新武装
//! - 到期提醒：剩余有效期跌破配置阈值（默认 24h/6h/1h）时各提醒一次
//! - 新增/减少提醒：按 record_id 对比，减少时区分「使用」与「过期作废」
//!   （依据 expireTime 是否已到 + lastXxxResetTime 是否变化）
//!
//! 渠道：系统桌面通知（各平台原生）+ 可选 webhook 机器人（企业微信/飞书/钉钉）。
//! 提醒状态持久化在配置目录 `notify_state.json`，重启不重复提醒、不误报「新增」。

use crate::api::{QuotaSnapshot, ResetCards};
use crate::config::{Config, NotifyConfig};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

/// 一张可用卡的跟踪信息（减少时用于归因）
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct TrackedCard {
    /// 是否周额度卡
    week: bool,
    /// 服务端过期时间字符串（"2026-10-01 23:59:59"，同格式可直接字典序比较）
    expire: String,
}

/// 去重/基线状态（配置目录 notify_state.json）。
/// 旧版本状态文件缺少的字段按默认值补齐，多出的字段（如早期版本的 counts）被忽略
#[derive(Debug, Serialize, Deserialize, Default, PartialEq)]
struct NotifyState {
    /// 上次见到的可用卡：record_id → 跟踪信息
    #[serde(default)]
    cards: HashMap<i64, TrackedCard>,
    /// 上次见到的「上次用卡时间」：kind("5h"/"week") → 服务端字符串（可能为 None）。
    /// 键存在 = 该类型已完成基线；值变化 = 期间有用卡发生（服务端仅存最近一次）
    #[serde(default)]
    last_reset: HashMap<String, Option<String>>,
    /// 已提醒过的到期阈值：record_id → 已提醒阈值（小时）
    #[serde(default)]
    expire_alerted: HashMap<i64, Vec<u64>>,
    /// 已提醒过的低额度阈值：额度类型（窗口 label / "MCP 月额度"）→
    /// 已提醒阈值（剩余百分比）。额度重置（剩余高于全部阈值）时清空
    #[serde(default)]
    quota_alerted: HashMap<String, Vec<f64>>,
    /// GLMeter 自己刚用掉的卡：等下次刷新确认其消失后消账，期间不计入减少提醒
    #[serde(default)]
    self_used: HashSet<i64>,
}

/// 状态文件名（与 config.toml 同目录）
const STATE_FILE: &str = "notify_state.json";

fn state_path() -> PathBuf {
    crate::config::config_path().with_file_name(STATE_FILE)
}

fn load_state_from(path: &PathBuf) -> NotifyState {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state_to(path: &PathBuf, st: &NotifyState) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let json = serde_json::to_string(st).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
}

/// 通用提醒：桌面通知 + 已配置的 webhook（更新检查等重置卡之外的场景复用，
/// 渠道开关沿用 [notify] 配置）
pub fn push(title: &str, body: &str, cfg: &Config) {
    if cfg.notify.desktop {
        send_desktop(title, body);
    }
    send_webhooks(cfg, title, body);
}

/// 每次拉取到额度快照后调用：对比持久化状态，产生并发送提醒。
/// 阻塞时长 = 桌面通知 + webhook 发送（仅在有提醒时发生），调用方须已释放状态锁。
pub fn check(snapshot: Option<&QuotaSnapshot>, cfg: &Config) {
    let Some(snapshot) = snapshot else { return };
    if !cfg.notify.any_enabled() {
        return;
    }
    let path = state_path();
    let mut st = load_state_from(&path);
    let mut lines = Vec::new();
    lines.extend(quota_lines(snapshot, &cfg.notify, &mut st));
    // 重置卡接口失败（resets = None）时跳过重置卡部分，本轮不更新其基线，
    // 下轮成功后整体对比，避免误报「消失」；额度提醒不受影响
    if let Some(resets) = &snapshot.resets {
        lines.extend(expire_lines(resets, &cfg.notify, &mut st));
        lines.extend(diff_lines(resets, &mut st));
    }

    // 先落盘再发送：宁可漏发也不重复轰炸（发送失败不回滚，避免每轮重试刷屏）
    if let Err(e) = save_state_to(&path, &st) {
        eprintln!("[GLMeter] 通知状态保存失败: {e}");
        return;
    }
    if lines.is_empty() {
        return;
    }
    let title = "GLMeter · 提醒";
    let body = lines.join("\n");
    if cfg.notify.desktop {
        send_desktop(title, &body);
    }
    send_webhooks(cfg, title, &body);
}

/// 额度耗尽提醒：5 小时 / 每周 Token 窗口与 MCP 月额度的剩余跌破配置阈值
/// （remain_pct，剩余百分比）时各提醒一次，每类每档只提醒一次；同一轮跨过
/// 多个档位只按最紧急的一档提醒，其余档位一并标记已提醒。额度重置（剩余
/// 高于全部阈值）后清空该类记录，下次耗尽重新提醒。
fn quota_lines(snap: &QuotaSnapshot, n: &NotifyConfig, st: &mut NotifyState) -> Vec<String> {
    // 过滤非法阈值（负数 / 超过 100），降序排好后首位即最大阈值；
    // 0 为合法档 = 仅在额度用光时提醒
    let mut thresholds: Vec<f64> = n
        .remain_pct
        .iter()
        .copied()
        .filter(|t| *t >= 0.0 && *t <= 100.0)
        .collect();
    thresholds.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    thresholds.dedup();
    if thresholds.is_empty() {
        return Vec::new();
    }
    let max_threshold = thresholds[0];

    // (类型, 剩余百分比, 剩余量文案)；去重键用窗口 label（parse_quota 硬编码，稳定）
    let mut items: Vec<(&str, f64, String)> = snap
        .windows
        .iter()
        .map(|w| {
            (
                w.label.as_str(),
                (100.0 - w.used_pct).clamp(0.0, 100.0),
                format!("{}%", fmt_pct((100.0 - w.used_pct).clamp(0.0, 100.0))),
            )
        })
        .collect();
    if let Some(m) = &snap.mcp {
        let remaining_pct = (100.0 - m.used_pct).clamp(0.0, 100.0);
        let remain_text = if m.total > 0 {
            format!("{}/{} 次", (m.total - m.used).max(0), m.total)
        } else {
            format!("{}%", fmt_pct(remaining_pct))
        };
        items.push(("MCP 月额度", remaining_pct, remain_text));
    }

    let mut lines = Vec::new();
    for (key, remaining, remain_text) in items {
        if remaining > max_threshold {
            // 已重置/回升到所有阈值之上 → 重新武装
            st.quota_alerted.remove(key);
            continue;
        }
        let alerted = st.quota_alerted.entry(key.to_string()).or_default();
        let newly: Vec<f64> = thresholds
            .iter()
            .copied()
            .filter(|t| remaining <= *t && !alerted.contains(t))
            .collect();
        if newly.is_empty() {
            continue;
        }
        alerted.extend(newly.iter().copied());
        let urgent = newly.iter().copied().fold(f64::INFINITY, f64::min);
        let why = if urgent == 0.0 {
            "已用完".to_string()
        } else {
            format!("低于 {}%", fmt_pct(urgent))
        };
        lines.push(format!("⚠ {key}剩余 {remain_text}（{why}）"));
    }
    // 阈值配置缩水（如删掉某档）时同步收缩已提醒记录，防止无限增长
    for alerted in st.quota_alerted.values_mut() {
        alerted.retain(|t| thresholds.contains(t));
    }
    st.quota_alerted.retain(|_, a| !a.is_empty());
    lines
}

/// 百分比显示：整数或 ≥10 取整（"20"），小数且 <10 保留 1 位（"0.8"）
fn fmt_pct(v: f64) -> String {
    if v >= 10.0 || v == v.trunc() {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.1}")
    }
}

/// 到期提醒：找出本轮新跌破阈值的卡，按「类型+最紧急档位」分组，每组一行。
/// 已到过期时间（阈值按 0 档）单独成组催用。
/// 同轮跨过多个档位的卡只按最紧急的一档提醒，其余档位一并标记已提醒。
fn expire_lines(resets: &ResetCards, n: &NotifyConfig, st: &mut NotifyState) -> Vec<String> {
    let mut thresholds = n.expire_hours.clone();
    thresholds.sort_unstable(); // 升序：小时数越小越紧急
    thresholds.dedup();
    if thresholds.is_empty() {
        return Vec::new();
    }
    let now = Local::now();
    // (类型, 紧急档位，0=已过期) →（张数, 最早过期时间字符串，同格式字典序即时间序）
    let mut groups: BTreeMap<(bool, u64), (usize, String)> = BTreeMap::new();
    for (week, _) in [(false, "5小时额度"), (true, "周额度")] {
        for rec in resets.list(week).iter().filter(|r| r.available) {
            let Some(expire) = parse_expire(&rec.expire_time) else {
                continue;
            };
            let mins = (expire - now).num_minutes();
            let crossed: Vec<u64> = thresholds
                .iter()
                .copied()
                .filter(|h| mins <= (*h as i64) * 60)
                .collect();
            if crossed.is_empty() {
                continue;
            }
            let alerted = st.expire_alerted.entry(rec.record_id).or_default();
            let newly: Vec<u64> = crossed
                .iter()
                .copied()
                .filter(|h| !alerted.contains(h))
                .collect();
            if newly.is_empty() {
                continue;
            }
            alerted.extend(newly.iter());
            let urgent = if mins <= 0 {
                0
            } else {
                newly.iter().copied().min().unwrap()
            };
            let e = groups
                .entry((week, urgent))
                .or_insert((0, rec.expire_time.clone()));
            e.0 += 1;
            if rec.expire_time < e.1 {
                e.1 = rec.expire_time.clone();
            }
        }
    }
    // 只保留仍可用的卡的记录，防止映射无限增长
    let live_ids: Vec<i64> = [(false), (true)]
        .iter()
        .flat_map(|&week| resets.list(week).iter().filter(|r| r.available))
        .map(|r| r.record_id)
        .collect();
    st.expire_alerted.retain(|id, _| live_ids.contains(id));

    let labels = [(false, "5小时额度"), (true, "周额度")];
    groups
        .into_iter()
        .map(|((week, urgent), (count, earliest))| {
            let label = labels.iter().find(|(w, _)| *w == week).unwrap().1;
            let exp = crate::ui::fmt_expire(&earliest);
            if urgent == 0 {
                format!("⏰ {label}重置卡×{count} 已到过期时间（{exp}），尽快使用")
            } else {
                format!("⏰ {label}重置卡×{count} 将在{urgent}小时内过期（最早 {exp} 过期）")
            }
        })
        .collect()
}

/// 新增/减少提醒（按 record_id 与上轮可用卡对比）：
/// - 消失的卡：expireTime 未到 → 必是被使用（卡不会提前过期）；已过期的默认过期，
///   但若该类型 lastXxxResetTime 变了且没有未过期的消失卡可解释，说明有 1 张
///   是赶在过期前被用掉的
/// - 新出现的 record_id → 新增
/// - GLMeter 自己用掉的卡（self_used）消账不计提醒，其引发的 lastReset 变化
///   也不作为其他卡「被使用」的归因信号
/// - last_reset 无键 = 首次见到该类型 → 只记基线不提醒
fn diff_lines(resets: &ResetCards, st: &mut NotifyState) -> Vec<String> {
    let now = Local::now();
    let mut lines = Vec::new();
    for (week, label, key) in [(false, "5小时额度", "5h"), (true, "周额度", "week")] {
        let now_cards: HashMap<i64, String> = resets
            .list(week)
            .iter()
            .filter(|r| r.available)
            .map(|r| (r.record_id, r.expire_time.clone()))
            .collect();
        let last = if week {
            resets.last_week_reset.clone()
        } else {
            resets.last_five_hour_reset.clone()
        };

        // 基线：键不存在 = 首次见到，只记录
        let baselined = st.last_reset.contains_key(key);
        let prev_last = st.last_reset.insert(key.to_string(), last.clone());
        let raw_changed = baselined && prev_last.flatten().as_deref() != last.as_deref();

        // 自己用掉的卡确认消失 → 待消账（先参与消失排除，再从 self_used 移除）
        let self_gone: Vec<i64> = st
            .self_used
            .iter()
            .copied()
            .filter(|id| {
                st.cards.get(id).is_some_and(|c| c.week == week) && !now_cards.contains_key(id)
            })
            .collect();
        // 自己用卡引发的变化不能拿来归因别的卡
        let last_changed = raw_changed && self_gone.is_empty();

        // 上轮可用、本轮不可用且非自己用掉的卡
        let vanished: Vec<String> = st
            .cards
            .iter()
            .filter(|(id, c)| {
                c.week == week
                    && !now_cards.contains_key(*id)
                    && !st.self_used.contains(*id)
                    && !self_gone.contains(id)
            })
            .map(|(_, c)| c.expire.clone())
            .collect();
        for id in &self_gone {
            st.self_used.remove(id);
        }
        let added_ids: Vec<i64> = now_cards
            .keys()
            .filter(|id| !st.cards.contains_key(*id))
            .copied()
            .collect();
        let added = added_ids.len();
        // 新到卡中最早过期的一张（同格式字典序即时间序）
        let added_expire = added_ids
            .iter()
            .filter_map(|id| now_cards.get(id))
            .min()
            .map(|e| crate::ui::fmt_expire(e).to_string())
            .filter(|e| !e.is_empty());

        if baselined {
            let n = now_cards.len();
            let future = vanished
                .iter()
                .filter(|e| parse_expire(e).is_some_and(|t| t > now))
                .count();
            let past = vanished.len() - future;
            let (used, expired) = if last_changed && future == 0 && past > 0 {
                (1, past - 1)
            } else {
                (future, past)
            };
            if used > 0 {
                lines.push(format!(
                    "♻️ {label}重置卡已使用 {used} 张，额度已重置（剩 {n} 张可用）"
                ));
            }
            if expired > 0 {
                lines.push(format!("⏰ {label}重置卡过期作废 {expired} 张"));
            }
            if added > 0 {
                let exp = added_expire
                    .map(|e| format!("，最早 {e} 过期"))
                    .unwrap_or_default();
                lines.push(format!(
                    "🎁 新到 {added} 张 {label}重置卡{exp}（现有 {n} 张可用）"
                ));
            }
        }

        // 更新该类型的跟踪集合：清掉本类型旧记录，写入仍可用的卡
        st.cards.retain(|_, c| c.week != week);
        for (id, expire) in now_cards {
            st.cards.insert(id, TrackedCard { week, expire });
        }
    }
    lines
}

/// GLMeter 自己使用重置卡成功后调用：登记该卡，
/// 下次刷新确认其消失时不再报「已使用」
pub fn mark_self_used(record_id: i64) {
    let path = state_path();
    let mut st = load_state_from(&path);
    st.self_used.insert(record_id);
    if let Err(e) = save_state_to(&path, &st) {
        eprintln!("[GLMeter] 通知状态保存失败: {e}");
    }
}

/// 服务端过期时间字符串（"2026-10-01 23:59:59"，官网按北京时间展示）按本机时区解析；
/// 国内用户本机时区即北京时间，与官网显示一致
fn parse_expire(s: &str) -> Option<chrono::DateTime<Local>> {
    use chrono::TimeZone;
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .and_then(|nd| Local.from_local_datetime(&nd).earliest())
}

// ── 桌面通知（各平台原生，与 confirm_dialog 同样的「外调命令」风格）────────

#[cfg(all(unix, not(target_os = "macos")))]
fn send_desktop(title: &str, body: &str) {
    let ok = std::process::Command::new("notify-send")
        .args(["-a", "GLMeter"])
        .arg(title)
        .arg(body)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("[GLMeter] 桌面通知失败（notify-send 不可用或被拒绝）");
    }
}

#[cfg(target_os = "macos")]
fn send_desktop(title: &str, body: &str) {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        esc(body),
        esc(title)
    );
    let ok = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("[GLMeter] 桌面通知失败（osascript 不可用）");
    }
}

#[cfg(target_os = "windows")]
fn send_desktop(title: &str, body: &str) {
    use base64::Engine;
    use std::os::windows::process::CommandExt;

    // Win10/11 原生 toast：借 PowerShell 自身的 AUMID 弹通知，无需安装模块
    // （XML 文本节点转义 & < >，PS 单引号字符串再翻倍 '）
    let xml = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\'', "''")
    };
    let script = format!(
        r#"$ErrorActionPreference='SilentlyContinue'
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] | Out-Null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom, ContentType=WindowsRuntime] | Out-Null
$xml=[Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$t=$xml.GetElementsByTagName('text')
$t.Item(0).AppendChild($xml.CreateTextNode('{}')) | Out-Null
$t.Item(1).AppendChild($xml.CreateTextNode('{}')) | Out-Null
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\WindowsPowerShell\v1.0\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($xml))"#,
        xml(title),
        xml(body),
    );
    let utf16le: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let enc = base64::engine::general_purpose::STANDARD.encode(utf16le);
    let ok = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &enc])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW，避免闪黑框
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("[GLMeter] 桌面通知失败（PowerShell toast 不可用）");
    }
}

// ── Webhook 机器人（单个 hook_url，按域名自动识别机器人类型）────────────

/// 群机器人类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    /// 企业微信群机器人
    Wecom,
    /// 飞书自定义机器人
    Feishu,
    /// 钉钉自定义机器人
    DingTalk,
}

impl Channel {
    fn label(self) -> &'static str {
        match self {
            Channel::Wecom => "企微",
            Channel::Feishu => "飞书",
            Channel::DingTalk => "钉钉",
        }
    }

    /// 按 hook URL 域名识别（域名后缀匹配，忽略大小写；
    /// 覆盖国内站与国际版 larksuite / dingtalk）
    fn detect(url: &str) -> Option<Channel> {
        let u = url.to_ascii_lowercase();
        if u.contains("weixin.qq.com") {
            Some(Channel::Wecom)
        } else if u.contains("feishu.cn") || u.contains("larksuite.com") {
            Some(Channel::Feishu)
        } else if u.contains("dingtalk.com") {
            Some(Channel::DingTalk)
        } else {
            None
        }
    }

    /// 各平台 text 消息体（企微与钉钉同构，飞书字段名不同）
    fn payload(self, text: &str) -> serde_json::Value {
        match self {
            Channel::Wecom | Channel::DingTalk => {
                serde_json::json!({"msgtype":"text","text":{"content": text}})
            }
            Channel::Feishu => serde_json::json!({"msg_type":"text","content":{"text": text}}),
        }
    }
}

fn send_webhooks(cfg: &Config, title: &str, body: &str) {
    let url = cfg.notify.hook_url.trim();
    if url.is_empty() {
        return;
    }
    let Some(channel) = Channel::detect(url) else {
        eprintln!("[GLMeter] hook_url 域名无法识别机器人类型（支持企微/飞书/钉钉）: {url}");
        return;
    };
    // 钉钉加签：安全设置选「加签」时必填 dingtalk_secret，其余平台忽略
    let url = if channel == Channel::DingTalk {
        match cfg.notify.dingtalk_secret.trim() {
            "" => url.to_string(),
            secret => dingtalk_signed(url, secret, &chrono::Utc::now()),
        }
    } else {
        url.to_string()
    };
    // 以 GLMeter 开头：钉钉自定义关键词安全设置可直接用「GLMeter」
    let text = format!("【{title}】\n{body}");
    let client = reqwest::blocking::Client::new();
    let payload = channel.payload(&text);
    webhook_result(channel.label(), post_json(&client, &url, &payload));
}

fn post_json(
    client: &reqwest::blocking::Client,
    url: &str,
    payload: &serde_json::Value,
) -> Result<String, String> {
    let resp = client
        .post(url)
        .timeout(std::time::Duration::from_secs(10))
        .json(payload)
        .send()
        .map_err(|e| format!("网络错误: {e}"))?;
    let status = resp.status();
    let body = resp.text().map_err(|e| format!("读取响应失败: {e}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", truncate(&body, 120)));
    }
    Ok(body)
}

/// 各平台返回码统一检查：errcode/code/StatusCode == 0 视为成功，否则记日志
fn webhook_result(kind: &str, result: Result<String, String>) {
    match result {
        Err(e) => eprintln!("[GLMeter] {kind}通知发送失败: {e}"),
        Ok(body) => {
            let code = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("errcode")
                        .or_else(|| v.get("code"))
                        .or_else(|| v.get("StatusCode"))
                        .and_then(|c| c.as_i64())
                });
            if code != Some(0) {
                eprintln!("[GLMeter] {kind}通知被拒: {}", truncate(&body, 200));
            }
        }
    }
}

/// 按字符数截断（直接按字节切会把中文切成非法 UTF-8 导致 panic）
fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// 钉钉加签 URL：sign = urlencode(base64(HMAC-SHA256(secret, "{timestamp}\n{secret}")))
fn dingtalk_signed(hook: &str, secret: &str, now: &chrono::DateTime<chrono::Utc>) -> String {
    let ts = now.timestamp_millis();
    format!("{hook}&timestamp={ts}&sign={}", dingtalk_sign(secret, ts))
}

fn dingtalk_sign(secret: &str, ts: i64) -> String {
    use base64::Engine;
    use hmac::Mac;
    let mut mac =
        hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("hmac 任意长度密钥");
    mac.update(format!("{ts}\n{secret}").as_bytes());
    base64::engine::general_purpose::STANDARD
        .encode(mac.finalize().into_bytes())
        .replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{McpLimit, QuotaSnapshot, ResetRecord, TokenWindow};
    use chrono::Duration;

    fn cards(entries: &[(i64, &str, bool)]) -> ResetCards {
        ResetCards {
            five_hour: entries
                .iter()
                .map(|(id, exp, av)| ResetRecord {
                    record_id: *id,
                    expire_time: exp.to_string(),
                    available: *av,
                })
                .collect(),
            week: Vec::new(),
            last_five_hour_reset: None,
            last_week_reset: None,
        }
    }

    fn expire_at(hours: f64) -> String {
        (Local::now() + Duration::minutes((hours * 60.0) as i64))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    fn notify_cfg(hours: &[u64]) -> NotifyConfig {
        NotifyConfig {
            expire_hours: hours.to_vec(),
            desktop: false,
            ..NotifyConfig::default()
        }
    }

    fn quota_cfg(pcts: &[f64]) -> NotifyConfig {
        NotifyConfig {
            remain_pct: pcts.to_vec(),
            desktop: false,
            ..NotifyConfig::default()
        }
    }

    /// 构造额度快照：windows 按索引贴标签（与 parse_quota 一致），mcp 传 (used, total, used_pct)
    fn quota_snap(used_pcts: &[f64], mcp: Option<(i64, i64, f64)>) -> QuotaSnapshot {
        let labels = ["5小时额度", "每周额度"];
        QuotaSnapshot {
            level: "lite".into(),
            windows: used_pcts
                .iter()
                .enumerate()
                .map(|(i, &p)| TokenWindow {
                    label: labels.get(i).copied().unwrap_or(labels[0]).to_string(),
                    used_pct: p,
                    activated: true,
                    next_reset: None,
                })
                .collect(),
            mcp: mcp.map(|(used, total, pct)| McpLimit {
                used,
                total,
                used_pct: pct,
                next_reset: None,
                details: Vec::new(),
            }),
            fetched_at: Local::now(),
            resets: None,
        }
    }

    #[test]
    fn expire_alerts_fire_once_per_level() {
        let n = notify_cfg(&[24, 6, 1]);
        let mut st = NotifyState::default();

        // 剩余 5h：同时跨过 24h 和 6h 两档 → 只按最紧急的 6h 提醒一次
        let r = cards(&[(1, &expire_at(5.0), true)]);
        let lines = expire_lines(&r, &n, &mut st);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("将在6小时内过期"), "{lines:?}");

        // 同一状态再查 → 不重复提醒
        assert!(expire_lines(&r, &n, &mut st).is_empty());

        // 跌破 1h → 追加 1h 提醒
        let r2 = cards(&[(1, &expire_at(0.5), true)]);
        let lines = expire_lines(&r2, &n, &mut st);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("将在1小时内过期"), "{lines:?}");
        assert!(expire_lines(&r2, &n, &mut st).is_empty());
    }

    #[test]
    fn expire_groups_same_level_cards() {
        let n = notify_cfg(&[24]);
        let mut st = NotifyState::default();
        let r = cards(&[
            (1, &expire_at(10.0), true),
            (2, &expire_at(20.0), true),
            (3, &expire_at(30.0), false), // 已用掉的卡不提醒
        ]);
        let lines = expire_lines(&r, &n, &mut st);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("×2"), "{lines:?}");
        // 最早过期时间取字典序最小者（10 小时那张）
        assert!(lines[0].contains(&expire_at(10.0)[5..16]), "{lines:?}");
    }

    #[test]
    fn expired_but_available_card_is_urgent() {
        let n = notify_cfg(&[24, 6, 1]);
        let mut st = NotifyState::default();
        let r = cards(&[(1, &expire_at(-0.1), true)]);
        let lines = expire_lines(&r, &n, &mut st);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("已到过期时间"), "{lines:?}");
        assert!(expire_lines(&r, &n, &mut st).is_empty());
    }

    #[test]
    fn quota_alerts_fire_once_per_level() {
        let n = quota_cfg(&[20.0, 10.0, 5.0, 1.0]);
        let mut st = NotifyState::default();

        // 剩余 18% → 跌破 20% 档
        let lines = quota_lines(&quota_snap(&[82.0], None), &n, &mut st);
        assert_eq!(lines, vec!["⚠ 5小时额度剩余 18%（低于 20%）"]);
        // 同一状态再查 → 不重复提醒
        assert!(quota_lines(&quota_snap(&[82.0], None), &n, &mut st).is_empty());

        // 一轮内跌到剩 4% → 跨过 10% 与 5% 两档，只按最紧急的一档提醒
        let lines = quota_lines(&quota_snap(&[96.0], None), &n, &mut st);
        assert_eq!(lines, vec!["⚠ 5小时额度剩余 4%（低于 5%）"]);

        // 跌破 1% 档（剩余 <10 显示 1 位小数）
        let lines = quota_lines(&quota_snap(&[99.5], None), &n, &mut st);
        assert_eq!(lines, vec!["⚠ 5小时额度剩余 0.5%（低于 1%）"]);
    }

    #[test]
    fn quota_rearms_after_reset() {
        let n = quota_cfg(&[20.0]);
        let mut st = NotifyState::default();

        let lines = quota_lines(&quota_snap(&[85.0], None), &n, &mut st);
        assert_eq!(lines, vec!["⚠ 5小时额度剩余 15%（低于 20%）"]);

        // 窗口重置（已用 0，含未激活窗口）→ 剩余回到阈值之上，静默并清空记录
        assert!(quota_lines(&quota_snap(&[0.0], None), &n, &mut st).is_empty());
        assert!(!st.quota_alerted.contains_key("5小时额度"));

        // 再次跌破 → 重新提醒
        let lines = quota_lines(&quota_snap(&[90.0], None), &n, &mut st);
        assert_eq!(lines, vec!["⚠ 5小时额度剩余 10%（低于 20%）"]);
    }

    #[test]
    fn quota_kinds_tracked_separately() {
        let n = quota_cfg(&[10.0]);
        let mut st = NotifyState::default();

        // 5h 剩 95%、周剩 5%、MCP 剩 8% → 后两类提醒，5h 不动；MCP 按剩余次数显示
        let lines = quota_lines(
            &quota_snap(&[5.0, 95.0], Some((92, 100, 92.0))),
            &n,
            &mut st,
        );
        assert_eq!(
            lines,
            vec![
                "⚠ 每周额度剩余 5%（低于 10%）",
                "⚠ MCP 月额度剩余 8/100 次（低于 10%）",
            ]
        );
        // 各类只提醒一次
        assert!(quota_lines(
            &quota_snap(&[5.0, 95.0], Some((92, 100, 92.0))),
            &n,
            &mut st
        )
        .is_empty());
    }

    #[test]
    fn quota_zero_threshold_alerts_when_exhausted() {
        let n = quota_cfg(&[0.0]);
        let mut st = NotifyState::default();
        // 仅剩 3% → 未触达 0 档
        assert!(quota_lines(&quota_snap(&[97.0], None), &n, &mut st).is_empty());
        // 用光 → 提醒
        let lines = quota_lines(&quota_snap(&[100.0], None), &n, &mut st);
        assert_eq!(lines, vec!["⚠ 5小时额度剩余 0%（已用完）"]);
    }

    #[test]
    fn quota_invalid_thresholds_ignored() {
        let mut st = NotifyState::default();
        // 全部非法（负数 / 超 100）→ 相当于关闭
        let n = quota_cfg(&[-5.0, 150.0]);
        assert!(quota_lines(&quota_snap(&[99.0], None), &n, &mut st).is_empty());
        // 合法与非法混合 → 只保留合法档
        let n = quota_cfg(&[20.0, 150.0]);
        let lines = quota_lines(&quota_snap(&[85.0], None), &n, &mut st);
        assert_eq!(lines, vec!["⚠ 5小时额度剩余 15%（低于 20%）"]);
    }

    #[test]
    fn quota_mcp_total_unknown_falls_back_to_pct() {
        let n = quota_cfg(&[5.0]);
        let mut st = NotifyState::default();
        // total = 0（接口未返回总量）→ 按剩余百分比显示
        let lines = quota_lines(&quota_snap(&[], Some((0, 0, 97.0))), &n, &mut st);
        assert_eq!(lines, vec!["⚠ MCP 月额度剩余 3%（低于 5%）"]);
    }

    #[test]
    fn diff_baseline_first_fetch_is_silent() {
        let mut st = NotifyState::default();
        assert!(diff_lines(&cards(&[(1, &expire_at(10.0), true)]), &mut st).is_empty());
        // 已记录基线：卡片集合 + last_reset 键
        assert_eq!(st.cards.len(), 1);
        assert!(st.last_reset.contains_key("5h"));
        assert!(st.last_reset.contains_key("week"));
    }

    #[test]
    fn diff_new_card_reported_by_id() {
        let mut st = NotifyState::default();
        diff_lines(&cards(&[(1, &expire_at(10.0), true)]), &mut st);
        let lines = diff_lines(
            &cards(&[(1, &expire_at(10.0), true), (2, &expire_at(20.0), true)]),
            &mut st,
        );
        assert_eq!(
            lines,
            vec![format!(
                "🎁 新到 1 张 5小时额度重置卡，最早 {} 过期（现有 2 张可用）",
                &expire_at(20.0)[5..16]
            )]
        );
        // 同一轮再查 → 静默
        assert!(diff_lines(
            &cards(&[(1, &expire_at(10.0), true), (2, &expire_at(20.0), true)]),
            &mut st
        )
        .is_empty());
    }

    #[test]
    fn diff_vanished_with_future_expiry_is_used() {
        let mut st = NotifyState::default();
        diff_lines(
            &cards(&[(1, &expire_at(10.0), true), (2, &expire_at(20.0), true)]),
            &mut st,
        );
        // 未过期的卡消失（lastReset 未变，如两次刷新间被用掉）→ 已使用
        let lines = diff_lines(&cards(&[(2, &expire_at(20.0), true)]), &mut st);
        assert_eq!(
            lines,
            vec!["♻️ 5小时额度重置卡已使用 1 张，额度已重置（剩 1 张可用）"]
        );
    }

    #[test]
    fn diff_expired_without_last_reset_change() {
        let mut st = NotifyState::default();
        diff_lines(
            &cards(&[(1, &expire_at(-0.1), true), (2, &expire_at(20.0), true)]),
            &mut st,
        );
        // 已过期时间点的卡消失，lastReset 未变 → 过期作废
        let lines = diff_lines(&cards(&[(2, &expire_at(20.0), true)]), &mut st);
        assert_eq!(lines, vec!["⏰ 5小时额度重置卡过期作废 1 张"]);
    }

    #[test]
    fn diff_last_reset_change_implies_use_of_expired_card() {
        let mut st = NotifyState::default();
        diff_lines(
            &cards(&[(1, &expire_at(-0.1), true), (2, &expire_at(-0.2), true)]),
            &mut st,
        );
        // 两张都已过 expireTime，但 lastReset 变了 → 1 张是赶在过期前用掉的，
        // 剩下 1 张过期作废
        let mut r = cards(&[]);
        r.last_five_hour_reset = Some("2026-09-28 10:00:00".into());
        let lines = diff_lines(&r, &mut st);
        assert_eq!(
            lines,
            vec![
                "♻️ 5小时额度重置卡已使用 1 张，额度已重置（剩 0 张可用）",
                "⏰ 5小时额度重置卡过期作废 1 张",
            ]
        );
    }

    #[test]
    fn diff_self_used_suppressed_and_neutralizes_last_reset() {
        let mut st = NotifyState::default();
        diff_lines(
            &cards(&[(1, &expire_at(-0.1), true), (2, &expire_at(20.0), true)]),
            &mut st,
        );
        // 自己用掉了卡 2（未过期），随后它消失且 lastReset 变化 → 不提醒，
        // lastReset 变化也不归因到同时消失的卡 1（过期）头上
        st.self_used.insert(2);
        let mut r = cards(&[(1, &expire_at(-0.1), true)]);
        r.last_five_hour_reset = Some("2026-09-28 10:00:00".into());
        let lines = diff_lines(&r, &mut st);
        assert!(lines.is_empty(), "{lines:?}");
        // 消账完成
        assert!(st.self_used.is_empty());
        // 下轮新增恢复正常
        let lines = diff_lines(
            &cards(&[(1, &expire_at(-0.1), true), (3, &expire_at(30.0), true)]),
            &mut st,
        );
        assert_eq!(
            lines,
            vec![format!(
                "🎁 新到 1 张 5小时额度重置卡，最早 {} 过期（现有 2 张可用）",
                &expire_at(30.0)[5..16]
            )]
        );
    }

    #[test]
    fn diff_swap_reports_used_and_added() {
        let mut st = NotifyState::default();
        diff_lines(&cards(&[(1, &expire_at(10.0), true)]), &mut st);
        // 同轮：卡 1 被用掉 + 新卡 2 到账（总数不变，但都应报出来）
        let lines = diff_lines(&cards(&[(2, &expire_at(20.0), true)]), &mut st);
        assert_eq!(
            lines,
            vec![
                "♻️ 5小时额度重置卡已使用 1 张，额度已重置（剩 1 张可用）".to_string(),
                format!(
                    "🎁 新到 1 张 5小时额度重置卡，最早 {} 过期（现有 1 张可用）",
                    &expire_at(20.0)[5..16]
                ),
            ]
        );
    }

    #[test]
    fn diff_week_kind_tracked_separately() {
        let mut st = NotifyState::default();
        let mut r = cards(&[]);
        r.week = vec![ResetRecord {
            record_id: 21,
            expire_time: expire_at(-0.1), // 已到过期时间
            available: true,
        }];
        assert!(diff_lines(&r, &mut st).is_empty());
        r.week[0].available = false;
        let lines = diff_lines(&r, &mut st);
        assert_eq!(lines, vec!["⏰ 周额度重置卡过期作废 1 张"]);
    }

    #[test]
    fn state_serializes_losslessly() {
        let mut st = NotifyState::default();
        st.cards.insert(
            7,
            TrackedCard {
                week: false,
                expire: "2026-10-01 23:59:59".into(),
            },
        );
        st.last_reset
            .insert("5h".into(), Some("2026-09-26 20:39:51".into()));
        st.last_reset.insert("week".into(), None);
        st.expire_alerted.insert(42, vec![24, 6]);
        st.quota_alerted.insert("5小时额度".into(), vec![20.0, 5.0]);
        st.self_used.insert(99);
        let json = serde_json::to_string(&st).unwrap();
        let back: NotifyState = serde_json::from_str(&json).unwrap();
        assert_eq!(st, back);
    }

    /// 旧版本状态文件（含已废弃的 counts 字段、缺新字段）应能正常读取
    #[test]
    fn state_loads_legacy_file() {
        let legacy = r#"{"counts":{"5h":3},"expire_alerted":{"42":[24]}}"#;
        let st: NotifyState = serde_json::from_str(legacy).unwrap();
        assert!(st.cards.is_empty());
        assert!(st.self_used.is_empty());
        assert_eq!(st.expire_alerted.get(&42), Some(&vec![24]));
    }

    #[test]
    fn parse_expire_handles_server_format() {
        let dt = parse_expire("2026-10-01 23:59:59").unwrap();
        assert_eq!(
            dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-10-01 23:59:59"
        );
        assert!(parse_expire("garbage").is_none());
        assert!(parse_expire("").is_none());
    }

    #[test]
    fn dingtalk_sign_matches_reference_vector() {
        // python3: base64(hmac_sha256("SEC1234567890abcdef", "1700000000000\nSEC1234567890abcdef"))
        assert_eq!(
            dingtalk_sign("SEC1234567890abcdef", 1_700_000_000_000),
            "RqBq3E1RTBDv3n2QBCh4adZ2WHk9mVklyUoDBLxarjI%3D"
        );
    }

    #[test]
    fn dingtalk_signed_url_appends_timestamp_and_urlencoded_sign() {
        let now = chrono::Utc::now();
        let url = dingtalk_signed(
            "https://oapi.dingtalk.com/robot/send?access_token=abc",
            "SECxxx",
            &now,
        );
        let prefix = format!(
            "https://oapi.dingtalk.com/robot/send?access_token=abc&timestamp={}&sign=",
            now.timestamp_millis()
        );
        assert!(url.starts_with(&prefix), "{url}");
        // 签名已 URL 编码：不含裸的 + / =
        let sign = &url[prefix.len()..];
        assert!(!sign.is_empty());
        assert!(sign
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'%' || b == b'-' || b == b'_'));
    }

    #[test]
    fn channel_detected_by_domain() {
        assert_eq!(
            Channel::detect("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc"),
            Some(Channel::Wecom)
        );
        assert_eq!(
            Channel::detect("https://open.feishu.cn/open-apis/bot/v2/hook/abc"),
            Some(Channel::Feishu)
        );
        // 飞书国际版
        assert_eq!(
            Channel::detect("https://open.larksuite.com/open-apis/bot/v2/hook/abc"),
            Some(Channel::Feishu)
        );
        assert_eq!(
            Channel::detect("https://oapi.dingtalk.com/robot/send?access_token=abc"),
            Some(Channel::DingTalk)
        );
        // 未知域名 / 空串 → 不发送
        assert_eq!(Channel::detect("https://example.com/hook"), None);
        assert_eq!(Channel::detect(""), None);
    }

    #[test]
    fn channel_payload_shapes() {
        let t = "hi";
        let wecom = Channel::Wecom.payload(t);
        assert_eq!(
            (wecom["msgtype"].as_str(), wecom["text"]["content"].as_str()),
            (Some("text"), Some(t))
        );
        let feishu = Channel::Feishu.payload(t);
        assert_eq!(
            (
                feishu["msg_type"].as_str(),
                feishu["content"]["text"].as_str()
            ),
            (Some("text"), Some(t))
        );
        let ding = Channel::DingTalk.payload(t);
        assert_eq!(
            (ding["msgtype"].as_str(), ding["text"]["content"].as_str()),
            (Some("text"), Some(t))
        );
    }
}
