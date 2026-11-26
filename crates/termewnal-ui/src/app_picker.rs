//! Cross-platform installed application discovery and FreeDesktop launcher helpers
//! for the SFTP+ `Open with...` application chooser.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::sftp_plus::FileBrowserEntry;

/// Injectable installed-application catalog builder for SFTP+ `Open with...` (overridden in tests).
pub type AppCatalogProvider = Arc<dyn Fn(&FileBrowserEntry) -> Vec<InstalledApp> + Send + Sync>;

/// Installed desktop application entry presented in the SFTP+ `Open with...` application chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    /// Stable identifier (FreeDesktop desktop ID on Linux, bundle/file path elsewhere).
    pub id: String,
    /// Human-readable application display name (e.g. `"Visual Studio Code"`).
    pub name: String,
    /// Optional generic name or short description shown underneath the title.
    pub subtitle: Option<String>,
    /// Launch target passed to [`crate::app::AppOpener`] (`.desktop` file, `.app` bundle, or binary).
    pub launch_path: PathBuf,
    /// Short executable / command hint rendered on the right side of the row.
    pub command_label: String,
    /// Lowercased declared MIME types supported by this application.
    pub mime_types: Vec<String>,
    /// True when configured as the user/system default handler for the target item.
    pub is_default: bool,
    /// True when associated with the target item's MIME type or directory handler.
    pub is_recommended: bool,
    /// True when the application runs inside a terminal (`Terminal=true`).
    pub terminal: bool,
}

/// Parsed fields extracted strictly from the `[Desktop Entry]` group of a `.desktop` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDesktopEntry {
    pub name: String,
    pub generic_name: Option<String>,
    pub comment: Option<String>,
    pub exec: String,
    pub mime_types: Vec<String>,
    pub terminal: bool,
}

/// Discovers installed OS applications ranked cleanly for `entry`.
pub fn discover_installed_apps_for_target(entry: &FileBrowserEntry) -> Vec<InstalledApp> {
    #[cfg(target_os = "linux")]
    {
        discover_linux_installed_apps(entry)
    }
    #[cfg(target_os = "macos")]
    {
        discover_macos_installed_apps(entry)
    }
    #[cfg(target_os = "windows")]
    {
        discover_windows_installed_apps(entry)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = entry;
        Vec::new()
    }
}

/// Filters `apps` case-insensitively across `name`, `subtitle`, `command_label`, and `launch_path`.
pub fn filter_installed_apps<'a>(apps: &'a [InstalledApp], query: &str) -> Vec<&'a InstalledApp> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return apps.iter().collect();
    }
    apps.iter()
        .filter(|app| {
            app.name.to_ascii_lowercase().contains(&needle)
                || app.command_label.to_ascii_lowercase().contains(&needle)
                || app
                    .subtitle
                    .as_deref()
                    .is_some_and(|s| s.to_ascii_lowercase().contains(&needle))
                || app
                    .launch_path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(&needle)
        })
        .collect()
}

/// Resolves a clean human-readable display name for `app_path` (reading `.desktop` `Name=` on Linux).
pub fn display_name_for_app_path(app_path: &Path) -> String {
    if app_path.extension().and_then(|e| e.to_str()) == Some("desktop")
        && let Some(parsed) = parse_desktop_entry(app_path)
    {
        return parsed.name;
    }
    if app_path.extension().and_then(|e| e.to_str()) == Some("app")
        && let Some(stem) = app_path.file_stem()
    {
        return stem.to_string_lossy().into_owned();
    }
    app_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| app_path.display().to_string())
}

/// Tokenizes a FreeDesktop `Exec=` command line respecting single/double quotes and backslash escapes.
pub fn tokenize_desktop_exec(exec_line: &str) -> Vec<String> {
    let mut raw_tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = exec_line.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if ch == '\\' && q == '"' {
                    if let Some(next_ch) = chars.next() {
                        current.push(next_ch);
                    }
                } else if ch == q {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                } else if ch == '\\' {
                    if let Some(next_ch) = chars.next() {
                        current.push(next_ch);
                    }
                } else if ch.is_whitespace() {
                    if !current.is_empty() {
                        raw_tokens.push(std::mem::take(&mut current));
                    }
                } else {
                    current.push(ch);
                }
            }
        }
    }
    if !current.is_empty() {
        raw_tokens.push(current);
    }
    raw_tokens
}

