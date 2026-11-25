use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, DebouncedEventBatch, Debouncer, new_debouncer_opt};
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;

use crate::config::{AppConfig, CURRENT_SCHEMA_VERSION, ConfigPaths, ensure_config_files};
use crate::error::CoreError;
use crate::profile::ProfilesConfig;
use crate::storage::{backup_path, stage_temp_file, sync_parent_dir};

/// Default filesystem event debounce window for external TOML edits.
pub const DEFAULT_RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// Identifies which configuration file changed or failed to reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConfigFileKind {
    /// `config.toml` (`AppConfig`).
    AppConfig,
    /// `profiles.toml` (`ProfilesConfig`).
    Profiles,
}

impl ConfigFileKind {
    /// Returns the canonical filename (`config.toml` or `profiles.toml`).
    pub fn file_name(self) -> &'static str {
        match self {
            Self::AppConfig => "config.toml",
            Self::Profiles => "profiles.toml",
        }
    }

    /// Resolves the full file path under `paths`.
    pub fn path_in(self, paths: &ConfigPaths) -> PathBuf {
        match self {
            Self::AppConfig => paths.config_file(),
            Self::Profiles => paths.profiles_file(),
        }
    }
}

/// Structured diagnostic returned when an external TOML edit fails validation or parsing.
///
/// The active in-memory configuration is preserved and the user's file on disk is never
/// overwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlReloadError {
    pub kind: ConfigFileKind,
    pub path: PathBuf,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
    pub backup_available: bool,
}

impl TomlReloadError {
    /// Formats a concise user-facing location + message string suitable for a UI error toast.
    pub fn display_location(&self) -> String {
        match (self.line, self.column) {
            (Some(line), Some(col)) => format!("line {line}, column {col}"),
            (Some(line), None) => format!("line {line}"),
            _ => "file".to_string(),
        }
    }
}

impl fmt::Display for TomlReloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.column) {
            (Some(line), Some(col)) => write!(
                f,
                "Failed to reload {} at line {}, column {}: {}",
                self.kind.file_name(),
                line,
                col,
                self.message
            ),
            (Some(line), None) => write!(
                f,
                "Failed to reload {} at line {}: {}",
                self.kind.file_name(),
                line,
                self.message
            ),
            _ => write!(
                f,
                "Failed to reload {}: {}",
                self.kind.file_name(),
                self.message
            ),
        }
    }
}

impl std::error::Error for TomlReloadError {}

/// Validated snapshot produced by [`restore_from_backup`].
#[derive(Debug, Clone, PartialEq)]
pub enum RestoredSnapshot {
    AppConfig(AppConfig),
    Profiles(ProfilesConfig),
}

/// Parses and validates `config.toml` from disk without mutating any files.
pub fn reload_config_snapshot(paths: &ConfigPaths) -> Result<AppConfig, TomlReloadError> {
    reload_toml_snapshot(
        ConfigFileKind::AppConfig,
        &paths.config_file(),
        |cfg: &AppConfig| cfg.schema_version,
    )
}

/// Parses and validates `profiles.toml` from disk without mutating any files.
pub fn reload_profiles_snapshot(paths: &ConfigPaths) -> Result<ProfilesConfig, TomlReloadError> {
    reload_toml_snapshot(
        ConfigFileKind::Profiles,
        &paths.profiles_file(),
        |profiles: &ProfilesConfig| profiles.schema_version,
    )
}

fn reload_toml_snapshot<T>(
    kind: ConfigFileKind,
    path: &Path,
    schema_version: impl FnOnce(&T) -> u32,
) -> Result<T, TomlReloadError>
where
    T: DeserializeOwned,
{
    let bak1_exists = backup_path(path, 1).is_file();
    let raw = fs::read_to_string(path).map_err(|err| TomlReloadError {
        kind,
        path: path.to_path_buf(),
        line: None,
        column: None,
        message: err.to_string(),
        backup_available: bak1_exists,
    })?;

    let parsed: T = toml::from_str(&raw).map_err(|err| {
        let (line, column) = err
            .span()
            .map(|span| translate_position(&raw, span.start))
            .map(|(l, c)| (Some(l), Some(c)))
            .unwrap_or((None, None));
        TomlReloadError {
            kind,
            path: path.to_path_buf(),
            line,
            column,
            message: err.message().to_string(),
            backup_available: bak1_exists,
        }
    })?;

    let found = schema_version(&parsed);
    if found != CURRENT_SCHEMA_VERSION {
        return Err(TomlReloadError {
            kind,
            path: path.to_path_buf(),
            line: None,
            column: None,
            message: format!(
                "unsupported schema_version {found} (expected {CURRENT_SCHEMA_VERSION})"
            ),
            backup_available: bak1_exists,
        });
    }

    Ok(parsed)
}

