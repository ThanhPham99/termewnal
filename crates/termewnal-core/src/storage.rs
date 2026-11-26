use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::NamedTempFile;

use crate::config::CURRENT_SCHEMA_VERSION;
use crate::error::CoreError;

/// Number of rotating `.bak.1..=MAX_BACKUPS` files retained per persisted TOML file.
pub const MAX_BACKUPS: u8 = 3;

/// Computes the rotating backup path `<file>.bak.<slot>` (e.g. `config.toml.bak.1`).
pub fn backup_path(target: &Path, slot: u8) -> PathBuf {
    let mut os = OsString::from(target.as_os_str());
    os.push(format!(".bak.{slot}"));
    PathBuf::from(os)
}

/// Loads a TOML document from `path`, returning `T::default()` if the file does not exist yet.
pub(crate) fn load_toml_or_default<T>(
    path: &Path,
    schema_version: impl FnOnce(&T) -> u32,
) -> Result<T, CoreError>
where
    T: DeserializeOwned + Default,
{
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(T::default()),
        Err(source) => {
            return Err(CoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let parsed: T = toml::from_str(&raw).map_err(|err| CoreError::TomlDeserialize {
        path: path.to_path_buf(),
        message: err.message().to_string(),
        span: err.span(),
    })?;

    let found = schema_version(&parsed);
    if found != CURRENT_SCHEMA_VERSION {
        return Err(CoreError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            found,
            expected: CURRENT_SCHEMA_VERSION,
        });
    }

    Ok(parsed)
}

/// Serializes `value` to pretty TOML, rotates `.bak.1`..`.bak.3` backups if `path` already
/// exists, and atomically replaces `path` via sibling `tempfile` + `sync_all`.
pub(crate) fn save_toml_atomic<T>(
    path: &Path,
    value: &T,
    schema_version: u32,
) -> Result<(), CoreError>
where
    T: Serialize,
{
    if schema_version != CURRENT_SCHEMA_VERSION {
        return Err(CoreError::UnsupportedSchemaVersion {
            path: path.to_path_buf(),
            found: schema_version,
            expected: CURRENT_SCHEMA_VERSION,
        });
    }

    let serialized = toml::to_string_pretty(value).map_err(|source| CoreError::TomlSerialize {
        path: path.to_path_buf(),
        source,
    })?;

    save_bytes_atomic(path, serialized.as_bytes())
}

/// Atomically writes `bytes` to `path` via temp staging and rolling `.bak` / `.bak1` backups.
pub(crate) fn save_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    let parent = path.parent().ok_or_else(|| CoreError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "target path has no parent directory",
        ),
    })?;

    fs::create_dir_all(parent).map_err(|source| CoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    // Stage and fsync the new content before rotating backups so a failed write never shifts backups.
    let tmp = stage_temp_file(parent, bytes)?;

    if path.exists() {
        rotate_backups(path, parent)?;
    }

    tmp.persist(path).map_err(|err| CoreError::Io {
        path: path.to_path_buf(),
        source: err.error,
    })?;

    sync_parent_dir(parent);
    Ok(())
}

fn rotate_backups(target: &Path, parent: &Path) -> Result<(), CoreError> {
    let existing = fs::read(target).map_err(|source| CoreError::Io {
        path: target.to_path_buf(),
        source,
    })?;

    for slot in (1..MAX_BACKUPS).rev() {
        let src = backup_path(target, slot);
        if src.exists() {
            let dst = backup_path(target, slot + 1);
            fs::rename(&src, &dst).map_err(|source| CoreError::Io {
                path: dst,
                source,
            })?;
        }
    }

    let bak1 = backup_path(target, 1);
    let tmp = stage_temp_file(parent, &existing)?;
    tmp.persist(&bak1).map_err(|err| CoreError::Io {
        path: bak1,
        source: err.error,
    })?;
    Ok(())
}

pub(crate) fn stage_temp_file(parent: &Path, bytes: &[u8]) -> Result<NamedTempFile, CoreError> {
    let mut tmp = NamedTempFile::new_in(parent).map_err(|source| CoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    tmp.write_all(bytes).map_err(|source| CoreError::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;
    tmp.as_file().sync_all().map_err(|source| CoreError::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;
    Ok(tmp)
}

#[cfg(unix)]
pub(crate) fn sync_parent_dir(dir: &Path) {
    if let Ok(dir_file) = fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
}

#[cfg(not(unix))]
pub(crate) fn sync_parent_dir(_dir: &Path) {}