/// Parses a FreeDesktop `Exec=` command line into argv tokens, stripping `%f`/`%u` field codes.
pub fn parse_desktop_exec_tokens(exec_line: &str) -> Vec<String> {
    tokenize_desktop_exec(exec_line)
        .into_iter()
        .filter_map(|tok| {
            let trimmed = tok.trim();
            if trimmed.starts_with('%') && trimmed.len() == 2 && trimmed != "%%" {
                None
            } else {
                Some(tok.replace("%%", "%"))
            }
        })
        .collect()
}

/// Builds runnable argv tokens from a `.desktop` `Exec=` line and a local `target_path`.
///
/// Substitutes `%f`/`%F`/`%u`/`%U` in-place (preserving wrapper markers such as Flatpak
/// `@@u %U @@`), strips other FreeDesktop field codes, and appends `target_path` if no file/URL
/// placeholder existed.
pub fn build_desktop_exec_argv(exec_line: &str, target_path: &Path) -> Vec<String> {
    let target_str = target_path.to_string_lossy().into_owned();
    let mut argv = Vec::new();
    let mut inserted_target = false;

    for tok in tokenize_desktop_exec(exec_line) {
        let trimmed = tok.trim();
        if matches!(trimmed, "%f" | "%F" | "%u" | "%U") {
            if !inserted_target {
                argv.push(target_str.clone());
                inserted_target = true;
            }
            continue;
        }
        if trimmed.starts_with('%') && trimmed.len() == 2 && trimmed != "%%" {
            continue;
        }
        let unescaped = tok.replace("%%", "%");
        argv.push(unescaped);
    }

    if !inserted_target && !argv.is_empty() {
        argv.push(target_str);
    }
    argv
}

