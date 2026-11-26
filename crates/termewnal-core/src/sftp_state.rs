use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{CURRENT_SCHEMA_VERSION, ConfigPaths};
use crate::error::CoreError;
use crate::storage::{load_toml_or_default, save_toml_atomic};

/// Persisted per-profile SFTP state stored in `sftp_state.toml` under `data_local_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SftpState {
    pub schema_version: u32,
    /// Maps saved SSH `profile_id` -> last active remote SFTP working directory.
    pub last_profile_dirs: BTreeMap<String, String>,
    /// Bookmarked local directories shown in the SFTP+ Local pane.
    #[serde(default)]
    pub local_bookmarks: Vec<String>,
    /// Maps saved SSH `profile_id` -> bookmarked remote directories.
    #[serde(default)]
    pub profile_bookmarks: BTreeMap<String, Vec<String>>,
}

impl Default for SftpState {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            last_profile_dirs: BTreeMap::new(),
            local_bookmarks: Vec::new(),
            profile_bookmarks: BTreeMap::new(),
        }
    }
}

impl SftpState {
    /// Returns the last remembered remote SFTP directory for `profile_id`, if any.
    pub fn last_dir(&self, profile_id: &str) -> Option<&str> {
        self.last_profile_dirs.get(profile_id).map(String::as_str)
    }

    /// Records `path` as the last working SFTP directory for `profile_id`.
    /// Returns `true` when the persisted entry changed.
    pub fn record_dir(&mut self, profile_id: impl Into<String>, path: impl AsRef<str>) -> bool {
        let pid = profile_id.into();
        let clean = path.as_ref().trim();
        if pid.trim().is_empty() || clean.is_empty() {
            return false;
        }
        if self.last_profile_dirs.get(&pid).map(String::as_str) == Some(clean) {
            return false;
        }
        self.last_profile_dirs.insert(pid, clean.to_string());
        true
    }

    /// Returns bookmarked local directory paths.
    pub fn local_bookmarks(&self) -> &[String] {
        &self.local_bookmarks
    }

    /// Adds or removes `path` from local bookmarks; returns `true` if now bookmarked.
    pub fn toggle_local_bookmark(&mut self, path: impl AsRef<str>) -> bool {
        let clean = path.as_ref().trim();
        if clean.is_empty() {
            return false;
        }
        if let Some(pos) = self.local_bookmarks.iter().position(|p| p == clean) {
            self.local_bookmarks.remove(pos);
            false
        } else {
            self.local_bookmarks.push(clean.to_string());
            true
        }
    }

    /// Returns bookmarked remote directory paths for `profile_id`.
    pub fn remote_bookmarks(&self, profile_id: &str) -> &[String] {
        self.profile_bookmarks
            .get(profile_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Adds or removes `path` from `profile_id` remote bookmarks; returns `true` if now bookmarked.
    pub fn toggle_remote_bookmark(
        &mut self,
        profile_id: impl Into<String>,
        path: impl AsRef<str>,
    ) -> bool {
        let pid = profile_id.into();
        let clean = path.as_ref().trim();
        if pid.trim().is_empty() || clean.is_empty() {
            return false;
        }
        let list = self.profile_bookmarks.entry(pid).or_default();
        if let Some(pos) = list.iter().position(|p| p == clean) {
            list.remove(pos);
            false
        } else {
            list.push(clean.to_string());
            true
        }
    }

    /// Removes any remembered SFTP directory or bookmarks for a deleted `profile_id`.
    pub fn remove_profile(&mut self, profile_id: &str) -> bool {
        let a = self.last_profile_dirs.remove(profile_id).is_some();
        let b = self.profile_bookmarks.remove(profile_id).is_some();
        a || b
    }
}

/// Loads `sftp_state.toml` from `paths`, returning [`SftpState::default()`] if missing.
pub fn load_sftp_state(paths: &ConfigPaths) -> Result<SftpState, CoreError> {
    load_toml_or_default(&paths.sftp_state_file(), |state: &SftpState| {
        state.schema_version
    })
}

/// Atomically saves `state` to `sftp_state.toml` under `paths`, rotating backups if present.
pub fn save_sftp_state(paths: &ConfigPaths, state: &SftpState) -> Result<(), CoreError> {
    save_toml_atomic(&paths.sftp_state_file(), state, state.schema_version)
}