/// Restores `<file>.bak.1` back to the primary TOML file atomically without overwriting `.bak.1`
/// with the malformed user edit.
pub fn restore_from_backup(
    paths: &ConfigPaths,
    kind: ConfigFileKind,
) -> Result<RestoredSnapshot, CoreError> {
    let target_path = kind.path_in(paths);
    let bak1_path = backup_path(&target_path, 1);

    let raw_bak = fs::read_to_string(&bak1_path).map_err(|source| CoreError::Io {
        path: bak1_path.clone(),
        source,
    })?;

    let snapshot = match kind {
        ConfigFileKind::AppConfig => {
            let cfg: AppConfig =
                toml::from_str(&raw_bak).map_err(|err| CoreError::TomlDeserialize {
                    path: bak1_path.clone(),
                    message: err.message().to_string(),
                    span: err.span(),
                })?;
            if cfg.schema_version != CURRENT_SCHEMA_VERSION {
                return Err(CoreError::UnsupportedSchemaVersion {
                    path: bak1_path,
                    found: cfg.schema_version,
                    expected: CURRENT_SCHEMA_VERSION,
                });
            }
            RestoredSnapshot::AppConfig(cfg)
        }
        ConfigFileKind::Profiles => {
            let profiles: ProfilesConfig =
                toml::from_str(&raw_bak).map_err(|err| CoreError::TomlDeserialize {
                    path: bak1_path.clone(),
                    message: err.message().to_string(),
                    span: err.span(),
                })?;
            if profiles.schema_version != CURRENT_SCHEMA_VERSION {
                return Err(CoreError::UnsupportedSchemaVersion {
                    path: bak1_path,
                    found: profiles.schema_version,
                    expected: CURRENT_SCHEMA_VERSION,
                });
            }
            RestoredSnapshot::Profiles(profiles)
        }
    };

    let parent = target_path.parent().ok_or_else(|| CoreError::Io {
        path: target_path.clone(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "target path has no parent directory",
        ),
    })?;
    fs::create_dir_all(parent).map_err(|source| CoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    // Stage and persist directly over target_path without rotating backups so broken edits
    // never overwrite `.bak.1`.
    let tmp = stage_temp_file(parent, raw_bak.as_bytes())?;
    tmp.persist(&target_path).map_err(|err| CoreError::Io {
        path: target_path.clone(),
        source: err.error,
    })?;
    sync_parent_dir(parent);

    Ok(snapshot)
}

/// Stateful helper maintaining last-known-valid `AppConfig` and `ProfilesConfig` snapshots across
/// hot-reload attempts and `.bak.1` recoveries.
#[derive(Debug, Clone)]
pub struct ConfigReloader {
    paths: ConfigPaths,
    config: AppConfig,
    profiles: ProfilesConfig,
    config_error: Option<TomlReloadError>,
    profiles_error: Option<TomlReloadError>,
}

impl ConfigReloader {
    /// Creates a [`ConfigReloader`] from preloaded snapshots.
    pub fn new(paths: ConfigPaths, config: AppConfig, profiles: ProfilesConfig) -> Self {
        Self {
            paths,
            config,
            profiles,
            config_error: None,
            profiles_error: None,
        }
    }

    /// Initializes/loads `config.toml` and `profiles.toml` under `paths`.
    pub fn load(paths: ConfigPaths) -> Result<Self, CoreError> {
        let (config, profiles) = ensure_config_files(&paths)?;
        Ok(Self::new(paths, config, profiles))
    }

    /// Returns the active valid [`AppConfig`].
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Returns the active valid [`ProfilesConfig`].
    pub fn profiles(&self) -> &ProfilesConfig {
        &self.profiles
    }

    /// Returns the active reload diagnostic for `kind`, if the last reload failed.
    pub fn error_for(&self, kind: ConfigFileKind) -> Option<&TomlReloadError> {
        match kind {
            ConfigFileKind::AppConfig => self.config_error.as_ref(),
            ConfigFileKind::Profiles => self.profiles_error.as_ref(),
        }
    }

    /// Safely reloads `kind` from disk. Returns `Ok(true)` if the parsed config changed, `Ok(false)`
    /// if identical, or `Err(TomlReloadError)` while keeping the previous valid state active.
    pub fn reload_file(&mut self, kind: ConfigFileKind) -> Result<bool, TomlReloadError> {
        match kind {
            ConfigFileKind::AppConfig => match reload_config_snapshot(&self.paths) {
                Ok(next) => {
                    let changed = next != self.config;
                    self.config = next;
                    self.config_error = None;
                    Ok(changed)
                }
                Err(err) => {
                    self.config_error = Some(err.clone());
                    Err(err)
                }
            },
            ConfigFileKind::Profiles => match reload_profiles_snapshot(&self.paths) {
                Ok(next) => {
                    let changed = next != self.profiles;
                    self.profiles = next;
                    self.profiles_error = None;
                    Ok(changed)
                }
                Err(err) => {
                    self.profiles_error = Some(err.clone());
                    Err(err)
                }
            },
        }
    }

    /// Restores `kind` from `<file>.bak.1`, activates the restored snapshot, and clears any error.
    pub fn restore_backup(&mut self, kind: ConfigFileKind) -> Result<(), CoreError> {
        match restore_from_backup(&self.paths, kind)? {
            RestoredSnapshot::AppConfig(cfg) => {
                self.config = cfg;
                self.config_error = None;
            }
            RestoredSnapshot::Profiles(profiles) => {
                self.profiles = profiles;
                self.profiles_error = None;
            }
        }
        Ok(())
    }
}

/// Notifications emitted by [`ConfigWatcher`] when watched TOML files change on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWatchEvent {
    Changed(Vec<ConfigFileKind>),
    Rescan,
    Error(String),
}