/// Parses `[Desktop Entry]` metadata from a FreeDesktop `.desktop` file on disk.
///
/// Enforces `Type=Application`, skips `NoDisplay=true` / `Hidden=true`, checks `OnlyShowIn` /
/// `NotShowIn` against `XDG_CURRENT_DESKTOP` when present, and validates `TryExec` when declared.
pub fn parse_desktop_entry(path: &Path) -> Option<ParsedDesktopEntry> {
    let content = fs::read_to_string(path).ok()?;
    let current_desktops: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .unwrap_or_default()
        .split([':', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    parse_desktop_entry_str(&content, &current_desktops)
}

fn parse_desktop_entry_str(content: &str, current_desktops: &[String]) -> Option<ParsedDesktopEntry> {
    let mut in_desktop_entry = false;
    let mut seen_desktop_entry = false;

    let mut entry_type: Option<String> = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut name: Option<String> = None;
    let mut generic_name: Option<String> = None;
    let mut comment: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut try_exec: Option<String> = None;
    let mut mime_types: Vec<String> = Vec::new();
    let mut terminal = false;
    let mut only_show_in: Vec<String> = Vec::new();
    let mut not_show_in: Vec<String> = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            if line == "[Desktop Entry]" {
                in_desktop_entry = true;
                seen_desktop_entry = true;
                continue;
            } else if seen_desktop_entry {
                break;
            } else {
                in_desktop_entry = false;
                continue;
            }
        }
        if !in_desktop_entry {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match key {
            "Type" => entry_type = Some(val.to_string()),
            "NoDisplay" => no_display = val.eq_ignore_ascii_case("true"),
            "Hidden" => hidden = val.eq_ignore_ascii_case("true"),
            "Name" => name = Some(val.to_string()),
            "GenericName" => {
                if !val.is_empty() {
                    generic_name = Some(val.to_string());
                }
            }
            "Comment" => {
                if !val.is_empty() {
                    comment = Some(val.to_string());
                }
            }
            "Exec" => {
                if !val.is_empty() {
                    exec = Some(val.to_string());
                }
            }
            "TryExec" => {
                if !val.is_empty() {
                    try_exec = Some(val.to_string());
                }
            }
            "MimeType" => {
                mime_types = val
                    .split(';')
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(|m| m.to_ascii_lowercase())
                    .collect();
            }
            "Terminal" => terminal = val.eq_ignore_ascii_case("true"),
            "OnlyShowIn" => {
                only_show_in = split_desktop_list(val);
            }
            "NotShowIn" => {
                not_show_in = split_desktop_list(val);
            }
            _ => {}
        }
    }

    if entry_type.as_deref() != Some("Application") || no_display || hidden {
        return None;
    }
    if !current_desktops.is_empty() {
        if !only_show_in.is_empty()
            && !only_show_in
                .iter()
                .any(|d| current_desktops.iter().any(|cur| cur == d))
        {
            return None;
        }
        if !not_show_in.is_empty()
            && not_show_in
                .iter()
                .any(|d| current_desktops.iter().any(|cur| cur == d))
        {
            return None;
        }
    }
    if let Some(te) = try_exec.as_deref()
        && !executable_exists_in_path(te)
    {
        return None;
    }

    let name = name.filter(|n| !n.is_empty())?;
    let exec = exec.filter(|e| !e.is_empty())?;
    if parse_desktop_exec_tokens(&exec).is_empty() {
        return None;
    }

    Some(ParsedDesktopEntry {
        name,
        generic_name,
        comment,
        exec,
        mime_types,
        terminal,
    })
}

fn split_desktop_list(raw: &str) -> Vec<String> {
    raw.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn executable_exists_in_path(cmd: &str) -> bool {
    let p = Path::new(cmd);
    if p.is_absolute() || cmd.contains(std::path::MAIN_SEPARATOR) {
        return p.is_file();
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(cmd).is_file())
}

fn extract_command_label(exec_line: &str) -> String {
    let tokens = parse_desktop_exec_tokens(exec_line);
    let mut iter = tokens.iter().peekable();
    while let Some(tok) = iter.peek() {
        let base = Path::new(tok.as_str())
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(tok.as_str());
        if base == "env" || (tok.contains('=') && !tok.starts_with('/') && !tok.starts_with('-')) {
            iter.next();
            continue;
        }
        if base == "flatpak" {
            if let Some(app_id) = tokens
                .iter()
                .rev()
                .find(|t| !t.starts_with('-') && !t.starts_with('@') && t.contains('.'))
            {
                return app_id.clone();
            }
        }
        return base.to_string();
    }
    exec_line.to_string()
}

/// Discovers Linux desktop applications from standard XDG directories and ranks them against `entry`.
#[cfg(target_os = "linux")]
fn discover_linux_installed_apps(entry: &FileBrowserEntry) -> Vec<InstalledApp> {
    let mut app_dirs: Vec<PathBuf> = Vec::new();
    let mut mimeapps_paths: Vec<PathBuf> = Vec::new();

    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        mimeapps_paths.push(config_home.join("mimeapps.list"));
    }

    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    {
        let user_apps = data_home.join("applications");
        mimeapps_paths.push(user_apps.join("mimeapps.list"));
        mimeapps_paths.push(user_apps.join("mimeinfo.cache"));
        app_dirs.push(user_apps);
    }

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let user_flatpak = home.join(".local/share/flatpak/exports/share/applications");
        mimeapps_paths.push(user_flatpak.join("mimeinfo.cache"));
        app_dirs.push(user_flatpak);
    }

    let data_dirs_raw = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| {
        "/usr/local/share:/usr/share:/var/lib/flatpak/exports/share:/var/lib/snapd/desktop"
            .to_string()
    });
    for dir in std::env::split_paths(&data_dirs_raw) {
        let apps = dir.join("applications");
        mimeapps_paths.push(apps.join("mimeapps.list"));
        mimeapps_paths.push(apps.join("mimeinfo.cache"));
        app_dirs.push(apps);
    }
    for extra in [
        "/usr/share/applications",
        "/usr/local/share/applications",
        "/var/lib/flatpak/exports/share/applications",
        "/var/lib/snapd/desktop/applications",
    ] {
        let p = PathBuf::from(extra);
        if !app_dirs.contains(&p) {
            mimeapps_paths.push(p.join("mimeinfo.cache"));
            app_dirs.push(p);
        }
    }
    mimeapps_paths.push(PathBuf::from("/etc/xdg/mimeapps.list"));

    let globs2 = Path::new("/usr/share/mime/globs2");
    discover_linux_apps_from_dirs(
        &app_dirs,
        globs2.is_file().then_some(globs2),
        &mimeapps_paths,
        entry,
    )
}

