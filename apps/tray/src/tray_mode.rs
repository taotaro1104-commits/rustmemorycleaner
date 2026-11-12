//! 常駐トレイ（ウィンドウなし）。ダッシュボードは別プロセス（--window）として必要なときだけ起動する。
//! これにより非表示時のワーキングセットを小さく保つ（GUI フレームワークを常駐させない）。
use crate::fmt::gb;
use crate::icon;
use crate::service_link::{Cmd, Link, Options};
use global_hotkey::{hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use memopt_core::Level;
use std::process::{Child, Command};
use std::time::Duration;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::UI::WindowsAndMessaging::*;

const DEFAULT_HOTKEY: &str = "Ctrl+Alt+M";
const TIMER_MS: u32 = 1000;

struct Tray {
    link: Link,
    icon: TrayIcon,
    open: MenuItem,
    clean: MenuItem,
    top: Submenu,
    pause: CheckMenuItem,
    estop: CheckMenuItem,
    quit: MenuItem,
    hotkeys: GlobalHotKeyManager,
    hotkey: Option<HotKey>,
    hotkey_text: String,
    last_percent: Option<u8>,
    last_top: Vec<String>,
    dashboard: Option<Child>,
}

fn build_icon(percent: u8) -> Option<Icon> {
    Icon::from_rgba(icon::render(percent), icon::SIZE as u32, icon::SIZE as u32).ok()
}

impl Tray {
    fn new() -> Result<Self, String> {
        let link = crate::service_link::spawn(Options {
            features_every: Some(Duration::from_secs(30)),
            report_session: true,
            poll_every: Duration::from_secs(5),
        });
        let open = MenuItem::new("ダッシュボードを開く", true, None);
        let clean = MenuItem::new("今すぐ軽く解放 (L1)", false, None);
        let top = Submenu::new("上位 5 機能", true);
        let pause = CheckMenuItem::new("一時停止（1 時間）", false, false, None);
        let estop = CheckMenuItem::new("緊急停止（自動解放を無効化）", false, false, None);
        let quit = MenuItem::new("終了", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &clean,
            &top,
            &PredefinedMenuItem::separator(),
            &pause,
            &estop,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| e.to_string())?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("MemOpt")
            .with_icon(build_icon(0).ok_or("アイコンを作れません")?)
            .build()
            .map_err(|e| e.to_string())?;
        let hotkeys = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
        let mut t = Self {
            link,
            icon,
            open,
            clean,
            top,
            pause,
            estop,
            quit,
            hotkeys,
            hotkey: None,
            hotkey_text: String::new(),
            last_percent: None,
            last_top: vec![],
            dashboard: None,
        };
        t.sync_hotkey(DEFAULT_HOTKEY);
        Ok(t)
    }

    /// ショートカットを登録し直す（他アプリと衝突して登録できなくても動作は続ける）。
    fn sync_hotkey(&mut self, text: &str) {
        if self.hotkey_text == text {
            return;
        }
        if let Some(old) = self.hotkey.take() {
            let _ = self.hotkeys.unregister(old);
        }
        self.hotkey_text = text.to_string();
        if let Some(hk) = crate::hotkey::to_hotkey(text) {
            if self.hotkeys.register(hk).is_ok() {
                self.hotkey = Some(hk);
            }
        }
    }

    fn open_dashboard(&mut self) {
        if let Some(c) = self.dashboard.as_mut() {
            if c.try_wait().ok().flatten().is_none() {
                return; // すでに開いている
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            self.dashboard = Command::new(exe).arg("--window").spawn().ok();
        }
    }

    fn tick(&mut self) {
        let v = self.link.snapshot();
        if let Some(ui) = &v.ui {
            let hk = ui.hotkey_clean.clone();
            self.sync_hotkey(&hk);
        }
        let mem = v.mem.or_else(memopt_probe::memory_snapshot);
        if let Some(m) = mem {
            let used = (100.0 - m.avail_pct()).round().clamp(0.0, 100.0) as u8;
            let show = v.ui.as_ref().is_none_or(|u| u.tray_shows_percent);
            let shown = if show { used } else { 0 };
            if self.last_percent != Some(shown) {
                if let Some(i) = build_icon(shown) {
                    let _ = self.icon.set_icon(Some(i));
                }
                self.last_percent = Some(shown);
            }
            let mode = if v.connected { "サービス接続中" } else { "監視モード" };
            let _ = self.icon.set_tooltip(Some(format!("MemOpt: メモリ使用 {used}%（空き {}）\n{mode}", gb(m.avail_phys))));
        }
        self.clean.set_enabled(v.connected);
        self.pause.set_enabled(v.connected);
        self.estop.set_enabled(v.connected);
        self.pause.set_checked(v.paused);
        self.estop.set_checked(v.emergency_stop);

        let top: Vec<String> = v.features.iter().take(5).map(|f| format!("{} — {}", f.name, gb(f.private_bytes))).collect();
        if top != self.last_top {
            while self.top.remove_at(0).is_some() {}
            for t in &top {
                let _ = self.top.append(&MenuItem::new(t, false, None));
            }
            self.last_top = top;
        }
    }

    /// false を返したら終了。
    fn on_menu(&mut self, id: &MenuId) -> bool {
        let v = self.link.snapshot();
        if id == self.open.id() {
            self.open_dashboard();
        } else if id == self.clean.id() {
            self.link.send(Cmd::RunClean(Level::L1));
        } else if id == self.pause.id() {
            self.link.send(Cmd::Pause(if v.paused { 0 } else { 3600 }));
        } else if id == self.estop.id() {
            self.link.send(Cmd::EmergencyStop(!v.emergency_stop));
        } else if id == self.quit.id() {
            return false;
        }
        true
    }
}

pub fn run() -> Result<(), String> {
    let mut tray = Tray::new()?;
    tray.tick();
    unsafe {
        SetTimer(None, 1, TIMER_MS, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if msg.message == WM_TIMER {
                tray.tick();
            }
            while let Ok(e) = MenuEvent::receiver().try_recv() {
                if !tray.on_menu(&e.id) {
                    return Ok(());
                }
            }
            while let Ok(e) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    tray.open_dashboard();
                }
            }
            while let Ok(e) = GlobalHotKeyEvent::receiver().try_recv() {
                if e.state == HotKeyState::Pressed && Some(e.id) == tray.hotkey.map(|h| h.id()) {
                    tray.link.send(Cmd::RunClean(Level::L1));
                }
            }
        }
    }
    Ok(())
}
