//! Windows 下作为 GUI 程序运行（不弹 CMD 窗口）；
//! `--check` 模式会重新附加父进程控制台，仍可在终端打印结果。
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

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
    /// scheduled = 定点（activate_at）/ 自动（auto_activate）触发：
    /// 撞上仍在计费的旧窗口时推迟到其重置后再发，避免请求白白计入垂死窗口
    Activate {
        scheduled: bool,
    },
    OpenConfig,
    OpenRepo,
    Quit,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        #[cfg(target_os = "windows")]
        attach_console();
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
        // 激活重试计数（激活后窗口仍未生效 → 1 分钟后重试，最多 3 次）
        let mut activate_retries: u32 = 0;
        const MAX_ACTIVATE_RETRIES: u32 = 3;
        const RETRY_DELAY_SECS: u64 = 60;
        // 整轮重试（首发 + 3 次重试）全部失败后进入指数退避冷却，
        // 防止「未生效 → 5 秒后再激活」的无限请求循环
        let mut failed_rounds: u32 = 0;
        let mut cooldown_until: Option<chrono::DateTime<chrono::Local>> = None;
        /// 退避时长：5 分钟起步、每失败一轮翻倍、封顶 1 小时
        const ACTIVATE_BACKOFF_BASE_SECS: u64 = 300;
        const ACTIVATE_BACKOFF_MAX_SECS: u64 = 3600;
        // 首轮查询
        let _ = cmd_tx.send(Cmd::Fetch);
        for cmd in cmd_rx {
            match cmd {
                Cmd::Fetch => {
                    worker_fetch(&client, &state);
                    notify();
                    // 窗口已激活（服务端最终生效/用户手动激活）→ 清除退避状态
                    if window_active(&state) {
                        failed_rounds = 0;
                        cooldown_until = None;
                    }
                    schedule_auto_activate(&cmd_tx, &state, &mut scheduled, cooldown_until);
                }
                Cmd::Activate { scheduled: sched } => {
                    // 定点激活若在旧窗口重置前发出，最小请求会计入垂死窗口、
                    // 新窗口不会被开启（表现为「定点激活失效」）→ 推迟到重置后续接
                    if sched {
                        if let Some(until) =
                            postpone_until(current_reset(&state), chrono::Local::now())
                        {
                            eprintln!(
                                "[GLMeter] 定点激活：当前窗口尚未重置，推迟至 {} 续接新窗口",
                                until.format("%H:%M")
                            );
                            set_busy(
                                &state,
                                Some(format!(
                                    "定点激活：等待当前窗口{}重置后自动续接…",
                                    until.format("%H:%M")
                                )),
                            );
                            notify();
                            let tx = cmd_tx.clone();
                            std::thread::spawn(move || {
                                let wait =
                                    (until - chrono::Local::now()).to_std().unwrap_or_default();
                                std::thread::sleep(wait);
                                // 续接时窗口可能已被用户自己开启，直接发一条最小请求即可，
                                // 无需再推迟（最多浪费几枚 token）
                                let _ = tx.send(Cmd::Activate { scheduled: false });
                            });
                            continue;
                        }
                    }
                    let attempt = activate_retries;
                    set_busy(
                        &state,
                        Some(if attempt == 0 {
                            "激活中，发送最小请求…".to_string()
                        } else {
                            format!("激活重试 {attempt}/{MAX_ACTIVATE_RETRIES}…")
                        }),
                    );
                    notify();
                    let cfg = config::load().0;
                    if let Err(e) = api::activate(&client, &cfg) {
                        eprintln!("[GLMeter] activate: {e}");
                    }
                    set_busy(&state, Some("刷新配额…".to_string()));
                    worker_fetch(&client, &state);
                    notify();

                    if window_active(&state) {
                        activate_retries = 0;
                        failed_rounds = 0;
                        cooldown_until = None;
                        set_busy(&state, None);
                        notify();
                        schedule_auto_activate(&cmd_tx, &state, &mut scheduled, cooldown_until);
                    } else if activate_retries < MAX_ACTIVATE_RETRIES {
                        activate_retries += 1;
                        set_busy(
                            &state,
                            Some(format!(
                                "激活未生效，{RETRY_DELAY_SECS}秒后重试（{activate_retries}/{MAX_ACTIVATE_RETRIES}）"
                            )),
                        );
                        notify();
                        let tx = cmd_tx.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(RETRY_DELAY_SECS));
                            let _ = tx.send(Cmd::Activate { scheduled: sched });
                        });
                    } else {
                        // 整轮重试失败：指数退避，到期后 Fetch 重新评估（而非直接再激活）
                        activate_retries = 0;
                        failed_rounds += 1;
                        let backoff_secs = (ACTIVATE_BACKOFF_BASE_SECS
                            << (failed_rounds - 1).min(7))
                        .min(ACTIVATE_BACKOFF_MAX_SECS);
                        cooldown_until = Some(
                            chrono::Local::now() + chrono::Duration::seconds(backoff_secs as i64),
                        );
                        scheduled = None;
                        set_busy(
                            &state,
                            Some(format!(
                                "激活未生效，{}分钟后自动重试（第{failed_rounds}轮退避）",
                                backoff_secs / 60
                            )),
                        );
                        notify();
                        eprintln!(
                            "[GLMeter] 激活重试 {MAX_ACTIVATE_RETRIES} 次仍未生效，{}分钟后重新评估",
                            backoff_secs / 60
                        );
                        let tx = cmd_tx.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                            let _ = tx.send(Cmd::Fetch);
                        });
                    }
                }
                Cmd::OpenConfig => {
                    let path = state.lock().unwrap().cfg_path.clone();
                    open_config(&path);
                }
                Cmd::OpenRepo => open_url(ui::REPO_URL),
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
/// - 退避冷却期内不调度（由退避线程到点后 Fetch 重新评估）
///
/// 由每次 Fetch/Activate 结果驱动，`scheduled` 防止对同一目标重复起线程
fn schedule_auto_activate(
    cmd_tx: &mpsc::Sender<Cmd>,
    state: &Arc<Mutex<UiState>>,
    scheduled: &mut Option<chrono::DateTime<chrono::Local>>,
    cooldown_until: Option<chrono::DateTime<chrono::Local>>,
) {
    if cooldown_until.is_some_and(|until| until > chrono::Local::now()) {
        *scheduled = None;
        return;
    }
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
        let _ = tx.send(Cmd::Activate { scheduled: true });
    });
}

