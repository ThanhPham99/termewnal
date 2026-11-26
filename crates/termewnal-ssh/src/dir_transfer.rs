use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpClientError;
use russh_sftp::protocol::{FileAttributes, StatusCode};
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

use crate::error::SshError;
use crate::sftp::join_remote_path;
use crate::transfer::{TransferId, TransferNotice};

/// Summary returned after recursively traversing and enqueueing a directory `Transfer`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DirectoryTransferSummary {
    pub transfer_ids: Vec<TransferId>,
    pub created_dir_count: usize,
    pub skipped_symlinks: Vec<String>,
    pub skipped_symlink_loops: Vec<String>,
    pub permission_notices: Vec<TransferNotice>,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteDirModeSpec {
    pub remote_path: String,
    pub permissions: Option<u32>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalUploadFileSpec {
    pub local_path: PathBuf,
    pub remote_path: String,
    pub total_bytes: u64,
}

#[derive(Debug, Default)]
pub(crate) struct LocalUploadWalkPlan {
    pub dirs: Vec<RemoteDirModeSpec>,
    pub files: Vec<LocalUploadFileSpec>,
    pub skipped_symlinks: Vec<String>,
    pub skipped_symlink_loops: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteDownloadFileSpec {
    pub remote_path: String,
    pub local_path: PathBuf,
    pub total_bytes: u64,
}

#[derive(Debug, Default)]
pub(crate) struct RemoteDownloadWalkPlan {
    pub dirs: Vec<(PathBuf, Option<u32>)>,
    pub files: Vec<RemoteDownloadFileSpec>,
    pub skipped_symlinks: Vec<String>,
    pub skipped_symlink_loops: Vec<String>,
}

pub(crate) struct UploadDirBatch {
    dirs: Vec<RemoteDirModeSpec>,
    dirs_ready: AsyncMutex<bool>,
    pending_ids: Mutex<HashSet<TransferId>>,
}

impl UploadDirBatch {
    pub fn new(dirs: Vec<RemoteDirModeSpec>, file_ids: &[TransferId]) -> Self {
        Self {
            dirs,
            dirs_ready: AsyncMutex::new(false),
            pending_ids: Mutex::new(file_ids.iter().copied().collect()),
        }
    }

    pub async fn ensure_dirs_created(&self, sftp: &SftpSession) -> Result<(), SshError> {
        let mut ready = self.dirs_ready.lock().await;
        if *ready {
            return Ok(());
        }
        for spec in &self.dirs {
            ensure_remote_dir_exists(sftp, &spec.remote_path).await?;
        }
        *ready = true;
        Ok(())
    }

    pub async fn finish_file_and_finalize_dirs(
        &self,
        id: TransferId,
        sftp: &SftpSession,
    ) -> Vec<TransferNotice> {
        let all_done = {
            let mut guard = self.pending_ids.lock().expect("pending_ids mutex poisoned");
            guard.remove(&id);
            guard.is_empty()
        };
        if all_done {
            self.apply_dir_permissions_bottom_up(sftp).await
        } else {
            Vec::new()
        }
    }

    pub async fn apply_dir_permissions_bottom_up(&self, sftp: &SftpSession) -> Vec<TransferNotice> {
        let mut notices = Vec::new();
        for spec in self.dirs.iter().rev() {
            if let Some(mode) = spec.permissions
                && let Err(err) = apply_remote_permissions(sftp, &spec.remote_path, mode).await
            {
                warn!(
                    remote_path = %spec.remote_path,
                    mode,
                    error = %err,
                    "failed preserving remote directory permissions"
                );
                notices.push(TransferNotice::PermissionPreserveSkipped {
                    reason: format!("{}: {err}", spec.remote_path),
                });
            }
        }
        notices
    }
}

pub(crate) struct DownloadDirBatch {
    dirs: Vec<(PathBuf, Option<u32>)>,
    pending_ids: Mutex<HashSet<TransferId>>,
}

impl DownloadDirBatch {
    pub fn new(dirs: Vec<(PathBuf, Option<u32>)>, file_ids: &[TransferId]) -> Self {
        Self {
            dirs,
            pending_ids: Mutex::new(file_ids.iter().copied().collect()),
        }
    }

    pub async fn finish_file_and_finalize_dirs(&self, id: TransferId) -> Vec<TransferNotice> {
        let all_done = {
            let mut guard = self.pending_ids.lock().expect("pending_ids mutex poisoned");
            guard.remove(&id);
            guard.is_empty()
        };
        if all_done {
            self.apply_dir_permissions_bottom_up().await
        } else {
            Vec::new()
        }
    }

    pub async fn apply_dir_permissions_bottom_up(&self) -> Vec<TransferNotice> {
        let mut notices = Vec::new();
        for (dir_path, permissions) in self.dirs.iter().rev() {
            if let Some(mode) = permissions
                && let Err(err) = apply_local_permissions(dir_path, *mode).await
            {
                warn!(
                    local_path = %dir_path.display(),
                    mode,
                    error = %err,
                    "failed preserving local directory permissions"
                );
                notices.push(TransferNotice::PermissionPreserveSkipped {
                    reason: format!("{}: {err}", dir_path.display()),
                });
            }
        }
        notices
    }
}

pub(crate) fn extract_local_posix_mode(meta: &fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

pub(crate) async fn apply_local_permissions(path: &Path, mode: u32) -> Result<(), SshError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(mode & 0o7777);
        tokio::fs::set_permissions(path, perms).await?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

pub(crate) async fn apply_remote_permissions(
    sftp: &SftpSession,
    remote_path: &str,
    mode: u32,
) -> Result<(), SshError> {
    let attrs = FileAttributes {
        permissions: Some(mode & 0o7777),
        ..Default::default()
    };
    sftp.set_metadata(remote_path, attrs).await?;
    Ok(())
}

async fn ensure_remote_dir_exists(sftp: &SftpSession, remote_dir: &str) -> Result<(), SshError> {
    match sftp.metadata(remote_dir).await {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(SshError::Sftp(format!(
            "remote path '{remote_dir}' exists and is not a directory"
        ))),
        Err(SftpClientError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
            match sftp.create_dir(remote_dir).await {
                Ok(()) => Ok(()),
                Err(mkdir_err) => {
                    if sftp.metadata(remote_dir).await.is_ok_and(|m| m.is_dir()) {
                        Ok(())
                    } else {
                        Err(mkdir_err.into())
                    }
                }
            }
        }
        Err(err) => Err(err.into()),
    }
}

/// Safely walks `local_dir` in breadth-first order (visiting real subdirectories before symlinked
/// subdirectories so siblings never get shadowed), preserving POSIX modes and guarding symlinks.
pub(crate) fn collect_local_dir_plan(
    local_dir: &Path,
    remote_dir: &str,
    follow_symlinks: bool,
) -> Result<LocalUploadWalkPlan, SshError> {
    let mut plan = LocalUploadWalkPlan::default();
    let root_lstat = fs::symlink_metadata(local_dir)?;
    if root_lstat.file_type().is_symlink() && !follow_symlinks {
        plan.skipped_symlinks
            .push(local_dir.to_string_lossy().into_owned());
        return Ok(plan);
    }

    let root_stat = fs::metadata(local_dir)?;
    if !root_stat.is_dir() {
        return Err(SshError::Sftp(format!(
            "local path '{}' is not a directory",
            local_dir.display()
        )));
    }
    let root_canon = fs::canonicalize(local_dir)?;
    let mut visited_dirs = HashSet::new();
    visited_dirs.insert(root_canon);

    plan.dirs.push(RemoteDirModeSpec {
        remote_path: remote_dir.to_string(),
        permissions: extract_local_posix_mode(&root_stat),
    });

    let mut queue = VecDeque::new();
    queue.push_back((local_dir.to_path_buf(), remote_dir.to_string()));

    while let Some((curr_local, curr_remote)) = queue.pop_front() {
        let mut entries: Vec<_> = fs::read_dir(&curr_local)?.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());

        let mut real_dirs = Vec::new();
        let mut symlink_entries = Vec::new();

        for entry in entries {
            let child_local = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_remote = join_remote_path(&curr_remote, &name);
            let lstat = match fs::symlink_metadata(&child_local) {
                Ok(m) => m,
                Err(_) => continue,
            };

            if lstat.file_type().is_symlink() {
                symlink_entries.push((child_local, child_remote));
            } else if lstat.is_dir() {
                if let Ok(canon) = fs::canonicalize(&child_local)
                    && visited_dirs.insert(canon)
                {
                    plan.dirs.push(RemoteDirModeSpec {
                        remote_path: child_remote.clone(),
                        permissions: extract_local_posix_mode(&lstat),
                    });
                    real_dirs.push((child_local, child_remote));
                }
            } else if lstat.is_file() {
                plan.files.push(LocalUploadFileSpec {
                    local_path: child_local,
                    remote_path: child_remote,
                    total_bytes: lstat.len(),
                });
            }
        }

        for dir_item in real_dirs {
            queue.push_back(dir_item);
        }

        for (link_local, link_remote) in symlink_entries {
            let link_display = link_local.to_string_lossy().into_owned();
            let loop_detected = is_local_symlink_loop(&link_local, &visited_dirs);
            if !follow_symlinks {
                plan.skipped_symlinks.push(link_display.clone());
                if loop_detected {
                    plan.skipped_symlink_loops.push(link_display);
                }
                continue;
            }

            let Ok(target_meta) = fs::metadata(&link_local) else {
                // Dangling symlink.
                plan.skipped_symlinks.push(link_display);
                continue;
            };
            if target_meta.is_dir() {
                let Ok(canon) = fs::canonicalize(&link_local) else {
                    plan.skipped_symlinks.push(link_display);
                    continue;
                };
                if !visited_dirs.insert(canon) {
                    plan.skipped_symlinks.push(link_display.clone());
                    plan.skipped_symlink_loops.push(link_display);
                    continue;
                }
                plan.dirs.push(RemoteDirModeSpec {
                    remote_path: link_remote.clone(),
                    permissions: extract_local_posix_mode(&target_meta),
                });
                queue.push_back((link_local, link_remote));
            } else if target_meta.is_file() {
                plan.files.push(LocalUploadFileSpec {
                    local_path: link_local,
                    remote_path: link_remote,
                    total_bytes: target_meta.len(),
                });
            }
        }
    }

    Ok(plan)
}

fn is_local_symlink_loop(link_path: &Path, visited_dirs: &HashSet<PathBuf>) -> bool {
    let Ok(target_canon) = fs::canonicalize(link_path) else {
        return false;
    };
    if visited_dirs.contains(&target_canon) {
        return true;
    }
    if let Some(parent) = link_path.parent()
        && let Ok(parent_canon) = fs::canonicalize(parent)
        && parent_canon.starts_with(&target_canon)
    {
        return true;
    }
    false
}

/// Safely walks `remote_dir` in breadth-first order via SFTP `lstat`/`read_dir`/`canonicalize`,
/// preserving remote POSIX modes and skipping symlinks & symlink cycles.
pub(crate) async fn collect_remote_dir_plan(
    sftp: &SftpSession,
    remote_dir: &str,
    local_dir: &Path,
    follow_symlinks: bool,
) -> Result<RemoteDownloadWalkPlan, SshError> {
    let mut plan = RemoteDownloadWalkPlan::default();
    let root_lstat = sftp.symlink_metadata(remote_dir).await?;
    if root_lstat.is_symlink() && !follow_symlinks {
        plan.skipped_symlinks.push(remote_dir.to_string());
        return Ok(plan);
    }

    let root_stat = sftp.metadata(remote_dir).await?;
    if !root_stat.is_dir() {
        return Err(SshError::Sftp(format!(
            "remote path '{remote_dir}' is not a directory"
        )));
    }
    let root_canon = resolve_remote_canonical(sftp, remote_dir).await;
    let mut visited_dirs = HashSet::new();
    visited_dirs.insert(root_canon);

    plan.dirs.push((
        local_dir.to_path_buf(),
        root_stat.permissions.map(|m| m & 0o7777),
    ));

    let mut queue = VecDeque::new();
    queue.push_back((remote_dir.to_string(), local_dir.to_path_buf()));

    while let Some((curr_remote, curr_local)) = queue.pop_front() {
        let mut entries: Vec<_> = sftp.read_dir(curr_remote.as_str()).await?.collect();
        entries.sort_by_key(|e| e.file_name());

        let mut real_dirs = Vec::new();
        let mut symlink_entries = Vec::new();

        for entry in entries {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            crate::sftp::validate_remote_entry_name(&name)?;
            let child_remote = join_remote_path(&curr_remote, &name);
            let child_local = curr_local.join(&name);

            let Ok(lstat) = sftp.symlink_metadata(child_remote.as_str()).await else {
                continue;
            };

            if lstat.is_symlink() {
                symlink_entries.push((child_remote, child_local));
            } else if lstat.is_dir() {
                let canon = resolve_remote_canonical(sftp, &child_remote).await;
                if visited_dirs.insert(canon) {
                    plan.dirs
                        .push((child_local.clone(), lstat.permissions.map(|m| m & 0o7777)));
                    real_dirs.push((child_remote, child_local));
                }
            } else if lstat.is_regular() {
                plan.files.push(RemoteDownloadFileSpec {
                    remote_path: child_remote,
                    local_path: child_local,
                    total_bytes: lstat.len(),
                });
            }
        }

        for dir_item in real_dirs {
            queue.push_back(dir_item);
        }

        for (link_remote, link_local) in symlink_entries {
            let resolved_target = resolve_remote_symlink_target(sftp, &link_remote).await;
            let loop_detected = is_remote_symlink_loop(
                &curr_remote,
                resolved_target.as_deref(),
                &visited_dirs,
            );
            if !follow_symlinks {
                plan.skipped_symlinks.push(link_remote.clone());
                if loop_detected {
                    plan.skipped_symlink_loops.push(link_remote);
                }
                continue;
            }

            let Ok(target_meta) = sftp.metadata(link_remote.as_str()).await else {
                plan.skipped_symlinks.push(link_remote);
                continue;
            };
            if target_meta.is_dir() {
                let canon = resolved_target
                    .unwrap_or_else(|| normalize_posix_path(&link_remote));
                if !visited_dirs.insert(canon) {
                    plan.skipped_symlinks.push(link_remote.clone());
                    plan.skipped_symlink_loops.push(link_remote);
                    continue;
                }
                plan.dirs.push((
                    link_local.clone(),
                    target_meta.permissions.map(|m| m & 0o7777),
                ));
                queue.push_back((link_remote, link_local));
            } else if target_meta.is_regular() {
                plan.files.push(RemoteDownloadFileSpec {
                    remote_path: link_remote,
                    local_path: link_local,
                    total_bytes: target_meta.len(),
                });
            }
        }
    }

    Ok(plan)
}

async fn resolve_remote_canonical(sftp: &SftpSession, path: &str) -> String {
    match sftp.canonicalize(path).await {
        Ok(canon) => normalize_posix_path(&canon),
        Err(_) => normalize_posix_path(path),
    }
}

async fn resolve_remote_symlink_target(sftp: &SftpSession, link_path: &str) -> Option<String> {
    if let Ok(canon) = sftp.canonicalize(link_path).await {
        let norm_canon = normalize_posix_path(&canon);
        let norm_link = normalize_posix_path(link_path);
        if norm_canon != norm_link {
            return Some(norm_canon);
        }
    }
    if let Ok(raw_target) = sftp.read_link(link_path).await {
        let joined = if raw_target.starts_with('/') {
            raw_target
        } else {
            let parent = crate::sftp::parent_remote_path(link_path);
            join_remote_path(&parent, &raw_target)
        };
        return Some(normalize_posix_path(&joined));
    }
    None
}

fn is_remote_symlink_loop(
    curr_dir: &str,
    resolved_target: Option<&str>,
    visited_dirs: &HashSet<String>,
) -> bool {
    let Some(target) = resolved_target else {
        return false;
    };
    if visited_dirs.contains(target) {
        return true;
    }
    let norm_curr = normalize_posix_path(curr_dir);
    norm_curr == target
        || norm_curr
            .strip_prefix(target)
            .is_some_and(|rest| target == "/" || rest.starts_with('/'))
}

fn normalize_posix_path(raw: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in Path::new(raw).components() {
        match comp {
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn download_dir_batch_emits_permission_preserve_skipped_when_chmod_fails() {
        let missing = PathBuf::from("/nonexistent_termewnal_perm_test_dir_12345");
        let batch = DownloadDirBatch::new(vec![(missing, Some(0o755))], &[TransferId(1)]);
        let notices = batch.finish_file_and_finalize_dirs(TransferId(1)).await;
        assert_eq!(notices.len(), 1);
        assert!(matches!(
            notices[0],
            TransferNotice::PermissionPreserveSkipped { .. }
        ));
    }
}
