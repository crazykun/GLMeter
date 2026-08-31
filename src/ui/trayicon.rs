//! Windows / macOS 后端：tray-icon + tao（原生菜单）。
//!
//! 这两个平台 tooltip / set_title 原生可用：
//! - Windows: set_title = 悬停提示文字
//! - macOS:   set_title = 菜单栏标题文字
//!
//! 菜单更新策略与 Linux 相同：结构指纹不变时仅 set_text 原地更新，
//! 避免原生菜单反复重建。

use super::UiState;
use crate::ui;
use crate::{spawn_ticker, spawn_worker, Cmd};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

enum UserEvent {
    Menu(String),
    TrayClick,
    Render,
}

pub fn run(state: Arc<Mutex<UiState>>) {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();

    // mut 仅为 macOS set_activation_policy 所需
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    // 托盘应用不显示在 Dock 中
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

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

    // worker 完成网络请求后，唤醒事件循环重渲染
    let render_proxy = event_loop.create_proxy();
    spawn_worker(cmd_rx, cmd_tx.clone(), state.clone(), move || {
        let _ = render_proxy.send_event(UserEvent::Render);
    });
    // ticker 自行重读配置，无需传入快照
    spawn_ticker(cmd_tx.clone());

    // 初次构建菜单（结构指纹 + 句柄）
    let (menu0, slots0, shape0) = {
        let guard = state.lock().unwrap();
        let entries = ui::menu_entries(&guard);
        let shape: Vec<u8> = entries.iter().map(|e| e.kind()).collect();
        (build_menu(&entries), Slots::from_entries(&entries), shape)
    };
    let mut slots = Some((shape0, slots0));

    let img = crate::icon_rgba(64);
    let (w, h) = img.dimensions();
    let icon = tray_icon::Icon::from_rgba(img.into_raw(), w, h).expect("icon rgba");

    let mut tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_menu(Box::new(menu0))
        .with_tooltip(ui::title_text(&state.lock().unwrap()))
        .build()
        .expect("tray build");

    let cmd_handle: Sender<Cmd> = cmd_tx;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let Event::UserEvent(ref ue) = event {
            match ue {
                UserEvent::Render => {
                    render(&mut tray, &mut slots, &state);
                }
                UserEvent::Menu(id) => {
                    let cmd = match id.as_str() {
                        ui::ID_ACTIVATE => Cmd::Activate { scheduled: false },
                        ui::ID_REFRESH => Cmd::Fetch,
                        ui::ID_CONFIG => Cmd::OpenConfig,
                        ui::ID_REPO => Cmd::OpenRepo,
                        ui::ID_QUIT => {
                            *control_flow = ControlFlow::Exit;
                            return;
                        }
                        _ => return,
                    };
                    let _ = cmd_handle.send(cmd);
                }
                UserEvent::TrayClick => {
                    let _ = cmd_handle.send(Cmd::Fetch);
                }
            }
        }
    });
}

struct Slots(Vec<MenuItem>);

impl Slots {
    fn from_entries(entries: &[ui::MenuEntry]) -> Self {
        Self(
            entries
                .iter()
                .filter_map(|e| match e {
                    ui::MenuEntry::Info(_) | ui::MenuEntry::Button { .. } => Some(menu_item(e)),
                    ui::MenuEntry::Separator => None,
                })
                .collect(),
        )
    }
}

fn menu_item(e: &ui::MenuEntry) -> MenuItem {
    match e {
        ui::MenuEntry::Info(t) => MenuItem::with_id("info", t, false, None),
        ui::MenuEntry::Button { id, text } => MenuItem::with_id(*id, text, true, None),
        ui::MenuEntry::Separator => unreachable!(),
    }
}

fn build_menu(entries: &[ui::MenuEntry]) -> Menu {
    let menu = Menu::new();
    for e in entries {
        match e {
            ui::MenuEntry::Info(_) | ui::MenuEntry::Button { .. } => {
                let _ = menu.append(&menu_item(e));
            }
            ui::MenuEntry::Separator => {
                let _ = menu.append(&PredefinedMenuItem::separator());
            }
        }
    }
    menu
}

fn render(tray: &mut TrayIcon, slots: &mut Option<(Vec<u8>, Slots)>, state: &Arc<Mutex<UiState>>) {
    let ui = state.lock().unwrap();
    let entries = ui::menu_entries(&ui);
    let shape: Vec<u8> = entries.iter().map(|e| e.kind()).collect();
    let title = ui::title_text(&ui);
    let (tip_title, tip_lines) = ui::tooltip(&ui);
    let tip = if tip_lines.is_empty() {
        tip_title
    } else {
        format!("{tip_title}\n{}", tip_lines.join("\n"))
    };

    let needs_rebuild = slots.as_ref().is_none_or(|(s, _)| *s != shape);
    if needs_rebuild {
        tray.set_menu(Some(Box::new(build_menu(&entries))));
        *slots = Some((shape, Slots::from_entries(&entries)));
    } else if let Some((_, sl)) = slots.as_ref() {
        for (item, e) in sl.0.iter().zip(entries.iter()) {
            match e {
                ui::MenuEntry::Info(t) | ui::MenuEntry::Button { text: t, .. } => {
                    item.set_text(t);
                }
                ui::MenuEntry::Separator => {}
            }
        }
    }

    let _ = tray.set_tooltip(Some(&tip));
    tray.set_title(Some(&title));
}