/// 当前 5 小时窗口的重置时间（无快照/未激活/异常时为 None）
fn current_reset(state: &Arc<Mutex<UiState>>) -> Option<chrono::DateTime<chrono::Local>> {
    let ui = state.lock().unwrap();
    match &ui.status {
        ui::Status::Ok(s) => s.five_hour().and_then(|w| w.next_reset),
        _ => None,
    }
}

/// 定点激活决策：窗口已激活且重置时间在未来 → 推迟到重置 + 60 秒再发请求
/// （请求早于重置发出只会计入垂死窗口，新窗口不会被开启）；
/// 其余情况（未激活 / 重置已过 / 查询失败）→ None，立即发送
fn postpone_until(
    next_reset: Option<chrono::DateTime<chrono::Local>>,
    now: chrono::DateTime<chrono::Local>,
) -> Option<chrono::DateTime<chrono::Local>> {
    next_reset
        .filter(|r| *r > now)
        .map(|r| r + chrono::Duration::seconds(60))
}

fn worker_fetch(client: &reqwest::blocking::Client, state: &Arc<Mutex<UiState>>) {
    let cfg = config::load().0;
    let result = api::fetch_quota(client, &cfg);
    let mut ui = state.lock().unwrap();
    ui.cfg = cfg;
    ui.busy = None;
    ui.status = match result {
        Ok(_) if !ui.cfg.configured() => ui::Status::NoKey,
        Ok(s) => ui::Status::Ok(s),
        Err(_) if !ui.cfg.configured() => ui::Status::NoKey,
        Err(e) => ui::Status::Err(e),
    };
}

