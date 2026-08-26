//! 平台无关的 UI 状态与菜单模型。
//!
//! 两个后端（Linux: ksni / Windows+macOS: tray-icon）都从这里取数据：
//! - [`menu_entries`] 生成菜单结构
//! - [`title_text`]   生成托盘显示文字（模板可配置）
//! - [`tooltip`]      生成悬停提示内容

use crate::api::QuotaSnapshot;
use crate::config::Config;
use chrono::{DateTime, Duration, Local};
use std::path::PathBuf;

#[cfg(target_os = "linux")]
pub mod sni;
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub mod trayicon;

pub const ID_ACTIVATE: &str = "activate";
pub const ID_REFRESH: &str = "refresh";
pub const ID_CONFIG: &str = "config";
pub const ID_REPO: &str = "repo";
pub const ID_QUIT: &str = "quit";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPO: &str = "github.com/crazykun/GLMeter";
pub const REPO_URL: &str = "https://github.com/crazykun/GLMeter";

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

impl UiState {
    pub fn new(cfg: Config, cfg_path: PathBuf) -> Self {
        let status = if cfg.configured() {
            Status::Loading
        } else {
            Status::NoKey
        };
        Self {
            cfg,
            cfg_path,
            status,
            busy: None,
        }
    }
}

/// 与平台无关的菜单模型项
pub enum MenuEntry {
    /// 不可点击的信息行
    Info(String),
    /// 可点击按钮（id 对应 ID_* 常量）
    Button {
        id: &'static str,
        text: String,
    },
    Separator,
}

impl MenuEntry {
    /// 结构指纹：不看文本，仅看行类型（用于判断是否需要重建菜单）
    /// （仅 Windows/macOS 后端使用，Linux 的 DBusMenu 按需拉取无需指纹）
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn kind(&self) -> u8 {
        match self {
            MenuEntry::Info(_) => b'i',
            MenuEntry::Button { .. } => b'b',
            MenuEntry::Separator => b's',
        }
    }
}

pub fn menu_entries(state: &UiState) -> Vec<MenuEntry> {
    let mut v = Vec::new();
    match &state.status {
        Status::Loading => {
            v.push(MenuEntry::Info("加载中…".into()));
        }
        Status::NoKey => {
            v.push(MenuEntry::Info("未配置 API Key".into()));
            v.push(MenuEntry::Info(format!(
                "配置文件: {}",
                state.cfg_path.display()
            )));
            v.push(MenuEntry::Info(
                "填入 api_key 保存后，点击「刷新」即可".into(),
            ));
        }
        Status::Err(msg) => {
            for line in wrap(msg, 28).into_iter().take(3) {
                v.push(MenuEntry::Info(format!("⚠ {line}")));
            }
        }
        Status::Ok(s) => {
            v.push(MenuEntry::Info(format!(
                "GLM Coding Plan · {} 套餐",
                level_text(s)
            )));
            v.push(MenuEntry::Separator);
            for w in &s.windows {
                v.push(MenuEntry::Info(format!(
                    "{} {} 剩余 {}",
                    w.label,
                    bar(100.0 - w.used_pct, 12),
                    pct(100.0 - w.used_pct)
                )));
                match w.next_reset {
                    Some(dt) => v.push(MenuEntry::Info(format!(
                        "  ↻ 重置 {}（{}后）",
                        fmt_reset(&dt),
                        countdown(&dt)
                    ))),
                    None => v.push(MenuEntry::Info(
                        "  ↻ 未激活，点下方「激活额度」开始计算".into(),
                    )),
                }
            }
            if let Some(mcp) = &s.mcp {
                v.push(MenuEntry::Separator);
                v.push(MenuEntry::Info(format!(
                    "MCP 月额度 {} {}/{} 次",
                    bar(100.0 - mcp.used_pct, 12),
                    mcp.used,
                    mcp.total
                )));
                if !mcp.details.is_empty() {
                    let tools: Vec<String> = mcp
                        .details
                        .iter()
                        .map(|(n, u)| format!("{n} {u}"))
                        .collect();
                    v.push(MenuEntry::Info(format!("  · {}", tools.join(" · "))));
                }
                if let Some(dt) = mcp.next_reset {
                    v.push(MenuEntry::Info(format!("  ↻ 重置 {}", fmt_reset(&dt))));
                }
            }
        }
    }

    v.push(MenuEntry::Separator);
    if let Some(busy) = &state.busy {
        v.push(MenuEntry::Info(format!("⏳ {busy}")));
    }
    v.push(MenuEntry::Button {
        id: ID_ACTIVATE,
        text: activate_text(state).into(),
    });
    // 上次刷新时间挂在「立即刷新」按钮上：动作与反馈在同一行
    let refreshed_at = match &state.status {
        Status::Ok(s) => format!("（更新于 {}）", s.fetched_at.format("%H:%M:%S")),
        _ => String::new(),
    };
    v.push(MenuEntry::Button {
        id: ID_REFRESH,
        text: format!("↻ 立即刷新{refreshed_at}"),
    });
    v.push(MenuEntry::Button {
        id: ID_CONFIG,
        text: "⚙ 打开配置文件".into(),
    });
    v.push(MenuEntry::Button {
        id: ID_REPO,
        // 点击整行 → 默认浏览器打开仓库页
        text: format!("↗ GLMeter v{VERSION} · {REPO}"),
    });
    v.push(MenuEntry::Button {
        id: ID_QUIT,
        text: "✕ 退出".into(),
    });
    v
}

