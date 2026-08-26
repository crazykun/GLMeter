//! Linux 后端：基于 ksni（StatusNotifierItem 协议纯 Rust 实现，zbus）。
//!
//! 相比 libappindicator（tray-icon 默认 Linux 路线）的优势：
//! - 支持 Title / ToolTip 属性 → 悬停提示可用，托盘文字可自定义
//! - 纯 Rust DBus，无 GTK/libappindicator 依赖
//! - 菜单按需拉取（DBusMenu），数据刷新不会导致重复注册

use super::{UiState, ID_ACTIVATE, ID_CONFIG, ID_REFRESH, ID_REPO};
use crate::ui;
use crate::{spawn_ticker, spawn_worker, Cmd};
use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, ToolTip, Tray};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub struct GlmTray {
    pub state: Arc<Mutex<UiState>>,
    pub cmd: Sender<Cmd>,
}

impl Tray for GlmTray {
    fn id(&self) -> String {
        "GLMeter".into()
    }

    /// SNI Title 属性：多数桌面（含 Deepin）把它作为托盘悬停文字
    fn title(&self) -> String {
        ui::title_text(&self.state.lock().unwrap())
    }

    /// SNI ToolTip 属性：悬停时优先展示的富提示（多行详情）
    fn tool_tip(&self) -> ToolTip {
        let state = self.state.lock().unwrap();
        let (title, lines) = ui::tooltip(&state);
        ToolTip {
            title,
            description: lines.join("\n"),
            ..Default::default()
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![crate::icon_argb(64)]
    }

    /// 左键点击托盘图标 → 立即刷新
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.cmd.send(Cmd::Fetch);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = self.state.lock().unwrap();
        ui::menu_entries(&state)
            .into_iter()
            .map(|entry| match entry {
                ui::MenuEntry::Info(text) => StandardItem {
                    label: escape_label(&text),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
                ui::MenuEntry::Button { id, text } => {
                    let cmd_tx = self.cmd.clone();
                    StandardItem {
                        label: escape_label(&text),
                        activate: Box::new(move |_| {
                            let cmd = match id {
                                ID_ACTIVATE => Cmd::Activate,
                                ID_REFRESH => Cmd::Fetch,
                                ID_CONFIG => Cmd::OpenConfig,
                                ID_REPO => Cmd::OpenRepo,
                                _ => Cmd::Quit,
                            };
                            let _ = cmd_tx.send(cmd);
                        }),
                        ..Default::default()
                    }
                    .into()
                }
                ui::MenuEntry::Separator => MenuItem::Separator,
            })
            .collect()
    }

    /// watcher 离线（dock 未启动/重启中）时保持等待，dock 恢复后自动重连
    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        eprintln!("[GLMeter] 托盘 watcher 离线（{reason:?}），等待恢复…");
        true
    }
}

/// DBusMenu 里 "_" 是快捷键标记，需转义
fn escape_label(s: &str) -> String {
    s.replace('_', "__")
}

pub fn run(state: Arc<Mutex<UiState>>) {
    let (interval_secs, align, activate_at) = {
        let ui = state.lock().unwrap();
        (
            ui.cfg.interval_secs,
            ui.cfg.refresh_align.clone(),
            ui.cfg.activate_at.clone(),
        )
    };
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();

    let tray = GlmTray {
        state: state.clone(),
        cmd: cmd_tx.clone(),
    };

    let handle = match tray.assume_sni_available(true).spawn() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("[GLMeter] 托盘启动失败: {e}");
            std::process::exit(1);
        }
    };

    // worker 完成网络请求后，通过 update 通知 ksni 重新读取 Title/ToolTip/Menu
    spawn_worker(cmd_rx, cmd_tx.clone(), state, move || {
        let _ = handle.update(|_| ());
    });
    spawn_ticker(cmd_tx, interval_secs, align, activate_at);

    // 主线程驻留（ksni 服务与 worker 均在各自线程）
    loop {
        std::thread::park();
    }
}
