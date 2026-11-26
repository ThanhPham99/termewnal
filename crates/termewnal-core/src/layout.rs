use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::{CURRENT_SCHEMA_VERSION, ConfigPaths};
use crate::error::CoreError;
use crate::profile::{
    DockerRuntime, NewlineMode, SerialFlowControl, SerialInputMode, SerialOutputMode, SerialParity,
};
use crate::storage::{load_toml_or_default, save_toml_atomic};

/// Axis along which a split container divides two child subtrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutSplitAxis {
    Horizontal,
    Vertical,
}

/// Serializable descriptor for restoring a local or remote `Session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionDescriptor {
    /// Local OS shell `Session` respawned via PTY, optionally at its captured `working_dir`.
    Local {
        #[serde(default)]
        profile_id: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        working_dir: Option<PathBuf>,
        #[serde(default)]
        shell: Option<String>,
    },
    /// Remote SSH `Session` restored in `Disconnected` state (or auto-connected per policy).
    Remote {
        #[serde(default)]
        profile_id: Option<String>,
        host: String,
        #[serde(default = "default_ssh_port")]
        port: u16,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        auto_connect_on_startup: bool,
    },
    /// Serial RS-232/USB `Session`.
    Serial {
        #[serde(default)]
        profile_id: Option<String>,
        port: String,
        baud_rate: u32,
        #[serde(default = "default_serial_data_bits")]
        data_bits: u8,
        #[serde(default)]
        parity: SerialParity,
        #[serde(default = "default_serial_stop_bits")]
        stop_bits: u8,
        #[serde(default)]
        flow_control: SerialFlowControl,
        #[serde(default)]
        input_mode: SerialInputMode,
        #[serde(default)]
        output_mode: SerialOutputMode,
        #[serde(default)]
        newline_mode: NewlineMode,
        #[serde(default)]
        auto_reconnect: bool,
        #[serde(default)]
        title: Option<String>,
    },
    /// Telnet TCP `Session`.
    Telnet {
        #[serde(default)]
        profile_id: Option<String>,
        host: String,
        #[serde(default = "default_telnet_port")]
        port: u16,
        #[serde(default)]
        auto_reconnect: bool,
        #[serde(default)]
        title: Option<String>,
    },
    /// Docker / Podman container exec `Session`.
    Docker {
        #[serde(default)]
        profile_id: Option<String>,
        #[serde(default)]
        runtime: DockerRuntime,
        container: String,
        #[serde(default)]
        shell: Option<String>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        working_dir: Option<String>,
        #[serde(default)]
        title: Option<String>,
    },
}

fn default_ssh_port() -> u16 {
    22
}

fn default_telnet_port() -> u16 {
    23
}

fn default_serial_data_bits() -> u8 {
    8
}

fn default_serial_stop_bits() -> u8 {
    1
}

/// Binary split-tree node persisted for a workspace tab.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayoutSplitNode {
    Leaf {
        session: SessionDescriptor,
    },
    Split {
        axis: LayoutSplitAxis,
        ratio: f32,
        first: Box<LayoutSplitNode>,
        second: Box<LayoutSplitNode>,
    },
}

/// Persisted state for a single workspace tab.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabLayout {
    #[serde(default)]
    pub title: Option<String>,
    /// Optional custom hex color accent for this tab.
    #[serde(default)]
    pub color: Option<String>,
    /// Whether this tab is pinned (prevented from accidental close-others/close-right).
    #[serde(default)]
    pub pinned: bool,
    /// 0-based leaf traversal index of the focused pane inside `root`.
    #[serde(default)]
    pub focused_pane: usize,
    /// Optional 0-based leaf traversal index of the zoomed pane inside `root`.
    #[serde(default)]
    pub zoomed_pane: Option<usize>,
    pub root: LayoutSplitNode,
}

/// Persisted state for a single application window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowLayout {
    pub width: u32,
    pub height: u32,
    pub active_tab: usize,
    pub tabs: Vec<TabLayout>,
}

impl Default for WindowLayout {
    fn default() -> Self {
        Self {
            width: 1200,
            height: 800,
            active_tab: 0,
            tabs: Vec::new(),
        }
    }
}

/// Top-level `layout.toml` schema capturing multi-window workspace geometry across restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceLayout {
    pub schema_version: u32,
    pub windows: Vec<WindowLayout>,
}

impl Default for WorkspaceLayout {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            windows: Vec::new(),
        }
    }
}

/// Loads `layout.toml` from `paths`, returning an empty [`WorkspaceLayout`] if missing.
pub fn load_layout(paths: &ConfigPaths) -> Result<WorkspaceLayout, CoreError> {
    load_toml_or_default(&paths.layout_file(), |layout: &WorkspaceLayout| {
        layout.schema_version
    })
}

/// Atomically writes `layout` to `layout.toml` under `paths`, rotating backups if present.
pub fn save_layout(paths: &ConfigPaths, layout: &WorkspaceLayout) -> Result<(), CoreError> {
    save_toml_atomic(&paths.layout_file(), layout, layout.schema_version)
}
