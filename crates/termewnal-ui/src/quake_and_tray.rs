//! Quake drop-down console window geometry, X11 global hotkey listener, and Linux
//! `org.kde.StatusNotifierItem` (`zbus`) System Tray adapter.

#[cfg(target_os = "linux")]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::time::Duration;

use iced::window;
use iced::{Point, Size};
use termewnal_core::{QuakeConfig, QuakeEdge};
use tokio::sync::mpsc;
#[cfg(target_os = "linux")]
use tracing::{debug, warn};
#[cfg(target_os = "linux")]
use x11rb::connection::Connection as X11Connection;
#[cfg(target_os = "linux")]
use x11rb::protocol::Event as X11Event;
#[cfg(target_os = "linux")]
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, Keycode, ModMask};
#[cfg(target_os = "linux")]
use zbus::interface;
#[cfg(target_os = "linux")]
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

const DEFAULT_MONITOR_WIDTH: f32 = 1440.0;
const DEFAULT_MONITOR_HEIGHT: f32 = 900.0;

/// Logical screen rectangle for anchoring the Quake drop-down window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuakeBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Computes Quake window position & size anchored to `edge` at `size_percent` of `monitor`.
pub fn compute_quake_bounds(edge: QuakeEdge, size_percent: u8, monitor: Size) -> QuakeBounds {
    let mw = monitor.width.max(320.0);
    let mh = monitor.height.max(240.0);
    let frac = (size_percent.clamp(15, 90) as f32) / 100.0;
    match edge {
        QuakeEdge::Top => QuakeBounds {
            x: 0.0,
            y: 0.0,
            width: mw,
            height: (mh * frac).round(),
        },
        QuakeEdge::Bottom => {
            let h = (mh * frac).round();
            QuakeBounds {
                x: 0.0,
                y: (mh - h).max(0.0),
                width: mw,
                height: h,
            }
        }
        QuakeEdge::Left => QuakeBounds {
            x: 0.0,
            y: 0.0,
            width: (mw * frac).round(),
            height: mh,
        },
        QuakeEdge::Right => {
            let w = (mw * frac).round();
            QuakeBounds {
                x: (mw - w).max(0.0),
                y: 0.0,
                width: w,
                height: mh,
            }
        }
    }
}

/// Builds `iced::window::Settings` for a borderless always-on-top Quake drop-down window.
pub fn quake_window_settings(config: &QuakeConfig, monitor: Size) -> window::Settings {
    let bounds = compute_quake_bounds(config.edge, config.size_percent, monitor);
    window::Settings {
        size: Size::new(bounds.width, bounds.height),
        position: window::Position::Specific(Point::new(bounds.x, bounds.y)),
        decorations: false,
        level: window::Level::AlwaysOnTop,
        exit_on_close_request: false,
        visible: true,
        ..window::Settings::default()
    }
}

/// Runtime state tracking the persistent Quake drop-down console window.
#[derive(Debug, Clone)]
pub struct QuakeController {
    window_id: Option<window::Id>,
    visible: bool,
    monitor_size: Size,
}

impl Default for QuakeController {
    fn default() -> Self {
        Self {
            window_id: None,
            visible: false,
            monitor_size: Size::new(DEFAULT_MONITOR_WIDTH, DEFAULT_MONITOR_HEIGHT),
        }
    }
}

impl QuakeController {
    pub fn window_id(&self) -> Option<window::Id> {
        self.window_id
    }

    pub fn is_visible(&self) -> bool {
        self.window_id.is_some() && self.visible
    }

    pub fn monitor_size(&self) -> Size {
        self.monitor_size
    }

    pub fn set_monitor_size(&mut self, size: Size) {
        if size.width >= 320.0 && size.height >= 240.0 {
            self.monitor_size = size;
        }
    }

    pub fn set_open(&mut self, id: window::Id) {
        self.window_id = Some(id);
        self.visible = true;
    }

    pub fn set_visible(&mut self, visible: bool) {
        if self.window_id.is_some() {
            self.visible = visible;
        } else {
            self.visible = false;
        }
    }

    pub fn clear_window(&mut self) {
        self.window_id = None;
        self.visible = false;
    }

    pub fn current_bounds(&self, config: &QuakeConfig) -> QuakeBounds {
        compute_quake_bounds(config.edge, config.size_percent, self.monitor_size)
    }
}

/// Parsed global hotkey descriptor compatible with X11 `XGrabKey`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedGlobalHotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
    pub x11_keysym: u32,
    pub display: String,
}

