//! System tray icon (Windows notification area / macOS menu bar).
//!
//! Linux is not supported here (tray-icon needs GTK + libappindicator there);
//! on Linux [`Tray::new`] returns `None` and closing the window quits as usual.
//!
//! Tray callbacks run outside egui, so they only queue a [`TrayAction`] and call
//! `request_repaint()`; the app drains the queue in `App::logic`, which eframe
//! keeps calling even while the window is hidden.

use eframe::egui;
use std::sync::{Arc, Mutex};

// Only constructed by the tray callbacks (Windows / macOS).
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    OpenFolder,
    ToggleAutoAccept,
    Quit,
}

pub struct Tray {
    actions: Arc<Mutex<Vec<TrayAction>>>,
    #[cfg(any(windows, target_os = "macos"))]
    icon: tray_icon::TrayIcon,
    #[cfg(any(windows, target_os = "macos"))]
    auto_item: tray_icon::menu::CheckMenuItem,
}

impl Tray {
    #[cfg(any(windows, target_os = "macos"))]
    pub fn new(ctx: &egui::Context, auto_accept: bool) -> Option<Self> {
        use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
        use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

        let open = MenuItem::with_id("open", "AnyDrop 열기", true, None);
        let folder = MenuItem::with_id("folder", "받은 폴더 열기", true, None);
        let auto_item = CheckMenuItem::with_id("auto", "자동 수락", true, auto_accept, None);
        let quit = MenuItem::with_id("quit", "종료", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &folder,
            &PredefinedMenuItem::separator(),
            &auto_item,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .ok()?;

        let (rgba, w, h) = crate::icon::app_icon_rgba(32);
        let icon = tray_icon::Icon::from_rgba(rgba, w, h).ok()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("AnyDrop")
            .with_icon(icon)
            // Windows: left click opens the window, right click shows the menu.
            // macOS: menu-bar convention is "click shows the menu".
            .with_menu_on_left_click(cfg!(target_os = "macos"))
            .build()
            .ok()?;

        let actions: Arc<Mutex<Vec<TrayAction>>> = Arc::default();

        let (q, c) = (actions.clone(), ctx.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let a = match e.id.0.as_str() {
                "open" => TrayAction::Show,
                "folder" => TrayAction::OpenFolder,
                "auto" => TrayAction::ToggleAutoAccept,
                "quit" => TrayAction::Quit,
                _ => return,
            };
            q.lock().unwrap().push(a);
            c.request_repaint();
        }));

        let (q, c) = (actions.clone(), ctx.clone());
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            let show = matches!(
                e,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            );
            if show && !cfg!(target_os = "macos") {
                q.lock().unwrap().push(TrayAction::Show);
                c.request_repaint();
            }
        }));

        Some(Self {
            actions,
            icon: tray,
            auto_item,
        })
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    pub fn new(_ctx: &egui::Context, _auto_accept: bool) -> Option<Self> {
        None
    }

    pub fn take_actions(&self) -> Vec<TrayAction> {
        std::mem::take(&mut *self.actions.lock().unwrap())
    }

    pub fn set_auto_accept(&self, _on: bool) {
        #[cfg(any(windows, target_os = "macos"))]
        self.auto_item.set_checked(_on);
    }

    pub fn set_tooltip(&self, _text: &str) {
        #[cfg(any(windows, target_os = "macos"))]
        let _ = self.icon.set_tooltip(Some(_text));
    }
}