/// 5 小时窗口是否已激活（nextResetTime 可查且尚在有效期）。
/// 安全边际 90 秒：刚激活的新窗口 reset = now+5h 必然通过；
/// 垂死/刚过期的旧窗口（reset ≤ now+90s）视为未生效 → 触发重试，
/// 60 秒后的重试会落在重置之后、真正开启新窗口
fn window_active(state: &Arc<Mutex<UiState>>) -> bool {
    let ui = state.lock().unwrap();
    let margin = chrono::Local::now() + chrono::Duration::seconds(90);
    matches!(&ui.status, ui::Status::Ok(s) if s
        .five_hour()
        .is_some_and(|w| w.next_reset.is_some_and(|r| r > margin)))
}

fn set_busy(state: &Arc<Mutex<UiState>>, msg: Option<String>) {
    state.lock().unwrap().busy = msg;
}

/// 调度器入口：
/// - 周期刷新：每 interval_secs（可对齐网格）自动 Fetch
/// - 定点激活：activate_at 每天在配置时刻自动 Activate
///
/// 两个线程每轮醒来都重读配置，interval_secs / refresh_align / activate_at
/// 的改动无需重启即可生效（发现粒度约 TICKER_POLL_SECS）。
pub fn spawn_ticker(cmd: mpsc::Sender<Cmd>) {
    spawn_interval_ticker(cmd.clone());
    spawn_daily_activate(cmd);
}

/// 单次睡眠封顶：既保证定点时刻精确触发，又能及时发现配置变更
const TICKER_POLL_SECS: u64 = 60;

/// "HH:MM" 列表 → NaiveTime 列表（非法条目告警后忽略）
fn parse_activate_at(activate_at: &[String]) -> Vec<chrono::NaiveTime> {
    activate_at
        .iter()
        .filter_map(|s| {
            match chrono::NaiveTime::parse_from_str(s.trim(), "%H:%M")
                .or_else(|_| chrono::NaiveTime::parse_from_str(s.trim(), "%H:%M:%S"))
            {
                Ok(t) => Some(t),
                Err(_) => {
                    eprintln!("[GLMeter] activate_at 条目 {s:?} 格式应为 \"HH:MM\"，已忽略");
                    None
                }
            }
        })
        .collect()
}

/// "HH:MM" → NaiveTime；非法值告警后忽略（None → 滚动间隔模式）
fn parse_align(align: &Option<String>) -> Option<chrono::NaiveTime> {
    align
        .as_deref()
        .and_then(|s| chrono::NaiveTime::parse_from_str(s, "%H:%M").ok())
        .or_else(|| {
            if align.as_deref().is_some_and(|s| !s.trim().is_empty()) {
                eprintln!("[GLMeter] refresh_align 格式应为 \"HH:MM\"，忽略该配置");
            }
            None
        })
}

/// 每天在配置时刻触发一次激活（每轮重读配置，activate_at 增删改即时生效）。
/// 触发的激活若撞上仍在计费的旧窗口，worker 会自动推迟到重置后续接
fn spawn_daily_activate(cmd: mpsc::Sender<Cmd>) {
    std::thread::spawn(move || {
        // 仅在 activate_at 原始值变化时重新解析（避免每轮重复告警）
        let mut last_raw: Option<Vec<String>> = None;
        let mut times: Vec<chrono::NaiveTime> = Vec::new();
        loop {
            let raw = config::load().0.activate_at;
            if last_raw.as_deref() != Some(raw.as_slice()) {
                times = parse_activate_at(&raw);
                last_raw = Some(raw);
            }
            match next_daily(&times, chrono::Local::now()) {
                Some(next) => {
                    let wait = (next - chrono::Local::now())
                        .to_std()
                        .unwrap_or(std::time::Duration::from_secs(TICKER_POLL_SECS));
                    if wait > std::time::Duration::from_secs(TICKER_POLL_SECS) {
                        // 距离触发还远：分段睡，及时感知配置变化
                        std::thread::sleep(std::time::Duration::from_secs(TICKER_POLL_SECS));
                    } else {
                        std::thread::sleep(wait);
                        if cmd.send(Cmd::Activate { scheduled: true }).is_err() {
                            return;
                        }
                    }
                }
                None => std::thread::sleep(std::time::Duration::from_secs(TICKER_POLL_SECS)),
            }
        }
    });
}