/// Parses human-readable shortcut strings (`"Ctrl+Alt+Space"`, `"Ctrl+`"`, `"Super+F12"`) into X11 keysyms.
pub fn parse_global_hotkey(spec: &str) -> Result<ParsedGlobalHotkey, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("Global hotkey string cannot be empty".to_string());
    }

    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut super_key = false;
    let mut key_token: Option<&str> = None;

    for raw_part in trimmed.split('+') {
        let part = raw_part.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "meta" | "mod1" => alt = true,
            "shift" => shift = true,
            "super" | "win" | "logo" | "mod4" | "cmd" => super_key = true,
            _ => {
                if key_token.is_some() {
                    return Err(format!("Multiple non-modifier keys in hotkey '{spec}'"));
                }
                key_token = Some(part);
            }
        }
    }

    let key = key_token.ok_or_else(|| format!("Hotkey '{spec}' is missing a trigger key"))?;
    let keysym = match key.to_ascii_lowercase().as_str() {
        "space" => 0x0020,
        "grave" | "`" => 0x0060,
        "tilde" | "asciitilde" | "~" => 0x007e,
        "enter" | "return" => 0xFF0D,
        "esc" | "escape" => 0xFF1B,
        "tab" => 0xFF09,
        "backspace" => 0xFF08,
        "f1" => 0xFFBE,
        "f2" => 0xFFBF,
        "f3" => 0xFFC0,
        "f4" => 0xFFC1,
        "f5" => 0xFFC2,
        "f6" => 0xFFC3,
        "f7" => 0xFFC4,
        "f8" => 0xFFC5,
        "f9" => 0xFFC6,
        "f10" => 0xFFC7,
        "f11" => 0xFFC8,
        "f12" => 0xFFC9,
        lower if lower.chars().count() == 1 => {
            let ch = lower.chars().next().unwrap();
            if ch.is_ascii() {
                ch as u32
            } else {
                return Err(format!("Unsupported non-ASCII hotkey trigger '{key}'"));
            }
        }
        other => return Err(format!("Unsupported hotkey trigger '{other}'")),
    };

    Ok(ParsedGlobalHotkey {
        ctrl,
        alt,
        shift,
        super_key,
        x11_keysym: keysym,
        display: trimmed.to_string(),
    })
}

/// Active registration guard returned by [`GlobalHotkeyRegistrar`]; unregisters cleanly on drop.
pub trait HotkeyRegistrationGuard: Send + Sync {}

/// Injectable global hotkey listener boundary.
pub trait GlobalHotkeyRegistrar: Send + Sync {
    fn register(
        &self,
        spec: &str,
        on_trigger: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Option<Box<dyn HotkeyRegistrationGuard>>, String>;

    fn backend_name(&self) -> &'static str;
}

/// Null hotkey registrar for headless tests or platforms without a global grabber.
#[derive(Debug, Default)]
pub struct NullGlobalHotkeyRegistrar;

impl GlobalHotkeyRegistrar for NullGlobalHotkeyRegistrar {
    fn register(
        &self,
        spec: &str,
        _on_trigger: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Option<Box<dyn HotkeyRegistrationGuard>>, String> {
        let _ = parse_global_hotkey(spec)?;
        Ok(None)
    }

    fn backend_name(&self) -> &'static str {
        "ipc-fallback"
    }
}

/// Returns the default platform global hotkey registrar.
pub fn default_global_hotkey_registrar() -> Arc<dyn GlobalHotkeyRegistrar> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(X11GlobalHotkeyRegistrar)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(NullGlobalHotkeyRegistrar)
    }
}

/// Returns the default platform desktop System Tray adapter.
pub fn default_system_tray(
    action_tx: mpsc::UnboundedSender<TrayAction>,
) -> Arc<dyn SystemTrayAdapter> {
    #[cfg(target_os = "linux")]
    {
        ZbusStatusNotifierItem::spawn(action_tx)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = action_tx;
        Arc::new(NullSystemTray)
    }
}

#[cfg(target_os = "linux")]
struct X11HotkeyGuard {
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[cfg(target_os = "linux")]
impl HotkeyRegistrationGuard for X11HotkeyGuard {}

#[cfg(target_os = "linux")]
impl Drop for X11HotkeyGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.handle.lock()
            && let Some(join) = guard.take()
        {
            let _ = join.join();
        }
    }
}

