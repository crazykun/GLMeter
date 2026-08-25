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

/// 菜单结构指纹。
///
/// Linux 上 `tray.set_menu()` 会触发 DBusMenu 重新注册，部分桌面
/// （如 Deepin DDE 的 StatusNotifierWatcher）会拒绝重复注册并报
/// "notifier item has been registered"。因此仅在结构变化时重建菜单，
/// 常规数据刷新一律通过 `MenuItem::set_text` 原地更新。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    Loading,
    NoKey,
    Error(usize),
    Quota {
        windows: usize,
        mcp: bool,
        mcp_details: usize,
    },
}

pub fn shape_of(state: &UiState) -> Shape {
    match &state.status {
        Status::Loading => Shape::Loading,
        Status::NoKey => Shape::NoKey,
        Status::Err(msg) => Shape::Error(wrap(msg, 28).len().min(3)),
        Status::Ok(s) => Shape::Quota {
            windows: s.windows.len(),
            mcp: s.mcp.is_some(),
            mcp_details: s.mcp.as_ref().map(|m| m.details.len()).unwrap_or(0),
        },
    }
}

/// 需要长期持有的菜单项句柄，用于原地更新文本
pub struct Slots {
    header: MenuItem,
    rows: Vec<MenuItem>,
    activate: MenuItem,
}

fn item(id: &str, text: &str, enabled: bool) -> MenuItem {
    MenuItem::with_id(id, text, enabled, None)
}

fn info(text: &str) -> MenuItem {
    MenuItem::with_id("info", text, false, None)
}

pub fn build(state: &UiState) -> (Menu, Slots) {
    let menu = Menu::new();
    let header = info(&header_text(state));
    let _ = menu.append(&header);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let rows: Vec<MenuItem> = rows_text(state).iter().map(|t| info(t)).collect();
    for r in &rows {
        let _ = menu.append(r);
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
    let activate = item(ID_ACTIVATE, activate_text(state), true);
    let _ = menu.append(&activate);
    let _ = menu.append(&item(ID_REFRESH, "↻ 立即刷新", true));
    let _ = menu.append(&item(ID_CONFIG, "⚙ 打开配置文件", true));
    let _ = menu.append(&info(&format!("GLMeter v{VERSION} · {REPO}")));
    let _ = menu.append(&item(ID_QUIT, "✕ 退出", true));

    (
        menu,
        Slots {
            header,
            rows,
            activate,
        },
    )
}

/// 结构未变化时仅原地更新文本（不触发 DBusMenu 重新注册）
pub fn apply(slots: &Slots, state: &UiState) {
    slots.header.set_text(header_text(state));
    slots.activate.set_text(activate_text(state));
    for (r, t) in slots.rows.iter().zip(rows_text(state)) {
        r.set_text(t);
    }
}

fn header_text(state: &UiState) -> String {
    let busy = if state.busy.is_some() { "⏳ " } else { "" };
    match &state.status {
        Status::Loading => format!("{busy}GLM Coding Plan · 加载中…"),
        Status::NoKey => format!("{busy}GLM Coding Plan · 未配置"),
        Status::Err(_) => format!("{busy}GLM Coding Plan · 查询失败"),
        Status::Ok(s) => format!(
            "{busy}GLM Coding Plan · {} · 更新于 {}",
            level_text(s),
            s.fetched_at.format("%H:%M")
        ),
    }
}

fn activate_text(state: &UiState) -> &'static str {
    match &state.status {
        Status::Ok(s) if s.needs_activation() => "⚡ 激活 5 小时额度（当前未激活）",
        _ => "⚡ 激活额度（发送 \"1\"）",
    }
}

/// 与 Shape 一一对应的动态行文本
fn rows_text(state: &UiState) -> Vec<String> {
    match &state.status {
        Status::Loading => vec!["加载中…".to_string()],
        Status::NoKey => vec![
            "未配置 API Key".to_string(),
            format!("配置文件: {}", state.cfg_path.display()),
            "填入 api_key 保存后，点击「刷新」即可".to_string(),
        ],
        Status::Err(msg) => wrap(msg, 28)
            .into_iter()
            .take(3)
            .map(|l| format!("⚠ {l}"))
            .collect(),
        Status::Ok(s) => {
            let mut rows = Vec::new();
            for w in &s.windows {
                rows.push(format!(
                    "{}: 已用 {}（剩余 {}）",
                    w.label,
                    pct(w.used_pct),
                    pct(100.0 - w.used_pct)
                ));
                match w.next_reset {
                    Some(dt) => rows.push(format!(
                        "  ↻ 重置 {}（{}后）",
                        fmt_reset(&dt),
                        countdown(&dt)
                    )),
                    None => rows.push("  ↻ 未激活，点下方「激活额度」开始计算".to_string()),
                }
            }
            if let Some(mcp) = &s.mcp {
                let total = if mcp.total > 0 {
                    format!("{}/{} 次", mcp.used, mcp.total)
                } else {
                    format!("{} 次", mcp.used)
                };
                rows.push(format!("MCP 月额度: {total}（已用 {}）", pct(mcp.used_pct)));
                if !mcp.details.is_empty() {
                    let tools: Vec<String> = mcp
                        .details
                        .iter()
                        .map(|(n, u)| format!("{n} {u}"))
                        .collect();
                    rows.push(format!("  · {}", tools.join(" · ")));
                }
            }
            rows
        }
    }
}

/// 托盘悬停提示（Linux 部分桌面环境不支持则静默忽略）
pub fn tooltip(state: &UiState) -> String {
    let busy = state.busy.as_deref().map(|_| "⏳ ").unwrap_or("");
    match &state.status {
        Status::Loading => format!("{busy}GLMeter · 加载中…"),
        Status::NoKey => format!("{busy}GLMeter · 未配置 API Key"),
        Status::Err(m) => format!(
            "{busy}GLMeter · 查询失败: {}",
            m.chars().take(60).collect::<String>()
        ),
        Status::Ok(s) => {
            let mut t = format!("{busy}GLMeter");
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

fn level_text(s: &QuotaSnapshot) -> String {
    if s.level.is_empty() {
        return "…".to_string();
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
        lines.push("(空)".to_string());
    }
    lines
}