/// 周期刷新（每轮重读配置，interval_secs / refresh_align 即时生效）
fn spawn_interval_ticker(cmd: mpsc::Sender<Cmd>) {
    std::thread::spawn(move || {
        let mut last_fire = chrono::Local::now();
        // 仅在 refresh_align 原始值变化时重新解析（避免每轮重复告警）
        let mut last_align_raw: Option<Option<String>> = None;
        let mut align_time: Option<chrono::NaiveTime> = None;
        loop {
            let (interval_raw, align_raw) = {
                let c = config::load().0;
                (c.interval_secs, c.refresh_align)
            };
            if last_align_raw != Some(align_raw.clone()) {
                align_time = parse_align(&align_raw);
                last_align_raw = Some(align_raw);
            }
            let interval = interval_raw.max(60);
            let next = match align_time {
                Some(t) => next_aligned(t, interval, chrono::Local::now()),
                None => last_fire + chrono::Duration::seconds(interval as i64),
            };
            let now = chrono::Local::now();
            if now >= next {
                last_fire = now;
                if cmd.send(Cmd::Fetch).is_err() {
                    break;
                }
            } else {
                let wait = (next - now)
                    .to_std()
                    .unwrap_or(std::time::Duration::from_secs(interval))
                    .min(std::time::Duration::from_secs(TICKER_POLL_SECS));
                std::thread::sleep(wait);
            }
        }
    });
}

/// 定点刷新时刻中最近的一个未来时刻：
/// 优先取今天尚未到达的时刻，否则取明天最早的时刻
fn next_daily(
    times: &[chrono::NaiveTime],
    now: chrono::DateTime<chrono::Local>,
) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::TimeZone;
    let today = now.date_naive();
    let tomorrow = today + chrono::Duration::days(1);
    let at = |d: chrono::NaiveDate, t: chrono::NaiveTime| {
        chrono::Local
            .from_local_datetime(&d.and_time(t))
            .single()
            .filter(|dt| *dt > now)
    };
    times
        .iter()
        .copied()
        .filter_map(|t| at(today, t).or_else(|| at(tomorrow, t)))
        .min()
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

/// ksni 需要的 ARGB32（network byte order: A R G B）图标。
/// ksni 每次 update 都会重新拉取 pixmap，缓存解码/缩放结果避免重复计算
#[cfg(target_os = "linux")]
pub fn tray_icon() -> ksni::Icon {
    static CACHE: std::sync::OnceLock<ksni::Icon> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let rgba = icon_rgba(64);
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
        })
        .clone()
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

/// 用系统默认浏览器打开链接
/// （GUI 子系统无控制台，explorer/open/xdg-open 均不弹窗）
pub fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    {
        // explorer 直接接 URL 会调用默认浏览器（rundll32 备选，无需额外依赖）
        std::process::Command::new("explorer").arg(url).spawn().ok();
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn().ok();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn().ok();
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
            #[cfg(target_os = "windows")]
            alert_already_running();
            false
        }
    }
}

/// GUI 子系统无控制台：双实例退出前弹窗提示，避免「双击没反应」
#[cfg(target_os = "windows")]
fn alert_already_running() {
    use std::ffi::c_void;
    const MB_OK: u32 = 0x0;
    const MB_ICONINFORMATION: u32 = 0x40;

    extern "system" {
        fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, utype: u32)
            -> i32;
    }

    let text: Vec<u16> = "GLMeter 已在运行。\n\n请使用托盘中的现有实例；如需重启请先退出。"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let caption: Vec<u16> = "GLMeter".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// GUI 子系统下 `--check` 重新挂回调用方终端：
