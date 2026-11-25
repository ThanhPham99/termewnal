use std::env;
use std::path::{Path, PathBuf};

/// Recognized local shell family across supported operating systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    UnixShell,
    Pwsh,
    WindowsPowerShell,
    Cmd,
    GitBash,
    Wsl,
}

/// Launch specification for a detected local shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSpec {
    pub kind: ShellKind,
    pub program: String,
    pub args: Vec<String>,
    pub display_name: String,
}

impl ShellSpec {
    /// Creates an explicit [`ShellSpec`] from a user-configured shell path or command.
    pub fn custom(program: impl Into<String>, args: Vec<String>) -> Self {
        let program = program.into();
        let display_name = Path::new(&program)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&program)
            .to_string();
        Self {
            kind: ShellKind::UnixShell,
            program,
            args,
            display_name,
        }
    }
}

/// Auto-detects the default local shell for the current operating system.
pub fn detect_default_shell() -> ShellSpec {
    #[cfg(windows)]
    {
        detect_available_shells()
            .into_iter()
            .next()
            .unwrap_or_else(|| ShellSpec {
                kind: ShellKind::Cmd,
                program: env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
                args: Vec::new(),
                display_name: "Command Prompt".to_string(),
            })
    }

    #[cfg(not(windows))]
    {
        let shell_env = env::var("SHELL").ok();
        resolve_unix_shell(shell_env.as_deref(), is_usable_executable)
    }
}

/// Lists available local shells detected on the current operating system.
pub fn detect_available_shells() -> Vec<ShellSpec> {
    #[cfg(windows)]
    {
        let comspec = env::var("COMSPEC").ok();
        let program_files = env::var("ProgramFiles").ok();
        let system_root = env::var("SystemRoot").ok();
        let local_app_data = env::var("LOCALAPPDATA").ok();
        resolve_windows_shells(
            comspec.as_deref(),
            program_files.as_deref(),
            system_root.as_deref(),
            local_app_data.as_deref(),
            find_executable_on_system,
        )
    }

    #[cfg(not(windows))]
    {
        let mut shells = vec![detect_default_shell()];
        for candidate in ["/bin/bash", "/bin/zsh", "/usr/bin/fish", "/bin/sh"] {
            if is_usable_executable(Path::new(candidate))
                && !shells.iter().any(|s| s.program == candidate)
            {
                shells.push(ShellSpec::custom(candidate, Vec::new()));
            }
        }
        shells
    }
}

/// Resolves the Unix default shell from `$SHELL`, falling back to `/bin/sh` if unset or invalid.
pub fn resolve_unix_shell<F>(shell_env: Option<&str>, exists: F) -> ShellSpec
where
    F: Fn(&Path) -> bool,
{
    if let Some(raw) = shell_env.map(str::trim).filter(|s| !s.is_empty()) {
        let candidate = Path::new(raw);
        let valid = if candidate.is_absolute() || raw.contains('/') {
            exists(candidate)
        } else {
            find_in_path(raw, &exists).is_some()
        };

        if valid {
            return ShellSpec::custom(raw, Vec::new());
        }
    }

    ShellSpec::custom("/bin/sh", Vec::new())
}

/// Discovers Windows shells in priority order: `pwsh`, `powershell`, `cmd`, Git Bash, and WSL.
pub fn resolve_windows_shells<F>(
    comspec: Option<&str>,
    program_files: Option<&str>,
    system_root: Option<&str>,
    local_app_data: Option<&str>,
    find_executable: F,
) -> Vec<ShellSpec>
where
    F: Fn(&Path) -> Option<PathBuf>,
{
    let mut found = Vec::new();

    let pwsh_candidates = [
        Some(PathBuf::from("pwsh.exe")),
        program_files.map(|pf| join_windows_path(pf, r"PowerShell\7\pwsh.exe")),
    ];
    if let Some(path) = first_resolved(&pwsh_candidates, &find_executable) {
        found.push(ShellSpec {
            kind: ShellKind::Pwsh,
            program: path.to_string_lossy().into_owned(),
            args: Vec::new(),
            display_name: "PowerShell (pwsh)".to_string(),
        });
    }

    let powershell_candidates = [
        Some(PathBuf::from("powershell.exe")),
        system_root.map(|sr| {
            join_windows_path(sr, r"System32\WindowsPowerShell\v1.0\powershell.exe")
        }),
    ];
    if let Some(path) = first_resolved(&powershell_candidates, &find_executable) {
        found.push(ShellSpec {
            kind: ShellKind::WindowsPowerShell,
            program: path.to_string_lossy().into_owned(),
            args: Vec::new(),
            display_name: "Windows PowerShell".to_string(),
        });
    }

    let cmd_candidates = [
        comspec.filter(|s| !s.trim().is_empty()).map(PathBuf::from),
        system_root.map(|sr| join_windows_path(sr, r"System32\cmd.exe")),
        Some(PathBuf::from("cmd.exe")),
    ];
    if let Some(path) = first_resolved(&cmd_candidates, &find_executable) {
        found.push(ShellSpec {
            kind: ShellKind::Cmd,
            program: path.to_string_lossy().into_owned(),
            args: Vec::new(),
            display_name: "Command Prompt".to_string(),
        });
    }

    let git_bash_candidates = [
        program_files.map(|pf| join_windows_path(pf, r"Git\bin\bash.exe")),
        local_app_data.map(|la| join_windows_path(la, r"Programs\Git\bin\bash.exe")),
    ];
    if let Some(path) = first_resolved(&git_bash_candidates, &find_executable) {
        found.push(ShellSpec {
            kind: ShellKind::GitBash,
            program: path.to_string_lossy().into_owned(),
            args: Vec::new(),
            display_name: "Git Bash".to_string(),
        });
    }

    let wsl_candidates = [
        Some(PathBuf::from("wsl.exe")),
        system_root.map(|sr| join_windows_path(sr, r"System32\wsl.exe")),
    ];
    if let Some(path) = first_resolved(&wsl_candidates, &find_executable) {
        found.push(ShellSpec {
            kind: ShellKind::Wsl,
            program: path.to_string_lossy().into_owned(),
            args: Vec::new(),
            display_name: "WSL".to_string(),
        });
    }

    found
}

fn join_windows_path(base: &str, relative: &str) -> PathBuf {
    let trimmed = base.trim_end_matches(['\\', '/']);
    PathBuf::from(format!("{trimmed}\\{relative}"))
}

fn first_resolved<F>(candidates: &[Option<PathBuf>], resolver: &F) -> Option<PathBuf>
where
    F: Fn(&Path) -> Option<PathBuf>,
{
    candidates
        .iter()
        .flatten()
        .find_map(|candidate| resolver(candidate))
}

fn find_in_path<F>(binary: &str, exists: &F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let full = dir.join(binary);
        if exists(&full) {
            return Some(full);
        }
    }
    None
}

#[cfg(not(windows))]
fn is_usable_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(windows)]
fn find_executable_on_system(candidate: &Path) -> Option<PathBuf> {
    if candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    if candidate.components().count() == 1 {
        let name = candidate.to_str()?;
        return find_in_path(name, &|p| p.is_file());
    }
    None
}
