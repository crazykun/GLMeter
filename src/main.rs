mod api;
mod config;
mod menu;

use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::MenuEvent;
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

enum UserEvent {
    Menu(String),
    TrayClick,
    Working(String),
    Quota(Result<api::QuotaSnapshot, String>),
}

enum Cmd {
    Fetch,
    Activate,
}

static UI: OnceLock<Mutex<menu::UiState>> = OnceLock::new();

// TrayIcon 内部使用 Rc（非 Sync），仅在主线程访问
thread_local! {
    static TRAY: std::cell::RefCell<Option<TrayIcon>> = const { std::cell::RefCell::new(None) };
    // 当前菜单结构指纹 + 菜单项句柄；结构不变时只做 set_text 原地更新，
    // 避免 Linux 上 DBusMenu 反复重新注册
    static MENU: std::cell::RefCell<Option<(menu::Shape, menu::Slots)>> =
        const { std::cell::RefCell::new(None) };
}

fn main() -> tray_icon::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check") {
        let _ = run_check(args.iter().any(|a| a == "--activate"));
        return Ok(());
    }

    if !acquire_single_instance() {
        return Ok(());
    }

    let (cfg, cfg_path) = config::load();

    let mut builder = EventLoopBuilder::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder.build();

    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(e.id.0.clone()));
    }));

    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = e
        {
            let _ = proxy.send_event(UserEvent::TrayClick);
        }
    }));

    let ui = UI.get_or_init(|| {
        Mutex::new(menu::UiState {
            status: menu::Status::Loading,
            busy: None,
            cfg: cfg.clone(),
            cfg_path: cfg_path.clone(),
        })
    });

    let (menu0, slots0, shape0) = {
        let guard = ui.lock().unwrap();
        let (m, s) = menu::build(&guard);
        (m, s, menu::shape_of(&guard))
    };
    MENU.with(|m| *m.borrow_mut() = Some((shape0, slots0)));

    let tray = TrayIconBuilder::new()
        .with_icon(load_icon())
        .with_menu(Box::new(menu0))
        .with_tooltip("GLMeter · GLM Coding Plan 额度监控")
        .build()?;
    TRAY.with(|t| *t.borrow_mut() = Some(tray));

    // 工作线程：所有网络请求在此执行，结果通过事件循环代理回主线程
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let worker_tx = cmd_tx.clone();
    let worker_proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("GLMeter/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client");
        let _ = worker_tx.send(Cmd::Fetch);
        for cmd in cmd_rx {
            let cfg = config::load().0;
            match cmd {
                Cmd::Fetch => {
                    let _ =
                        worker_proxy.send_event(UserEvent::Quota(api::fetch_quota(&client, &cfg)));
                }
                Cmd::Activate => {
                    let _ =
                        worker_proxy.send_event(UserEvent::Working("激活中，发送最小请求…".into()));
                    if let Err(e) = api::activate(&client, &cfg) {
                        eprintln!("[GLMeter] activate: {e}");
                    }
                    let _ = worker_proxy.send_event(UserEvent::Working("刷新配额…".into()));
                    let _ =
                        worker_proxy.send_event(UserEvent::Quota(api::fetch_quota(&client, &cfg)));
                }
            }
        }
    });

    // 定时器线程：周期性触发刷新
    let ticker_tx = cmd_tx.clone();
    let interval = std::time::Duration::from_secs(cfg.interval_secs.max(60));
    std::thread::spawn(move || loop {
        std::thread::sleep(interval);
        if ticker_tx.send(Cmd::Fetch).is_err() {
            break;
        }
    });

    let cmd_handle = cmd_tx.clone();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::UserEvent(ue) = event {
            match ue {
                UserEvent::Menu(id) => match id.as_str() {
                    menu::ID_ACTIVATE => {
                        set_busy(Some("准备激活…".into()));
                        let _ = cmd_handle.send(Cmd::Activate);
                    }
                    menu::ID_REFRESH => {
                        set_busy(Some("刷新中…".into()));
                        let _ = cmd_handle.send(Cmd::Fetch);
                    }
                    menu::ID_CONFIG => open_path(&ui.lock().unwrap().cfg_path),
                    menu::ID_QUIT => {
                        *control_flow = ControlFlow::Exit;
                    }
                    _ => {}
                },
                UserEvent::TrayClick => {
                    set_busy(Some("刷新中…".into()));
                    let _ = cmd_handle.send(Cmd::Fetch);
                }
                UserEvent::Working(msg) => set_busy(Some(msg)),
                UserEvent::Quota(result) => {
                    let mut ui = ui.lock().unwrap();
                    ui.busy = None;
                    ui.cfg = config::load().0;
                    ui.status = match result {
                        Ok(_) if !ui.cfg.configured() => menu::Status::NoKey,
                        Ok(s) => menu::Status::Ok(s),
                        Err(e) => {
                            if !ui.cfg.configured() {
                                menu::Status::NoKey
                            } else {
                                menu::Status::Err(e)
                            }
                        }
                    };
                    drop(ui);
                    render();
                }
            }
        }
    });
}

fn set_busy(msg: Option<String>) {
    if let Some(ui) = UI.get() {
        let mut ui = ui.lock().unwrap();
        ui.busy = msg;
        drop(ui);
        render();
    }
}

/// 渲染托盘菜单与悬停提示：
/// 结构变化 → 整体重建；否则仅 set_text 原地更新
fn render() {
    let Some(ui_cell) = UI.get() else { return };
    let ui = ui_cell.lock().unwrap();
    let shape = menu::shape_of(&ui);
    let tip = menu::tooltip(&ui);

    MENU.with(|cell| {
        let mut cell = cell.borrow_mut();
        let needs_rebuild = cell.as_ref().is_none_or(|(s, _)| *s != shape);
        if needs_rebuild {
            let (m, slots) = menu::build(&ui);
            TRAY.with(|t| {
                if let Some(tray) = t.borrow().as_ref() {
                    tray.set_menu(Some(Box::new(m)));
                }
            });
            *cell = Some((shape, slots));
        }
        if let Some((_, slots)) = cell.as_ref() {
            menu::apply(slots, &ui);
        }
    });

    TRAY.with(|t| {
        if let Some(tray) = t.borrow().as_ref() {
            let _ = tray.set_tooltip(Some(tip.as_str()));
        }
    });
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

fn load_icon() -> tray_icon::Icon {
    let png = image::load_from_memory_with_format(
        include_bytes!("../assets/icon.png"),
        image::ImageFormat::Png,
    )
    .expect("decode icon")
    .to_rgba8();
    let (w, h) = png.dimensions();
    tray_icon::Icon::from_rgba(png.into_raw(), w, h).expect("icon rgba")
}

fn open_path(path: &std::path::Path) {
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

/// 无界面模式：查询并打印配额，便于调试与脚本化
fn run_check(also_activate: bool) -> std::io::Result<()> {
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
            Ok(())
        }
        Err(e) => {
            eprintln!("查询失败: {e}");
            std::process::exit(1);
        }
    }
}