/// AttachConsole(父进程) + 把 stdout/stderr 重定向到 CONOUT$
#[cfg(target_os = "windows")]
fn attach_console() {
    use std::ffi::c_void;
    const ATTACH_PARENT_PROCESS: usize = u32::MAX as usize;
    const STD_OUTPUT_HANDLE: u32 = u32::MAX - 10; // -11
    const STD_ERROR_HANDLE: u32 = u32::MAX - 11; // -12
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_WRITE: u32 = 2;
    const OPEN_EXISTING: u32 = 3;

    extern "system" {
        fn AttachConsole(process_id: usize) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            disposition: u32,
            flags: u32,
            template: *const c_void,
        ) -> *mut c_void;
        fn SetStdHandle(id: u32, handle: *mut c_void) -> i32;
    }

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return; // 非终端启动（如双击），无输出目标
        }
        let conout: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let handle = CreateFileW(
            conout.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null(),
        );
        if handle as isize != -1 {
            SetStdHandle(STD_OUTPUT_HANDLE, handle);
            SetStdHandle(STD_ERROR_HANDLE, handle);
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
    use crate::config::Config;
    use chrono::{Duration, Local, NaiveTime, TimeZone};
    use std::path::PathBuf;

    fn today_at(h: u32, m: u32, s: u32) -> chrono::DateTime<chrono::Local> {
        let naive = chrono::Local::now()
            .date_naive()
            .and_hms_opt(h, m, s)
            .unwrap();
        chrono::Local.from_local_datetime(&naive).single().unwrap()
    }

    #[test]
    fn daily_times() {
        let nine = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let eight = NaiveTime::from_hms_opt(8, 0, 0).unwrap();

        // 今天 9 点还没到 → 今天 9 点
        let now = today_at(7, 30, 0);
        assert_eq!(next_daily(&[nine], now).unwrap(), today_at(9, 0, 0));
        // 已过 9 点 → 明天 9 点
        let now = today_at(9, 0, 1);
        let expect = today_at(9, 0, 0) + Duration::days(1);
        assert_eq!(next_daily(&[nine], now).unwrap(), expect);
        // 多个时刻 → 取最近的未来时刻
        let now = today_at(8, 30, 0);
        assert_eq!(next_daily(&[nine, eight], now).unwrap(), today_at(9, 0, 0));
        // 恰好落在时刻点上 → 下一次是明天（不含当前）
        let now = today_at(8, 0, 0);
        assert_eq!(
            next_daily(&[eight], now).unwrap(),
            today_at(8, 0, 0) + Duration::days(1)
        );
        // 空列表 → 无下一次
        assert!(next_daily(&[], chrono::Local::now()).is_none());
    }

    #[test]
    fn parse_activate_at_entries() {
        let ok = parse_activate_at(&["09:00".into(), " 21:30 ".into(), "09:00:05".into()]);
        assert_eq!(ok.len(), 3);
        let bad = parse_activate_at(&["9点".into(), "".into()]);
        assert!(bad.is_empty());
    }

    #[test]
    fn scheduled_activation_postponed_until_after_reset() {
        let now = today_at(14, 0, 0);
        // 旧窗口还活着（14:00:30 重置）→ 推迟到重置 + 60s，绝不提前
        let reset = now + Duration::seconds(30);
        assert_eq!(
            postpone_until(Some(reset), now),
            Some(now + Duration::seconds(90))
        );
        // 旧窗口残余很久（次日才重置）→ 同样只等重置 + 60s
        let late = now + Duration::hours(4);
        assert_eq!(
            postpone_until(Some(late), now),
            Some(late + Duration::seconds(60))
        );
        // 窗口未激活 / 重置已过 / 查询失败 → 立即发送
        assert_eq!(postpone_until(None, now), None);
        assert_eq!(postpone_until(Some(now - Duration::seconds(1)), now), None);
    }

    #[test]
    fn window_active_requires_future_reset_with_margin() {
        let mk = |reset: Option<chrono::DateTime<chrono::Local>>| {
            let snap = crate::api::QuotaSnapshot {
                level: "lite".into(),
                windows: vec![crate::api::TokenWindow {
                    label: "5小时额度".into(),
                    used_pct: 0.0,
                    activated: reset.is_some(),
                    next_reset: reset,
                }],
                mcp: None,
                fetched_at: Local::now(),
            };
            let state = Arc::new(Mutex::new(UiState::new(
                Config::default(),
                PathBuf::from("/tmp/x"),
            )));
            state.lock().unwrap().status = ui::Status::Ok(snap);
            state
        };
        // 新窗口（reset 在 5h 后）→ 激活有效
        assert!(window_active(&mk(Some(Local::now() + Duration::hours(5)))));
        // 垂死窗口（30s 后重置）/ 已过期窗口 → 不算激活成功，应触发重试
        assert!(!window_active(&mk(Some(
            Local::now() + Duration::seconds(30)
        ))));
        assert!(!window_active(&mk(Some(
            Local::now() - Duration::minutes(1)
        ))));
        assert!(!window_active(&mk(None)));
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