fn activate_text(state: &UiState) -> &'static str {
    match &state.status {
        Status::Ok(s) if s.needs_activation() => "⚡ 激活 5 小时额度（当前未激活）",
        _ => "⚡ 激活额度（发送 \"1\"）",
    }
}

/// 托盘显示文字：由配置模板渲染，busy 时加前缀
pub fn title_text(state: &UiState) -> String {
    let mut s = render_template(&state.cfg.tray_title, state);
    if state.busy.is_some() {
        s = format!("⏳ {s}");
    }
    s
}

/// 悬停提示：(标题, 多行正文)
pub fn tooltip(state: &UiState) -> (String, Vec<String>) {
    match &state.status {
        Status::Loading => ("GLMeter".into(), vec!["加载中…".into()]),
        Status::NoKey => (
            "GLMeter".into(),
            vec![
                "未配置 API Key".into(),
                format!("配置文件: {}", state.cfg_path.display()),
            ],
        ),
        Status::Err(m) => ("GLMeter · 查询失败".into(), wrap(m, 40)),
        Status::Ok(s) => {
            let mut lines = Vec::new();
            for w in &s.windows {
                lines.push(format!(
                    "{} {} 剩余 {}",
                    w.label,
                    bar(100.0 - w.used_pct, 16),
                    pct(100.0 - w.used_pct)
                ));
                match w.next_reset {
                    Some(dt) => {
                        lines.push(format!("重置 {}（{}后）", fmt_reset(&dt), countdown(&dt)))
                    }
                    None => lines.push("未激活（菜单中可激活）".into()),
                }
            }
            if let Some(mcp) = &s.mcp {
                lines.push(format!(
                    "MCP 月额度 {} 剩余 {} 次",
                    bar(100.0 - mcp.used_pct, 16),
                    (mcp.total - mcp.used).max(0)
                ));
            }
            if let Some(b) = &state.busy {
                lines.push(format!("⏳ {b}"));
            }
            lines.push(format!("更新于 {}", s.fetched_at.format("%H:%M:%S")));
            (format!("GLMeter · {} 套餐", level_text(s)), lines)
        }
    }
}

/// 把 {var} 占位符替换为实际值
pub fn render_template(tpl: &str, state: &UiState) -> String {
    let mut out = tpl.to_string();
    for (k, v) in template_vars(state) {
        out = out.replace(&format!("{{{k}}}"), &v);
    }
    out
}

fn template_vars(state: &UiState) -> Vec<(&'static str, String)> {
    const NA: &str = "—";
    let mut vars: Vec<(&'static str, String)> = Vec::new();
    let mut push = |k: &'static str, v: String| vars.push((k, v));

    match &state.status {
        Status::Ok(s) => {
            push("level", level_text(s));
            if let Some(w) = s.five_hour() {
                push("5h_used", format!("{}", w.used_pct.round() as i64));
                push(
                    "5h_left",
                    format!("{}", (100.0 - w.used_pct).round() as i64),
                );
                push(
                    "5h_reset",
                    w.next_reset
                        .as_ref()
                        .map(fmt_reset)
                        .unwrap_or_else(|| "未激活".into()),
                );
                push(
                    "5h_countdown",
                    w.next_reset
                        .as_ref()
                        .map(countdown)
                        .unwrap_or_else(|| "未激活".into()),
                );
            } else {
                push("5h_used", NA.into());
                push("5h_left", NA.into());
                push("5h_reset", NA.into());
                push("5h_countdown", NA.into());
            }
            match s.windows.get(1) {
                Some(w) => {
                    push("weekly_used", format!("{}", w.used_pct.round() as i64));
                    push(
                        "weekly_left",
                        format!("{}", (100.0 - w.used_pct).round() as i64),
                    );
                }
                None => {
                    push("weekly_used", NA.into());
                    push("weekly_left", NA.into());
                }
            }
            match &s.mcp {
                Some(m) => {
                    push("mcp_used", format!("{}", m.used));
                    push("mcp_total", format!("{}", m.total));
                    push("mcp_left", format!("{}", (m.total - m.used).max(0)));
                }
                None => {
                    push("mcp_used", NA.into());
                    push("mcp_total", NA.into());
                    push("mcp_left", NA.into());
                }
            }
        }
        _ => {
            for k in [
                "level",
                "5h_used",
                "5h_left",
                "5h_reset",
                "5h_countdown",
                "weekly_used",
                "weekly_left",
                "mcp_used",
                "mcp_total",
                "mcp_left",
            ] {
                push(k, NA.into());
            }
        }
    }
    vars
}

fn level_text(s: &QuotaSnapshot) -> String {
    if s.level.is_empty() {
        return "…".into();
    }
    let mut c = s.level.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
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
        "即将重置".into()
    } else if mins >= 60 {
        format!("{}小时{:02}分", mins / 60, mins % 60)
    } else {
        format!("{mins}分")
    }
}

fn pct(v: f64) -> String {
    format!("{}%", v.round() as i64)
}

/// Unicode 块字符进度条：bar(76.0, 12) => "█████████░░░"
fn bar(remaining_pct: f64, len: usize) -> String {
    let filled = ((remaining_pct.clamp(0.0, 100.0) / 100.0) * len as f64).round() as usize;
    let filled = filled.min(len);
    format!("{}{}", "█".repeat(filled), "░".repeat(len - filled))
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if cur.chars().count() >= width {
            lines.push(cur.clone());
            cur.clear();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push("(空)".into());
    }
    lines
}