/// X11 `XGrabKey` global hotkey registrar via `x11rb`.
///
/// Falls back cleanly (returns `Ok(None)`) when `$DISPLAY` is absent or Wayland/headless
/// runs without XWayland, allowing compositors or `termewnal --toggle-quake` IPC to toggle Quake.
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
pub struct X11GlobalHotkeyRegistrar;

#[cfg(target_os = "linux")]
impl GlobalHotkeyRegistrar for X11GlobalHotkeyRegistrar {
    fn register(
        &self,
        spec: &str,
        on_trigger: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Option<Box<dyn HotkeyRegistrationGuard>>, String> {
        let parsed = parse_global_hotkey(spec)?;
        let has_display = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty());
        if !has_display {
            debug!("DISPLAY unset; global hotkey falling back to IPC `--toggle-quake`");
            return Ok(None);
        }

        let Ok((conn, screen_num)) = x11rb::connect(None) else {
            debug!("unable to connect to X11 display; global hotkey using IPC fallback");
            return Ok(None);
        };

        let setup = conn.setup();
        let Some(screen) = setup.roots.get(screen_num) else {
            return Ok(None);
        };
        let root = screen.root;
        let min_kc = setup.min_keycode;
        let max_kc = setup.max_keycode;
        let count = max_kc.saturating_sub(min_kc).saturating_add(1);

        let Ok(cookie) = conn.get_keyboard_mapping(min_kc, count) else {
            return Ok(None);
        };
        let Ok(mapping) = cookie.reply() else {
            return Ok(None);
        };
        let syms_per_kc = mapping.keysyms_per_keycode as usize;
        if syms_per_kc == 0 {
            return Ok(None);
        }

        let mut target_kc: Option<Keycode> = None;
        for (idx, chunk) in mapping.keysyms.chunks_exact(syms_per_kc).enumerate() {
            if chunk.contains(&parsed.x11_keysym) {
                target_kc = Some(min_kc.saturating_add(idx as u8));
                break;
            }
        }
        let Some(keycode) = target_kc else {
            warn!(hotkey = %parsed.display, "X11 keycode not found for global hotkey; using IPC fallback");
            return Ok(None);
        };

        let mut base_bits: u16 = 0;
        if parsed.shift {
            base_bits |= ModMask::SHIFT.bits();
        }
        if parsed.ctrl {
            base_bits |= ModMask::CONTROL.bits();
        }
        if parsed.alt {
            base_bits |= ModMask::M1.bits();
        }
        if parsed.super_key {
            base_bits |= ModMask::M4.bits();
        }

        // Register across NumLock (Mod2) & CapsLock (Lock) states.
        let lock_combos = [
            0u16,
            ModMask::LOCK.bits(),
            ModMask::M2.bits(),
            ModMask::LOCK.bits() | ModMask::M2.bits(),
        ];
        for extra in lock_combos {
            let mask = ModMask::from(base_bits | extra);
            if let Err(err) = conn.grab_key(
                false,
                root,
                mask,
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            ) {
                warn!(hotkey = %parsed.display, error = %err, "XGrabKey failed");
                return Ok(None);
            }
        }
        let _ = conn.flush();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let important_bits =
            ModMask::SHIFT.bits() | ModMask::CONTROL.bits() | ModMask::M1.bits() | ModMask::M4.bits();

        let join = std::thread::Builder::new()
            .name("termewnal-x11-hotkey".to_string())
            .spawn(move || {
                while !stop_bg.load(Ordering::SeqCst) {
                    loop {
                        match conn.poll_for_event() {
                            Ok(Some(X11Event::KeyPress(ev))) => {
                                if ev.detail == keycode && (ev.state.bits() & important_bits) == base_bits {
                                    on_trigger();
                                }
                            }
                            Ok(Some(_)) => {}
                            Ok(None) => break,
                            Err(_) => return,
                        }
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                for extra in lock_combos {
                    let _ = conn.ungrab_key(keycode, root, ModMask::from(base_bits | extra));
                }
                let _ = conn.flush();
            })
            .map_err(|e| format!("failed spawning X11 hotkey thread: {e}"))?;

        Ok(Some(Box::new(X11HotkeyGuard {
            stop,
            handle: Mutex::new(Some(join)),
        })))
    }

    fn backend_name(&self) -> &'static str {
        "x11rb"
    }
}

/// Interactive actions emitted by the desktop System Tray menu or primary click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    ShowOrHideWindow,
    ToggleQuake,
    LaunchProfile(String),
    Quit,
}

