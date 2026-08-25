mod api;
mod config;
mod ui;

#[cfg(target_os = "linux")]
use ui::sni as backend;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use ui::trayicon as backend;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use ui::UiState;

/// 后台命令
pub enum Cmd {
    Fetch,
    Activate,
    OpenConfig,
    Quit,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        run_check(args.iter().any(|a| a == "--activate"));
        return;
    }

    if !acquire_single_instance() {
        return;
    }

    let (cfg, cfg_path) = config::load();
    let state = Arc::new(Mutex::new(UiState::new(cfg, cfg_path)));

    backend::run(state);
}

/// 共享 worker：所有网络请求在此执行，完成后通过 notify 回调唤醒对应 UI 后端
pub fn spawn_worker(
    cmd_rx: mpsc::Receiver<Cmd>,
    cmd_tx: mpsc::Sender<Cmd>,
    state: Arc<Mutex<UiState>>,
    notify: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("GLMeter/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client");
        // 已安排的自动激活目标时刻（防止重复调度）
        let mut scheduled: Option<chrono::DateTime<chrono::Local>> = None;
        // 首轮查询
        let _ = cmd_tx.send(Cmd::Fetch);
        for cmd in cmd_rx {
            match cmd {
                Cmd::Fetch => {
                    worker_fetch(&client, &state);
                    notify();
                    schedule_auto_activate(&cmd_tx, &state, &mut scheduled);
                }
                Cmd::Activate => {
                    set_busy(&state, Some("激活中，发送最小请求…".into()));
                    notify();
                    let cfg = config::load().0;
                    if let Err(e) = api::activate(&client, &cfg) {
                        eprintln!("[GLMeter] activate: {e}");
                    }
                    set_busy(&state, Some("刷新配额…".into()));
                    worker_fetch(&client, &state);
                    notify();
                    schedule_auto_activate(&cmd_tx, &state, &mut scheduled);
                }
                Cmd::OpenConfig => {
                    let path = state.lock().unwrap().cfg_path.clone();
                    open_config(&path);
                }
                Cmd::Quit => {
                    std::process::exit(0);
                }
            }
        }
    });
}

/// 定时激活调度：
/// - 窗口未激活 → 5 秒后自动激活（让 nextResetTime 立即可查）
/// - 窗口已激活 → 在重置时间过后 60 秒自动激活新窗口
///
/// 由每次 Fetch/Activate 结果驱动，`scheduled` 防止对同一目标重复起线程
fn schedule_auto_activate(
    cmd_tx: &mpsc::Sender<Cmd>,
    state: &Arc<Mutex<UiState>>,
    scheduled: &mut Option<chrono::DateTime<chrono::Local>>,
) {
    let (enabled, target) = {
        let ui = state.lock().unwrap();
        let five_hour = match &ui.status {
            ui::Status::Ok(s) => s.five_hour(),
            _ => None,
        };
        let Some(w) = five_hour else {
            *scheduled = None;
            return;
        };
        let target = match w.next_reset {
            None => Some(chrono::Local::now() + chrono::Duration::seconds(5)),
            Some(reset) if reset > chrono::Local::now() => {
                Some(reset + chrono::Duration::seconds(60))
            }
            // reset 已到但窗口仍在（即将滚动）→ 稍后重试
            Some(_) => Some(chrono::Local::now() + chrono::Duration::seconds(30)),
        };
        (ui.cfg.auto_activate, target)
    };

    if !enabled {
        *scheduled = None;
        return;
    }
    let Some(target) = target else {
        *scheduled = None;
        return;
    };
    if *scheduled == Some(target) {
        return; // 该时刻已安排
    }
    *scheduled = Some(target);

    let tx = cmd_tx.clone();
    std::thread::spawn(move || {
        let wait = (target - chrono::Local::now())
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(1));
        std::thread::sleep(wait);
        let _ = tx.send(Cmd::Activate);
    });
}

fn worker_fetch(client: &reqwest::blocking::Client, state: &Arc<Mutex<UiState>>) {
    let cfg = config::load().0;
    let result = api::fetch_quota(client, &cfg);
    let mut ui = state.lock().unwrap();
    ui.cfg = config::load().0;
    ui.busy = None;
    ui.status = match result {
        Ok(_) if !ui.cfg.configured() => ui::Status::NoKey,
        Ok(s) => ui::Status::Ok(s),
        Err(_) if !ui.cfg.configured() => ui::Status::NoKey,
        Err(e) => ui::Status::Err(e),
    };
}

fn set_busy(state: &Arc<Mutex<UiState>>, msg: Option<String>) {
    state.lock().unwrap().busy = msg;
}

pub fn spawn_ticker(cmd: mpsc::Sender<Cmd>, interval_secs: u64, align: Option<String>) {
    let interval = interval_secs.max(60);
    let align_time = align
        .as_deref()
        .and_then(|s| chrono::NaiveTime::parse_from_str(s, "%H:%M").ok())
        .or_else(|| {
            if align.as_deref().is_some_and(|s| !s.trim().is_empty()) {
                eprintln!("[GLMeter] refresh_align 格式应为 \"HH:MM\"，忽略该配置");
            }
            None
        });
    std::thread::spawn(move || loop {
        let next = match align_time {
            Some(t) => next_aligned(t, interval, chrono::Local::now()),
            None => chrono::Local::now() + chrono::Duration::seconds(interval as i64),
        };
        let wait = (next - chrono::Local::now())
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(interval));
        std::thread::sleep(wait);
        if cmd.send(Cmd::Fetch).is_err() {
            break;
        }
    });
}

