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
                        ui::ID_USE_RESET_5H => Cmd::UseResetCard { week: false },
                        ui::ID_USE_RESET_WEEK => Cmd::UseResetCard { week: true },
                        ui::ID_RESET_SITE => Cmd::OpenResetSite,
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

/// 槽位顺序对应的文本：与 [`Slots::from_entries`] 同一过滤规则（跳过分隔线）
fn slot_texts(entries: &[ui::MenuEntry]) -> Vec<&String> {
    entries
        .iter()
        .filter_map(|e| match e {
            ui::MenuEntry::Info(t) | ui::MenuEntry::Button { text: t, .. } => Some(t),
            ui::MenuEntry::Separator => None,
        })
        .collect()
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
        // 槽位跳过了分隔线，配对时也必须按同一规则过滤，
        // 否则首个分隔线之后整体错位：紧跟分隔线的行永远不更新
        for (item, t) in sl.0.iter().zip(slot_texts(&entries)) {
            item.set_text(t);
        }
    }

    let _ = tray.set_tooltip(Some(&tip));
    tray.set_title(Some(&title));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{McpLimit, QuotaSnapshot, TokenWindow};
    use crate::ui::{MenuEntry, Status};
    use chrono::Local;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn ok_state() -> Arc<Mutex<UiState>> {
        let snap = QuotaSnapshot {
            level: "lite".into(),
            windows: vec![TokenWindow {
                label: "5小时额度".into(),
                used_pct: 30.0,
                activated: true,
                next_reset: Some(Local::now() + chrono::Duration::hours(3)),
            }],
            mcp: Some(McpLimit {
                used: 13,
                total: 100,
                used_pct: 13.0,
                details: vec![("zread".into(), 7)],
                next_reset: Some(Local::now() + chrono::Duration::days(20)),
            }),
            fetched_at: Local::now(),
            resets: None,
        };
        let state = Arc::new(Mutex::new(UiState::new(
            crate::config::Config::default(),
            PathBuf::from("/tmp/x"),
        )));
        state.lock().unwrap().status = Status::Ok(snap);
        state
    }

    /// 槽位（跳过分隔线的菜单句柄）与 slot_texts 必须按同一规则过滤，
    /// 否则 render 原地 set_text 时首个分隔线之后整体错位、
    /// 紧跟分隔线的行永远不更新（macOS/Windows 菜单"不刷新"的根因）
    #[test]
    fn slots_and_slot_texts_align_on_non_separator_entries() {
        let entries = ui::menu_entries(&ok_state().lock().unwrap());
        assert!(entries.iter().any(|e| matches!(e, MenuEntry::Separator)));

        let slots = Slots::from_entries(&entries);
        let texts = slot_texts(&entries);
        assert_eq!(slots.0.len(), texts.len(), "槽位数与文本数必须一致");

        let expected: Vec<&String> = entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Info(t) | MenuEntry::Button { text: t, .. } => Some(t),
                MenuEntry::Separator => None,
            })
            .collect();
        assert_eq!(texts, expected);
        // 含数据的行（进度条）必须真的出现在槽位文本里，
        // 旧实现的错位会让这一行配到 Separator 而被跳过
        assert!(texts.iter().any(|t| t.contains("5小时额度")));
    }
}