/// Immutable tray telemetry synchronized from `AppState`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrayStateSnapshot {
    pub window_visible: bool,
    pub quake_visible: bool,
    pub active_tunnel_count: usize,
    pub active_transfer_count: usize,
    pub quick_profiles: Vec<(String, String)>,
}

impl TrayStateSnapshot {
    /// Produces the concise tooltip summary shown by system tray hosts.
    pub fn tooltip_text(&self) -> String {
        let win_label = if self.window_visible { "Visible" } else { "Hidden" };
        let quake_label = if self.quake_visible { "Open" } else { "Hidden" };
        format!(
            "Window: {win_label} · Quake: {quake_label} · Tunnels: {} · Transfers: {}",
            self.active_tunnel_count, self.active_transfer_count
        )
    }

    /// Returns ordered tray menu entries for D-Bus `com.canonical.dbusmenu` and inspection tests.
    pub fn menu_entries(&self) -> Vec<TrayMenuItem> {
        let mut items = vec![
            TrayMenuItem {
                id: 1,
                label: if self.window_visible {
                    "Hide Window".to_string()
                } else {
                    "Show Window".to_string()
                },
                enabled: true,
                action: Some(TrayAction::ShowOrHideWindow),
            },
            TrayMenuItem {
                id: 2,
                label: if self.quake_visible {
                    "Hide Quake Console".to_string()
                } else {
                    "Toggle Quake Console".to_string()
                },
                enabled: true,
                action: Some(TrayAction::ToggleQuake),
            },
            TrayMenuItem {
                id: 3,
                label: format!("Active Tunnels: {}", self.active_tunnel_count),
                enabled: false,
                action: None,
            },
        ];
        for (idx, (pid, name)) in self.quick_profiles.iter().take(8).enumerate() {
            items.push(TrayMenuItem {
                id: 100 + idx as i32,
                label: format!("Launch: {name}"),
                enabled: true,
                action: Some(TrayAction::LaunchProfile(pid.clone())),
            });
        }
        items.push(TrayMenuItem {
            id: 99,
            label: "Quit Termewnal".to_string(),
            enabled: true,
            action: Some(TrayAction::Quit),
        });
        items
    }
}

/// Flat structured item representation shared between D-Bus menu serialization and unit tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayMenuItem {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub action: Option<TrayAction>,
}

/// Backend boundary pushing [`TrayStateSnapshot`] updates to desktop tray hosts.
pub trait SystemTrayAdapter: Send + Sync {
    fn update_state(&self, snapshot: TrayStateSnapshot);
}

/// No-op tray backend used when tray is disabled or in minimal test boots.
#[derive(Debug, Default)]
pub struct NullSystemTray;

impl SystemTrayAdapter for NullSystemTray {
    fn update_state(&self, _snapshot: TrayStateSnapshot) {}
}

/// Test double recording every [`TrayStateSnapshot`] published by `AppState`.
#[derive(Debug, Default)]
pub struct RecordingSystemTray {
    snapshots: Mutex<Vec<TrayStateSnapshot>>,
}

impl RecordingSystemTray {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn snapshots(&self) -> Vec<TrayStateSnapshot> {
        self.snapshots
            .lock()
            .expect("recording tray mutex poisoned")
            .clone()
    }

    pub fn latest(&self) -> Option<TrayStateSnapshot> {
        self.snapshots
            .lock()
            .expect("recording tray mutex poisoned")
            .last()
            .cloned()
    }
}

impl SystemTrayAdapter for RecordingSystemTray {
    fn update_state(&self, snapshot: TrayStateSnapshot) {
        self.snapshots
            .lock()
            .expect("recording tray mutex poisoned")
            .push(snapshot);
    }
}

#[cfg(target_os = "linux")]
mod linux_sni {
    use std::sync::atomic::AtomicU32;
    use tokio::sync::Notify;
    use zbus::object_server::SignalEmitter;

    use super::*;

    struct SniService {
        snapshot: Arc<Mutex<TrayStateSnapshot>>,
        action_tx: mpsc::UnboundedSender<TrayAction>,
    }

    #[interface(name = "org.kde.StatusNotifierItem")]
    impl SniService {
        #[zbus(property)]
        fn category(&self) -> &str {
            "ApplicationStatus"
        }

        #[zbus(property)]
        fn id(&self) -> &str {
            "termewnal"
        }

        #[zbus(property)]
        fn title(&self) -> &str {
            "Termewnal"
        }