/// Pure, filesystem-parameterized Linux `.desktop` + XDG MIME application discovery.
pub fn discover_linux_apps_from_dirs(
    app_dirs: &[PathBuf],
    mime_globs_path: Option<&Path>,
    mimeapps_paths: &[PathBuf],
    entry: &FileBrowserEntry,
) -> Vec<InstalledApp> {
    let candidate_mimes = infer_candidate_mime_types(&entry.name, entry.is_dir, mime_globs_path);
    let (default_ids, assoc_ids) = collect_mime_associations(&candidate_mimes, mimeapps_paths);

    let mut seen_desktop_ids = BTreeSet::new();
    let mut seen_name_cmds = BTreeSet::new();
    let mut apps = Vec::new();

    for dir in app_dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut desktop_files = Vec::new();
        collect_desktop_files_recursive(dir, dir, &mut desktop_files);
        desktop_files.sort_by(|a, b| a.0.cmp(&b.0));

        for (desktop_id, full_path) in desktop_files {
            if !seen_desktop_ids.insert(desktop_id.clone()) {
                continue;
            }
            let Some(parsed) = parse_desktop_entry(&full_path) else {
                continue;
            };
            let command_label = extract_command_label(&parsed.exec);
            let dedup_key = (
                parsed.name.to_ascii_lowercase(),
                command_label.to_ascii_lowercase(),
            );
            if !seen_name_cmds.insert(dedup_key) {
                continue;
            }
            let subtitle = parsed
                .generic_name
                .as_deref()
                .filter(|g| !g.eq_ignore_ascii_case(&parsed.name))
                .map(str::to_string)
                .or_else(|| {
                    parsed
                        .comment
                        .as_deref()
                        .filter(|c| !c.eq_ignore_ascii_case(&parsed.name) && c.len() <= 80)
                        .map(str::to_string)
                });
            let is_default = default_ids.contains(&desktop_id);
            let is_recommended = !is_default
                && (assoc_ids.contains(&desktop_id)
                    || candidate_mimes
                        .iter()
                        .any(|m| parsed.mime_types.iter().any(|pm| pm == m)));

            apps.push(InstalledApp {
                id: desktop_id,
                name: parsed.name,
                subtitle,
                launch_path: full_path,
                command_label,
                mime_types: parsed.mime_types,
                is_default,
                is_recommended,
                terminal: parsed.terminal,
            });
        }
    }

    apps.sort_by(|a, b| {
        (!a.is_default, !a.is_recommended, a.name.to_ascii_lowercase(), &a.id).cmp(&(
            !b.is_default,
            !b.is_recommended,
            b.name.to_ascii_lowercase(),
            &b.id,
        ))
    });
    apps
}