/// 计算对齐网格上的下一个刷新时刻：
/// 每天从 align 起每 interval 一跳，返回其中第一个 > now 的时刻
fn next_aligned(
    align: chrono::NaiveTime,
    interval: u64,
    now: chrono::DateTime<chrono::Local>,
) -> chrono::DateTime<chrono::Local> {
    let base_local = now
        .date_naive()
        .and_time(align)
        .and_local_timezone(chrono::Local)
        .single()
        .unwrap_or(now);
    let elapsed = (now - base_local).num_seconds();
    if elapsed < 0 {
        // 今天的对齐起始点还未到，第一次刷新就在 base 时刻
        return base_local;
    }
    let jumps = elapsed / interval as i64 + 1;
    base_local + chrono::Duration::seconds(jumps * interval as i64)
}

/// 解码内嵌图标为 RGBA
pub fn icon_rgba(size: u32) -> image::RgbaImage {
    image::load_from_memory_with_format(
        include_bytes!("../assets/icon.png"),
        image::ImageFormat::Png,
    )
    .expect("decode icon")
    .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
    .to_rgba8()
}

/// ksni 需要的 ARGB32（network byte order: A R G B）图标
#[cfg(target_os = "linux")]
pub fn icon_argb(size: u32) -> ksni::Icon {
    let rgba = icon_rgba(size);
    let (w, h) = rgba.dimensions();
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for px in rgba.pixels() {
        let [r, g, b, a] = px.0;
        data.extend_from_slice(&[a, r, g, b]);
    }
    ksni::Icon {
        width: w as i32,
        height: h as i32,
        data,
    }
}

pub fn open_config(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(path)
            .spawn()
            .ok();
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn().ok();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .ok();
    }
}

/// 单实例锁：避免多实例同时注册托盘（DBus watcher 会拒绝重复注册）
fn acquire_single_instance() -> bool {
    let path = config::config_path().with_file_name("instance.lock");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::File::create(&path).and_then(|f| {
        f.try_lock()?;
        Ok(f)
    }) {
        Ok(f) => {
            // 持锁到进程退出
            std::mem::forget(f);
            true
        }
        Err(_) => {
            eprintln!("GLMeter 已在运行（锁文件: {}）", path.display());
            false
        }
    }
}

/// 无界面模式：查询并打印配额，便于调试与脚本化
fn run_check(also_activate: bool) {
    let (cfg, path) = config::load();
    println!("配置文件 : {}", path.display());
    println!("端点     : {}", cfg.base_url);
    if !cfg.configured() {
        eprintln!("未配置 api_key（编辑上方文件或设置 GLM_API_KEY 环境变量）");
        std::process::exit(2);
    }
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("GLMeter/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client");
    if also_activate {
        print!("激活中… ");
        match api::activate(&client, &cfg) {
            Ok(()) => println!("ok"),
            Err(e) => println!("失败（将继续查询）: {e}"),
        }
    }
    match api::fetch_quota(&client, &cfg) {
        Ok(s) => {
            println!("套餐等级 : {}", s.level);
            for w in &s.windows {
                let bar_len = 24;
                let filled = ((w.used_pct / 100.0) * bar_len as f64).round() as usize;
                let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_len - filled));
                println!(
                    "{}: [{}] 已用 {:.0}%（剩余 {:.0}%）",
                    w.label,
                    bar,
                    w.used_pct,
                    100.0 - w.used_pct
                );
                match w.next_reset {
                    Some(dt) => println!(
                        "  重置时间: {}（{}后）",
                        dt.format("%Y-%m-%d %H:%M"),
                        {
                            let mins = (dt - chrono::Local::now()).num_minutes().max(0);
                            if mins >= 60 {
                                format!("{}小时{}分", mins / 60, mins % 60)
                            } else {
                                format!("{mins}分")
                            }
                        }
                    ),
                    None => println!("  重置时间: 未激活（--activate 可发送最小请求激活）"),
                }
            }
            if let Some(mcp) = &s.mcp {
                println!(
                    "MCP 月额度: {}/{} 次（已用 {}%）",
                    mcp.used, mcp.total, mcp.used_pct
                );
                for (name, used) in &mcp.details {
                    println!("  · {name}: {used}");
                }
                if let Some(dt) = mcp.next_reset {
                    println!("  重置时间: {}", dt.format("%Y-%m-%d %H:%M"));
                }
            }
        }
        Err(e) => {
            eprintln!("查询失败: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, NaiveTime, TimeZone};

    fn today_at(h: u32, m: u32, s: u32) -> chrono::DateTime<chrono::Local> {
        let naive = chrono::Local::now()
            .date_naive()
            .and_hms_opt(h, m, s)
            .unwrap();
        chrono::Local.from_local_datetime(&naive).single().unwrap()
    }

    #[test]
    fn aligned_grid() {
        let align = NaiveTime::from_hms_opt(9, 30, 0).unwrap();
        let base = today_at(9, 30, 0);

        // 起始点未到 → 下一次就是起始点本身
        let now = today_at(9, 0, 0);
        assert_eq!(next_aligned(align, 300, now), base);
        // 起始点刚过 10 秒 → 下一次在 5 分钟网格上
        assert_eq!(
            next_aligned(align, 300, base + Duration::seconds(10)),
            base + Duration::seconds(300)
        );
        // 恰好落在网格点上 → 下一个点（不含当前）
        assert_eq!(
            next_aligned(align, 300, base + Duration::seconds(300)),
            base + Duration::seconds(600)
        );
        // 深夜跨天：now 已过当天起点很远 → 仍在当天网格内推进
        let late = today_at(23, 50, 0);
        let jumps = ((late - base).num_seconds() / 300 + 1) * 300;
        assert_eq!(
            next_aligned(align, 300, late),
            base + Duration::seconds(jumps)
        );
    }
}
