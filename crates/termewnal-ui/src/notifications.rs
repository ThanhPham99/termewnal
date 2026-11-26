//! Desktop notification abstraction for background terminal bell alerts (`org.freedesktop.Notifications`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::debug;

/// Sends non-blocking desktop notifications on terminal bell / transfer completion.
pub trait DesktopNotifier: Send + Sync {
    fn notify(&self, summary: &str, body: &str);
}

/// No-op notifier used when notifications are disabled.
#[derive(Debug, Default)]
pub struct NullDesktopNotifier;

impl DesktopNotifier for NullDesktopNotifier {
    fn notify(&self, _summary: &str, _body: &str) {}
}

/// Test double capturing emitted desktop notifications in memory.
#[derive(Debug, Default)]
pub struct RecordingDesktopNotifier {
    events: Mutex<Vec<(String, String)>>,
}

impl RecordingDesktopNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take_notifications(&self) -> Vec<(String, String)> {
        let mut guard = self.events.lock().expect("notifier mutex poisoned");
        std::mem::take(&mut *guard)
    }

    pub fn notifications(&self) -> Vec<(String, String)> {
        self.events.lock().expect("notifier mutex poisoned").clone()
    }
}

impl DesktopNotifier for RecordingDesktopNotifier {
    fn notify(&self, summary: &str, body: &str) {
        self.events
            .lock()
            .expect("notifier mutex poisoned")
            .push((summary.to_string(), body.to_string()));
    }
}

/// Linux FreeDesktop D-Bus notifier via `zbus` (`org.freedesktop.Notifications`).
#[derive(Debug, Default)]
pub struct ZbusDesktopNotifier;

impl ZbusDesktopNotifier {
    pub fn shared() -> Arc<dyn DesktopNotifier> {
        Arc::new(Self)
    }
}

impl DesktopNotifier for ZbusDesktopNotifier {
    fn notify(&self, summary: &str, body: &str) {
        let summary = summary.to_string();
        let body = body.to_string();
        let send_fut = async move {
            let Ok(conn) = zbus::Connection::session().await else {
                debug!("no D-Bus session bus available for desktop notification");
                return;
            };
            let actions: Vec<&str> = Vec::new();
            let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
            let _ = conn
                .call_method(
                    Some("org.freedesktop.Notifications"),
                    "/org/freedesktop/Notifications",
                    Some("org.freedesktop.Notifications"),
                    "Notify",
                    &(
                        "Termewnal",
                        0_u32,
                        "utilities-terminal",
                        summary.as_str(),
                        body.as_str(),
                        actions,
                        hints,
                        5000_i32,
                    ),
                )
                .await;
        };

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(send_fut);
        } else {
            std::thread::spawn(move || {
                let _ = zbus::block_on(send_fut);
            });
        }
    }
}