fn collect_desktop_files_recursive(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for item in rd.flatten() {
        let path = item.path();
        if path.is_dir() {
            collect_desktop_files_recursive(root, &path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("desktop")
            && let Ok(rel) = path.strip_prefix(root)
        {
            let desktop_id = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("-");
            out.push((desktop_id, path));
        }
    }
}

/// Infers ordered candidate MIME types for a file name or directory.
pub fn infer_candidate_mime_types(
    file_name: &str,
    is_dir: bool,
    globs2_path: Option<&Path>,
) -> Vec<String> {
    if is_dir {
        return vec![
            "inode/directory".to_string(),
            "x-directory/normal".to_string(),
        ];
    }

    let lower = file_name.to_ascii_lowercase();
    let mut mimes: Vec<String> = Vec::new();
    let mut push_unique = |m: &str| {
        let m = m.trim().to_ascii_lowercase();
        if !m.is_empty() && !mimes.iter().any(|existing| existing == &m) {
            mimes.push(m);
        }
    };

    if let Some(globs_file) = globs2_path
        && let Ok(raw) = fs::read_to_string(globs_file)
    {
        let mut matches: Vec<(u32, usize, String)> = Vec::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(3, ':');
            let Some(prio_s) = parts.next() else { continue };
            let Some(mime) = parts.next() else { continue };
            let Some(glob) = parts.next() else { continue };
            let Ok(prio) = prio_s.parse::<u32>() else { continue };
            if let Some(suffix) = glob.strip_prefix('*')
                && !suffix.contains(['*', '?', '['])
                && lower.ends_with(&suffix.to_ascii_lowercase())
            {
                matches.push((prio, suffix.len(), mime.to_string()));
            } else if !glob.contains(['*', '?', '[']) && lower == glob.to_ascii_lowercase() {
                matches.push((prio, glob.len() + 100, mime.to_string()));
            }
        }
        matches.sort_by(|a, b| (b.1, b.0).cmp(&(a.1, a.0)));
        for (_, _, mime) in matches {
            push_unique(&mime);
        }
    }

    let ext = Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let text_like = matches!(
        ext,
        "txt"
            | "md"
            | "markdown"
            | "log"
            | "conf"
            | "cfg"
            | "ini"
            | "env"
            | "toml"
            | "yaml"
            | "yml"
            | "json"
            | "jsonc"
            | "xml"
            | "csv"
            | "tsv"
            | "sql"
            | "rs"
            | "py"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "jsx"
            | "go"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "java"
            | "kt"
            | "rb"
            | "php"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "css"
            | "scss"
            | "html"
            | "htm"
            | "svg"
            | "service"
            | "dockerfile"
    ) || matches!(
        lower.as_str(),
        "readme"
            | "license"
            | "makefile"
            | "dockerfile"
            | "cargo.toml"
            | "cargo.lock"
            | ".gitignore"
            | ".bashrc"
            | ".zshrc"
            | ".profile"
            | ".env"
    );

    match ext {
        "txt" | "log" | "conf" | "cfg" | "ini" | "env" => push_unique("text/plain"),
        "md" | "markdown" => push_unique("text/markdown"),
        "json" | "jsonc" => push_unique("application/json"),
        "toml" => push_unique("application/toml"),
        "yaml" | "yml" => push_unique("application/yaml"),
        "xml" => push_unique("application/xml"),
        "csv" => push_unique("text/csv"),
        "sql" => push_unique("application/sql"),
        "html" | "htm" => push_unique("text/html"),
        "css" => push_unique("text/css"),
        "js" | "mjs" | "cjs" => push_unique("text/javascript"),
        "ts" | "tsx" => push_unique("text/typescript"),
        "rs" => push_unique("text/rust"),
        "py" => push_unique("text/x-python"),
        "sh" | "bash" | "zsh" => push_unique("application/x-shellscript"),
        "pdf" => push_unique("application/pdf"),
        "png" => push_unique("image/png"),
        "jpg" | "jpeg" => push_unique("image/jpeg"),
        "gif" => push_unique("image/gif"),
        "webp" => push_unique("image/webp"),
        "svg" => push_unique("image/svg+xml"),
        "mp3" => push_unique("audio/mpeg"),
        "wav" => push_unique("audio/wav"),
        "ogg" => push_unique("audio/ogg"),
        "mp4" => push_unique("video/mp4"),
        "mkv" => push_unique("video/x-matroska"),
        "webm" => push_unique("video/webm"),
        "zip" => push_unique("application/zip"),
        "gz" => push_unique("application/gzip"),
        "tar" => push_unique("application/x-tar"),
        _ => {}
    }

    drop(push_unique);
    if (text_like || mimes.iter().any(|m| m.starts_with("text/")))
        && !mimes.iter().any(|m| m == "text/plain")
    {
        mimes.push("text/plain".to_string());
    }

    mimes
}

fn collect_mime_associations(
    candidate_mimes: &[String],
    mimeapps_paths: &[PathBuf],
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut defaults_by_mime: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut assoc_by_mime: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for path in mimeapps_paths {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let mut section = String::new();
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line.to_string();
                continue;
            }
            let Some((mime, rhs)) = line.split_once('=') else {
                continue;
            };
            let mime = mime.trim().to_ascii_lowercase();
            if !candidate_mimes.iter().any(|m| m == &mime) {
                continue;
            }
            let ids: Vec<String> = rhs
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.ends_with(".desktop"))
                .map(str::to_string)
                .collect();
            if ids.is_empty() {
                continue;
            }
            match section.as_str() {
                "[Default Applications]" => {
                    defaults_by_mime.entry(mime).or_insert(ids);
                }
                "[Added Associations]" | "[MIME Cache]" => {
                    assoc_by_mime.entry(mime).or_default().extend(ids);
                }
                _ => {}
            }
        }
    }

    let mut default_ids = BTreeSet::new();
    let mut assoc_ids = BTreeSet::new();
    for mime in candidate_mimes {
        if let Some(ids) = defaults_by_mime.get(mime) {
            if let Some(first) = ids.first() {
                default_ids.insert(first.clone());
            }
            for id in ids.iter().skip(1) {
                assoc_ids.insert(id.clone());
            }
        }
        if let Some(ids) = assoc_by_mime.get(mime) {
            for id in ids {
                assoc_ids.insert(id.clone());
            }
        }
    }
    (default_ids, assoc_ids)
}

