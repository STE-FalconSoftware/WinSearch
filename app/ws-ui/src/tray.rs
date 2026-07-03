//! System tray icon + global hotkey. All setup is best-effort: if the platform
//! refuses (hotkey already taken, no tray), the app still runs normally.

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Live tray + hotkey handles. Must be kept alive for the icon/hotkey to persist.
pub struct Tray {
    _icon: TrayIcon,
    _hotkeys: GlobalHotKeyManager,
    show_id: MenuId,
    quit_id: MenuId,
    hotkey_id: u32,
}

/// Result of polling tray/hotkey events for one frame.
#[derive(Default)]
pub struct TrayPoll {
    pub show: bool,
    pub quit: bool,
}

impl Tray {
    /// Build the tray icon and register the global hotkey (Ctrl+Alt+Space).
    /// Returns `None` if either could not be set up.
    pub fn setup() -> Option<Tray> {
        let menu = Menu::new();
        let show = MenuItem::new("Show WinSearch", true, None);
        let quit = MenuItem::new("Quit", true, None);
        menu.append(&show).ok()?;
        menu.append(&quit).ok()?;
        let show_id = show.id().clone();
        let quit_id = quit.id().clone();

        let icon = make_icon();
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("WinSearch — Ctrl+Alt+Space")
            .with_icon(icon)
            .build()
            .ok()?;

        let hotkeys = GlobalHotKeyManager::new().ok()?;
        let hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
        let hotkey_id = hk.id();
        hotkeys.register(hk).ok()?;

        Some(Tray {
            _icon: tray,
            _hotkeys: hotkeys,
            show_id,
            quit_id,
            hotkey_id,
        })
    }

    /// Drain tray-menu, tray-click, and hotkey events; report what happened.
    pub fn poll(&self) -> TrayPoll {
        let mut out = TrayPoll::default();

        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.id == self.hotkey_id && ev.state == HotKeyState::Pressed {
                out.show = true;
            }
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.show_id {
                out.show = true;
            } else if ev.id == self.quit_id {
                out.quit = true;
            }
        }
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::DoubleClick { .. } = ev {
                out.show = true;
            }
        }
        out
    }
}

/// A small 32×32 magnifier-ish icon drawn procedurally (no asset files).
fn make_icon() -> Icon {
    const S: usize = 32;
    let mut rgba = vec![0u8; S * S * 4];
    let cx = 13.0f32;
    let cy = 13.0f32;
    for y in 0..S {
        for x in 0..S {
            let i = (y * S + x) * 4;
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let r = (dx * dx + dy * dy).sqrt();
            // Lens ring.
            let ring = r > 7.0 && r < 10.0;
            // Handle from lower-right of the lens.
            let handle = x >= 19 && y >= 19 && (x as i32 - y as i32).abs() <= 3 && x < 30 && y < 30;
            let (rr, gg, bb, aa) = if ring || handle {
                (60u8, 130u8, 246u8, 255u8) // blue
            } else if r <= 7.0 {
                (200u8, 225u8, 255u8, 255u8) // light glass fill
            } else {
                (0, 0, 0, 0) // transparent
            };
            rgba[i] = rr;
            rgba[i + 1] = gg;
            rgba[i + 2] = bb;
            rgba[i + 3] = aa;
        }
    }
    Icon::from_rgba(rgba, S as u32, S as u32).expect("valid icon")
}