/// Parent-directory file watcher using `notify` + `notify-debouncer-mini` so atomic editor saves
/// (temp file + rename) are detected reliably without inode breakage.
pub struct ConfigWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
}

impl ConfigWatcher {
    /// Starts watching `paths.config_dir()` (`RecursiveMode::NonRecursive`) with [`DEFAULT_RELOAD_DEBOUNCE`].
    pub fn start(
        paths: ConfigPaths,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ConfigWatchEvent>), CoreError> {
        Self::start_with_debounce(paths, DEFAULT_RELOAD_DEBOUNCE)
    }

    /// Starts watching `paths.config_dir()` with a custom debounce interval.
    pub fn start_with_debounce(
        paths: ConfigPaths,
        debounce: Duration,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ConfigWatchEvent>), CoreError> {
        let config_dir = paths.config_dir().to_path_buf();
        fs::create_dir_all(&config_dir).map_err(|source| CoreError::Io {
            path: config_dir.clone(),
            source,
        })?;

        let (tx, rx) = mpsc::unbounded_channel();
        let debouncer_cfg = notify_debouncer_mini::Config::default()
            .with_timeout(debounce)
            .with_batch_mode(false);

        let mut debouncer = new_debouncer_opt::<_, RecommendedWatcher>(
            debouncer_cfg,
            move |res: DebounceEventResult| {
                if let Some(ev) = translate_debounced_result(res) {
                    let _ = tx.send(ev);
                }
            },
        )
        .map_err(|err| CoreError::Io {
            path: config_dir.clone(),
            source: io::Error::other(err.to_string()),
        })?;

        debouncer
            .watcher()
            .watch(&config_dir, RecursiveMode::NonRecursive)
            .map_err(|err| CoreError::Io {
                path: config_dir,
                source: io::Error::other(err.to_string()),
            })?;

        Ok((
            Self {
                _debouncer: debouncer,
            },
            rx,
        ))
    }
}

fn translate_debounced_result(res: DebounceEventResult) -> Option<ConfigWatchEvent> {
    match res {
        Ok(DebouncedEventBatch::Events(events)) => {
            let mut kinds = Vec::new();
            for event in events {
                let Some(name) = event.path.file_name() else {
                    continue;
                };
                if name == OsStr::new("config.toml") && !kinds.contains(&ConfigFileKind::AppConfig) {
                    kinds.push(ConfigFileKind::AppConfig);
                } else if name == OsStr::new("profiles.toml")
                    && !kinds.contains(&ConfigFileKind::Profiles)
                {
                    kinds.push(ConfigFileKind::Profiles);
                }
            }
            (!kinds.is_empty()).then_some(ConfigWatchEvent::Changed(kinds))
        }
        Ok(DebouncedEventBatch::Rescan) => Some(ConfigWatchEvent::Rescan),
        Ok(_) => Some(ConfigWatchEvent::Rescan),
        Err(err) => Some(ConfigWatchEvent::Error(err.to_string())),
    }
}

/// Converts a 0-based byte offset in `input` into 1-based `(line, column)` character coordinates.
fn translate_position(input: &str, byte_index: usize) -> (usize, usize) {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return (1, byte_index + 1);
    }

    if byte_index >= bytes.len() && bytes.last() == Some(&b'\n') {
        let newline_count = bytes.iter().filter(|&&b| b == b'\n').count();
        return (newline_count + 1, 1);
    }

    let safe_index = byte_index.min(bytes.len() - 1);
    let column_offset = byte_index - safe_index;

    let line_start = bytes[..safe_index]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let line_zero = bytes[..line_start].iter().filter(|&&b| b == b'\n').count();

    let col_zero = std::str::from_utf8(&bytes[line_start..=safe_index])
        .map(|s| s.chars().count().saturating_sub(1))
        .unwrap_or(safe_index - line_start);

    (line_zero + 1, col_zero + column_offset + 1)
}

#[cfg(test)]
mod tests {
    use super::translate_position;

    #[test]
    fn translate_position_handles_empty_utf8_and_trailing_newline_eof() {
        assert_eq!(translate_position("", 0), (1, 1));
        assert_eq!(translate_position("abc\n", 4), (2, 1));
        let utf8 = "name = \"ệ\"\nbad = true\n";
        let bad_offset = utf8.find("bad").unwrap();
        assert_eq!(translate_position(utf8, bad_offset), (2, 1));
    }
}