#[cfg(target_os = "macos")]
fn discover_macos_installed_apps(entry: &FileBrowserEntry) -> Vec<InstalledApp> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
    ];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join("Applications"));
    }

    let mut seen = BTreeSet::new();
    let mut apps = Vec::new();
    for root in roots {
        collect_macos_bundles(&root, 2, &mut seen, &mut apps, entry);
    }
    apps.sort_by(|a, b| {
        (!a.is_recommended, a.name.to_ascii_lowercase(), &a.id).cmp(&(
            !b.is_recommended,
            b.name.to_ascii_lowercase(),
            &b.id,
        ))
    });
    apps
}

#[cfg(target_os = "macos")]
fn collect_macos_bundles(
    dir: &Path,
    depth: usize,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<InstalledApp>,
    entry: &FileBrowserEntry,
) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for item in rd.flatten() {
        let path = item.path();
        if !path.is_dir() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("app") {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let key = name.to_ascii_lowercase();
            if !seen.insert(key.clone()) {
                continue;
            }
            let is_recommended = if entry.is_dir {
                matches!(
                    key.as_str(),
                    "finder" | "visual studio code" | "cursor" | "zed" | "antigravity"
                )
            } else {
                matches!(
                    key.as_str(),
                    "textedit" | "visual studio code" | "cursor" | "zed" | "preview"
                )
            };
            out.push(InstalledApp {
                id: path.to_string_lossy().into_owned(),
                name,
                subtitle: Some(path.display().to_string()),
                launch_path: path,
                command_label: "app".to_string(),
                mime_types: Vec::new(),
                is_default: false,
                is_recommended,
                terminal: false,
            });
        } else if depth > 1 {
            collect_macos_bundles(&path, depth - 1, seen, out, entry);
        }
    }
}

#[cfg(target_os = "windows")]
fn discover_windows_installed_apps(entry: &FileBrowserEntry) -> Vec<InstalledApp> {
    let mut apps = Vec::new();
    let mut seen = BTreeSet::new();

    for (name, bin, rec) in [
        ("Notepad", "notepad.exe", !entry.is_dir),
        ("File Explorer", "explorer.exe", entry.is_dir),
    ] {
        seen.insert(bin.to_ascii_lowercase());
        apps.push(InstalledApp {
            id: bin.to_string(),
            name: name.to_string(),
            subtitle: Some("Windows Built-in".to_string()),
            launch_path: PathBuf::from(bin),
            command_label: bin.to_string(),
            mime_types: Vec::new(),
            is_default: false,
            is_recommended: rec,
            terminal: false,
        });
    }

    let mut search_roots = Vec::new();
    for env_key in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(val) = std::env::var_os(env_key) {
            let p = PathBuf::from(val);
            search_roots.push(if env_key == "LOCALAPPDATA" {
                p.join("Programs")
            } else {
                p
            });
        }
    }
    for root in search_roots {
        collect_windows_exes(&root, 2, &mut seen, &mut apps);
    }

    apps.sort_by(|a, b| {
        (!a.is_recommended, a.name.to_ascii_lowercase(), &a.id).cmp(&(
            !b.is_recommended,
            b.name.to_ascii_lowercase(),
            &b.id,
        ))
    });
    apps
}

#[cfg(target_os = "windows")]
fn collect_windows_exes(
    dir: &Path,
    depth: usize,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<InstalledApp>,
) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for item in rd.flatten() {
        let path = item.path();
        if path.is_dir() && depth > 0 {
            collect_windows_exes(&path, depth - 1, seen, out);
        } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("exe"))
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            let lower = stem.to_ascii_lowercase();
            if lower.contains("unins") || lower.contains("setup") || lower.contains("update") {
                continue;
            }
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| lower.clone());
            if !seen.insert(path.to_string_lossy().to_ascii_lowercase()) {
                continue;
            }
            out.push(InstalledApp {
                id: path.to_string_lossy().into_owned(),
                name: stem.to_string(),
                subtitle: Some(path.display().to_string()),
                launch_path: path,
                command_label: file_name,
                mime_types: Vec::new(),
                is_default: false,
                is_recommended: false,
                terminal: false,
            });
        }
    }
}
