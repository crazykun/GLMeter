use crate::api::QuotaSnapshot;
use crate::config::Config;
use chrono::{DateTime, Duration, Local};
use std::path::PathBuf;
use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};

pub const ID_ACTIVATE: &str = "activate";
pub const ID_REFRESH: &str = "refresh";
pub const ID_CONFIG: &str = "config";
pub const ID_QUIT: &str = "quit";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPO: &str = "github.com/crazykun/GLMeter";

pub enum Status {
    Loading,
    NoKey,
    Ok(QuotaSnapshot),
    Err(String),
}

pub struct UiState {
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub status: Status,
    pub busy: Option<String>,
}

fn fmt_reset(dt: &DateTime<Local>) -> String {
    let now = Local::now();
    let hm = dt.format("%H:%M");
    match dt.date_naive() - now.date_naive() {
        d if d == Duration::zero() => format!("今天 {hm}"),
        d if d == Duration::days(1) => format!("明天 {hm}"),
        _ => dt.format("%m-%d %H:%M").to_string(),
    }
}

fn countdown(to: &DateTime<Local>) -> String {
    let mins = (*to - Local::now()).num_minutes();
    if mins <= 0 {
        "即将重置".to_string()
    } else if mins >= 60 {
        format!("{}小时{:02}分", mins / 60, mins % 60)
    } else {
        format!("{mins}分")
    }
}

fn pct(v: f64) -> String {
    format!("{}%", v.round() as i64)
}

fn item(id: &str, text: &str, enabled: bool) -> MenuItem {
    MenuItem::with_id(id, text, enabled, None)
}

fn info(text: &str) -> MenuItem {
    MenuItem::with_id("info", text, false, None)
}

pub fn build(state: &UiState) -> Menu {
    let menu = Menu::new();
    let _ = menu.append(&info(&format!("GLM Coding Plan · {}", level_text(state))));

    match &state.status {
        Status::Loading => {
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&info("加载中…"));
        }
        Status::NoKey => {
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&info("未配置 API Key"));
            let _ = menu.append(&info(&format!("配置文件: {}", state.cfg_path.display())));
            let _ = menu.append(&info("填入 api_key 保存后，点击「刷新」即可"));
        }
        Status::Err(msg) => {
            let _ = menu.append(&PredefinedMenuItem::separator());
            for line in wrap(msg, 28).iter().take(3) {
                let _ = menu.append(&info(&format!("⚠ {line}")));
            }
        }
        Status::Ok(s) => {
            let _ = menu.append(&PredefinedMenuItem::separator());
            for w in &s.windows {
                let _ = menu.append(&info(&format!(
                    "{}: 已用 {}（剩余 {}）",
                    w.label,
                    pct(w.used_pct),
                    pct(100.0 - w.used_pct)
                )));
                match w.next_reset {
                    Some(dt) => {
                        let _ = menu.append(&info(&format!(
                            "  ↻ 重置 {}（{}后）",
                            fmt_reset(&dt),
                            countdown(&dt)
                        )));
                    }
                    None => {
                        let _ = menu.append(&info("  ↻ 未激活，点下方「激活额度」开始计算"));
                    }
                }
            }
            if let Some(mcp) = &s.mcp {
                let _ = menu.append(&PredefinedMenuItem::separator());
                let total = if mcp.total > 0 {
                    format!("{}/{} 次", mcp.used, mcp.total)
                } else {
                    format!("{} 次", mcp.used)
                };
                let _ = menu.append(&info(&format!(
                    "MCP 月额度: {total}（已用 {}）",
                    pct(mcp.used_pct)
                )));
                if let Some(dt) = mcp.next_reset {
                    let _ = menu.append(&info(&format!("  ↻ 重置 {}", fmt_reset(&dt))));
                }
                if !mcp.details.is_empty() {
                    let tools: Vec<String> = mcp
                        .details
                        .iter()
                        .map(|(n, u)| format!("{n} {u}"))
                        .collect();
                    let _ = menu.append(&info(&format!("  · {}", tools.join(" · "))));
                }
            }
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&info(&format!(
                "更新于 {}",
                s.fetched_at.format("%H:%M:%S")
            )));
        }
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
    if let Some(busy) = &state.busy {
        let _ = menu.append(&info(&format!("⏳ {busy}")));
    }
    let activate_text = match &state.status {
        Status::Ok(s) if s.needs_activation() => "⚡ 激活 5 小时额度（当前未激活）",
        _ => "⚡ 激活额度（发送 \"1\"）",
    };
    let _ = menu.append(&item(ID_ACTIVATE, activate_text, true));
    let _ = menu.append(&item(ID_REFRESH, "↻ 立即刷新", true));
    let _ = menu.append(&item(ID_CONFIG, "⚙ 打开配置文件", true));
    let _ = menu.append(&info(&format!("GLMeter v{VERSION} · {REPO}")));
    let _ = menu.append(&item(ID_QUIT, "✕ 退出", true));
    menu
}

fn level_text(state: &UiState) -> String {
    match &state.status {
        Status::Ok(s) if !s.level.is_empty() => {
            let mut c = s.level.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
        _ => "…".to_string(),
    }
}

/// 托盘悬停提示（Linux 部分桌面环境不支持则忽略）
pub fn tooltip(state: &UiState) -> String {
    match &state.status {
        Status::Loading => "GLMeter · 加载中…".to_string(),
        Status::NoKey => "GLMeter · 未配置 API Key".to_string(),
        Status::Err(m) => format!(
            "GLMeter · 查询失败: {}",
            m.chars().take(60).collect::<String>()
        ),
        Status::Ok(s) => {
            let mut t = String::from("GLMeter");
            if let Some(w) = s.five_hour() {
                t.push_str(&format!(" · 5h剩余{}", pct(100.0 - w.used_pct)));
                if let Some(dt) = w.next_reset {
                    t.push_str(&format!(" · 重置 {}", fmt_reset(&dt)));
                }
            }
            t
        }
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        let len = cur.chars().count();
        if len >= width {
            lines.push(cur.clone());
            cur.clear();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push("(空)".to_string());
    }
    lines
}