        #[zbus(property)]
        fn status(&self) -> &str {
            "Active"
        }

        #[zbus(property)]
        fn icon_name(&self) -> &str {
            "utilities-terminal"
        }

        #[zbus(property)]
        fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
            let mut argb = Vec::with_capacity(crate::window_chrome::APP_ICON_64_RGBA.len());
            for rgba in crate::window_chrome::APP_ICON_64_RGBA.chunks_exact(4) {
                argb.extend_from_slice(&[rgba[3], rgba[0], rgba[1], rgba[2]]);
            }
            let side = crate::window_chrome::APP_ICON_SIZE as i32;
            vec![(side, side, argb)]
        }

        #[zbus(property)]
        fn item_is_menu(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn menu(&self) -> ObjectPath<'_> {
            ObjectPath::from_static_str_unchecked("/MenuBar")
        }

        #[zbus(property)]
        fn tool_tip(&self) -> (String, Vec<(i32, i32, Vec<u8>)>, String, String) {
            let body = self
                .snapshot
                .lock()
                .expect("tray snapshot mutex")
                .tooltip_text();
            (
                "utilities-terminal".to_string(),
                Vec::new(),
                "Termewnal".to_string(),
                body,
            )
        }

        #[zbus(signal)]
        async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

        fn activate(&self, _x: i32, _y: i32) {
            let _ = self.action_tx.send(TrayAction::ShowOrHideWindow);
        }

