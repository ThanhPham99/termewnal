//! Persisted `QuickCommand` snippets (`quick_commands.toml`) and named `WorkspaceTemplate`s (`workspaces.toml`).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{CURRENT_SCHEMA_VERSION, ConfigPaths};
use crate::error::CoreError;
use crate::layout::TabLayout;
use crate::storage::{load_toml_or_default, save_toml_atomic};

/// Reusable command snippet persisted in `quick_commands.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickCommand {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub group: Option<String>,
    pub command: String,
    #[serde(default = "default_true")]
    pub append_newline: bool,
    #[serde(default)]
    pub broadcast: bool,
    #[serde(default)]
    pub confirm_before_run: bool,
}

impl QuickCommand {
    /// Creates a new [`QuickCommand`] with a generated UUID identifier.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            group: None,
            command: command.into(),
            append_newline: true,
            broadcast: false,
            confirm_before_run: false,
        }
    }

    /// Formats the bytes to write into a terminal session.
    pub fn payload_bytes(&self) -> Vec<u8> {
        let mut out = self.command.as_bytes().to_vec();
        if self.append_newline && !out.ends_with(b"\r") && !out.ends_with(b"\n") {
            out.push(b'\r');
        }
        out
    }
}

fn default_true() -> bool {
    true
}

/// Top-level `quick_commands.toml` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuickCommandsConfig {
    pub schema_version: u32,
    pub commands: Vec<QuickCommand>,
}

impl Default for QuickCommandsConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            commands: Vec::new(),
        }
    }
}

/// Loads `quick_commands.toml` from `paths`, returning an empty list if missing.
pub fn load_quick_commands(paths: &ConfigPaths) -> Result<QuickCommandsConfig, CoreError> {
    load_toml_or_default(&paths.quick_commands_file(), |cfg: &QuickCommandsConfig| {
        cfg.schema_version
    })
}

/// Atomically saves `quick_commands.toml` under `paths` with rotating backups.
pub fn save_quick_commands(
    paths: &ConfigPaths,
    commands: &QuickCommandsConfig,
) -> Result<(), CoreError> {
    save_toml_atomic(
        &paths.quick_commands_file(),
        commands,
        commands.schema_version,
    )
}

/// Named multi-tab / split-pane [`WorkspaceTemplate`] saved in `workspaces.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceTemplate {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub active_tab: usize,
    #[serde(default)]
    pub tabs: Vec<TabLayout>,
}

impl WorkspaceTemplate {
    /// Creates a new [`WorkspaceTemplate`] with a generated UUID identifier.
    pub fn new(name: impl Into<String>, tabs: Vec<TabLayout>, active_tab: usize) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            description: None,
            active_tab,
            tabs,
        }
    }
}

/// Top-level `workspaces.toml` document holding saved [`WorkspaceTemplate`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspacesConfig {
    pub schema_version: u32,
    pub workspaces: Vec<WorkspaceTemplate>,
}

impl Default for WorkspacesConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            workspaces: Vec::new(),
        }
    }
}

/// Loads `workspaces.toml` from `paths`, returning an empty list if missing.
pub fn load_workspaces(paths: &ConfigPaths) -> Result<WorkspacesConfig, CoreError> {
    load_toml_or_default(&paths.workspaces_file(), |cfg: &WorkspacesConfig| {
        cfg.schema_version
    })
}

/// Atomically saves `workspaces.toml` under `paths` with rotating backups.
pub fn save_workspaces(paths: &ConfigPaths, workspaces: &WorkspacesConfig) -> Result<(), CoreError> {
    save_toml_atomic(
        &paths.workspaces_file(),
        workspaces,
        workspaces.schema_version,
    )
}