        fn secondary_activate(&self, _x: i32, _y: i32) {
            let _ = self.action_tx.send(TrayAction::ToggleQuake);
        }
    }

    struct DbusMenuService {
        snapshot: Arc<Mutex<TrayStateSnapshot>>,
        action_tx: mpsc::UnboundedSender<TrayAction>,
        revision: Arc<AtomicU32>,
    }

    fn owned_str(s: &str) -> OwnedValue {
        Value::from(s)
            .try_to_owned()
            .expect("string converts to OwnedValue")
    }

    #[interface(name = "com.canonical.dbusmenu")]
    impl DbusMenuService {
        #[zbus(property)]
        fn version(&self) -> u32 {
            4
        }

        #[zbus(property)]
        fn status(&self) -> &str {
            "normal"
        }

        #[zbus(signal)]
        async fn layout_updated(
            emitter: &SignalEmitter<'_>,
            revision: u32,
            parent: i32,
        ) -> zbus::Result<()>;

        fn get_layout(
            &self,
            parent_id: i32,
            _recursion_depth: i32,
            _property_names: Vec<String>,
        ) -> (u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>)) {
            let rev = self.revision.load(Ordering::SeqCst);
            if parent_id != 0 {
                return (rev, (parent_id, HashMap::new(), Vec::new()));
            }
            let entries = self
                .snapshot
                .lock()
                .expect("tray snapshot mutex")
                .menu_entries();
            let mut children = Vec::with_capacity(entries.len());
            for entry in entries {
                let mut props = HashMap::new();
                props.insert("label".to_string(), owned_str(&entry.label));
                props.insert("enabled".to_string(), OwnedValue::from(entry.enabled));
                props.insert("visible".to_string(), OwnedValue::from(true));
                let child_node: (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>) =
                    (entry.id, props, Vec::new());
                if let Ok(val) = Value::from(child_node).try_to_owned() {
                    children.push(val);
                }
            }
            let mut root_props = HashMap::new();
            root_props.insert("children-display".to_string(), owned_str("submenu"));
            (rev, (0, root_props, children))
        }

        fn get_group_properties(
            &self,
            ids: Vec<i32>,
            _property_names: Vec<String>,
        ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
            let entries = self
                .snapshot
                .lock()
                .expect("tray snapshot mutex")
                .menu_entries();
            ids.into_iter()
                .filter_map(|id| {
                    entries.iter().find(|e| e.id == id).map(|e| {
                        let mut props = HashMap::new();
                        props.insert("label".to_string(), owned_str(&e.label));
                        props.insert("enabled".to_string(), OwnedValue::from(e.enabled));
                        (id, props)
                    })
                })
                .collect()
        }

        fn event(&self, id: i32, event_id: &str, _data: OwnedValue, _timestamp: u32) {
            if event_id != "clicked" {
                return;
            }
            let action = {
                let guard = self.snapshot.lock().expect("tray snapshot mutex");
                guard
                    .menu_entries()
                    .into_iter()
                    .find(|item| item.id == id)
                    .and_then(|item| item.action)
            };
            if let Some(act) = action {
                let _ = self.action_tx.send(act);
            }
        }

        fn about_to_show(&self, _id: i32) -> bool {
            true
        }
    }

    /// Linux FreeDesktop `org.kde.StatusNotifierItem` System Tray implementation over `zbus`.
    pub struct ZbusStatusNotifierItem {
        snapshot: Arc<Mutex<TrayStateSnapshot>>,
        revision: Arc<AtomicU32>,
        state_changed: Arc<Notify>,
        stop: Arc<AtomicBool>,
        thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    impl ZbusStatusNotifierItem {
        /// Spawns a background D-Bus StatusNotifierItem host registration when session D-Bus is available.
        ///
        /// Gracefully degrades on headless/non-DBus environments without panicking.
        pub fn spawn(action_tx: mpsc::UnboundedSender<TrayAction>) -> Arc<Self> {
            let snapshot = Arc::new(Mutex::new(TrayStateSnapshot::default()));
            let revision = Arc::new(AtomicU32::new(1));
            let state_changed = Arc::new(Notify::new());
            let stop = Arc::new(AtomicBool::new(false));
            let snap_bg = Arc::clone(&snapshot);
            let rev_bg = Arc::clone(&revision);
            let notify_bg = Arc::clone(&state_changed);
            let stop_bg = Arc::clone(&stop);

            let thread = std::thread::Builder::new()
                .name("termewnal-sni-tray".to_string())
                .spawn(move || {
                    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        return;
                    };
                    rt.block_on(async move {
                        let sni = SniService {
                            snapshot: Arc::clone(&snap_bg),
                            action_tx: action_tx.clone(),
                        };
                        let menu = DbusMenuService {
                            snapshot: snap_bg,
                            action_tx,
                            revision: Arc::clone(&rev_bg),
                        };
                        let bus_name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
                        let Ok(builder) = zbus::connection::Builder::session() else {
                            debug!("D-Bus session bus unavailable; skipping StatusNotifierItem registration");
                            return;
                        };
                        let Ok(builder) = builder.name(bus_name.as_str()) else {
                            return;
                        };
                        let Ok(builder) = builder.serve_at("/StatusNotifierItem", sni) else {
                            return;
                        };
                        let Ok(builder) = builder.serve_at("/MenuBar", menu) else {
                            return;
                        };
                        let Ok(conn) = builder.build().await else {
                            debug!("failed publishing StatusNotifierItem objects on session D-Bus");
                            return;
                        };
                        let _ = conn
                            .call_method(
                                Some("org.kde.StatusNotifierWatcher"),
                                "/StatusNotifierWatcher",
                                Some("org.kde.StatusNotifierWatcher"),
                                "RegisterStatusNotifierItem",
                                &(bus_name.as_str(),),
                            )
                            .await;
                        while !stop_bg.load(Ordering::SeqCst) {
                            tokio::select! {
                                _ = notify_bg.notified() => {
                                    let rev = rev_bg.load(Ordering::SeqCst);
                                    if let Ok(menu_iface) = conn.object_server().interface::<_, DbusMenuService>("/MenuBar").await {
                                        let _ = menu_iface.layout_updated(rev, 0).await;
                                    }
                                    if let Ok(sni_iface) = conn.object_server().interface::<_, SniService>("/StatusNotifierItem").await {
                                        let _ = sni_iface.new_tool_tip().await;
                                    }
                                }
                                _ = tokio::time::sleep(Duration::from_millis(150)) => {}
                            }
                        }
                    });
                })
                .ok();

            Arc::new(Self {
                snapshot,
                revision,
                state_changed,
                stop,
                thread: Mutex::new(thread),
            })
        }
    }

    impl Drop for ZbusStatusNotifierItem {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.state_changed.notify_one();
            if let Ok(mut guard) = self.thread.lock()
                && let Some(join) = guard.take()
            {
                let _ = join.join();
            }
        }
    }

    impl SystemTrayAdapter for ZbusStatusNotifierItem {
        fn update_state(&self, snapshot: TrayStateSnapshot) {
            let mut guard = self
                .snapshot
                .lock()
                .expect("tray snapshot mutex poisoned");
            if *guard != snapshot {
                *guard = snapshot;
                drop(guard);
                self.revision.fetch_add(1, Ordering::SeqCst);
                self.state_changed.notify_one();
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux_sni::ZbusStatusNotifierItem;
