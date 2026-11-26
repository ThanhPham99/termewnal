//! Scrollback grid search, URL detection, and shell path escaping helpers.

use std::path::Path;
use std::sync::LazyLock;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Column, Direction, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::search::RegexSearch;
use regex::{Regex, RegexBuilder};
use termewnal_core::{HighlightRule, RgbColor};

use crate::session::TerminalSnapshot;

/// Canonical `(line, col)` coordinate in an `alacritty_terminal` grid.
///
/// Negative `line` values denote scrollback history; `0..rows` denote the live bottom screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GridCoord {
    pub line: i32,
    pub col: usize,
}

/// Inclusive terminal grid match returned by [`crate::PtySession::search_scrollback`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSearchMatch {
    pub start: GridCoord,
    pub end: GridCoord,
}

impl GridSearchMatch {
    /// Returns `true` when grid cell `(line, col)` lies inside this match span.
    pub fn contains(&self, line: i32, col: usize, cols: usize) -> bool {
        if line < self.start.line || line > self.end.line {
            return false;
        }
        if self.start.line == self.end.line {
            col >= self.start.col && col <= self.end.col
        } else if line == self.start.line {
            col >= self.start.col && col < cols
        } else if line == self.end.line {
            col <= self.end.col
        } else {
            col < cols
        }
    }
}

/// Single viewport row span covered by a detected URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrlSpan {
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
}

/// URL detected within a [`TerminalSnapshot`] along with its visible viewport spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedUrl {
    pub url: String,
    pub spans: Vec<UrlSpan>,
}

impl DetectedUrl {
    /// Returns `true` when viewport cell `(col, row)` falls inside this URL.
    pub fn contains_cell(&self, col: usize, row: usize) -> bool {
        self.spans
            .iter()
            .any(|span| span.row == row && col >= span.start_col && col <= span.end_col)
    }
}

/// Category of clickable terminal link detected in a [`TerminalSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectedLinkKind {
    WebUrl,
    LocalFile {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
}

/// Clickable link (HTTP/HTTPS URL or local file path) detected inside [`TerminalSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedLink {
    pub target: String,
    pub kind: DetectedLinkKind,
    pub spans: Vec<UrlSpan>,
}

impl DetectedLink {
    /// Returns `true` when viewport cell `(col, row)` falls inside this link.
    pub fn contains_cell(&self, col: usize, row: usize) -> bool {
        self.spans
            .iter()
            .any(|span| span.row == row && col >= span.start_col && col <= span.end_col)
    }
}

static URL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s<>"'`{}\[\]|\\^]+"#).expect("valid HTTP(S) URL regex")
});

static FILE_URI_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"file://[^\s<>"'`{}\[\]|\\^]+"#).expect("valid file URI regex")
});

static LOCAL_PATH_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|[\s(\['"])(((?:\.\.?|~)?/(?:[^\s<>"'`{}\[\]|\\^:]+/?)+|[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)+)(?::(\d+)(?::(\d+))?)?)"#)
        .expect("valid local file path regex")
});

/// Detects `http://` and `https://` URLs across visible terminal rows (including soft-wrapped lines).
pub fn detect_urls_in_snapshot(snapshot: &TerminalSnapshot) -> Vec<DetectedUrl> {
    detect_links_in_snapshot(snapshot)
        .into_iter()
        .filter(|link| matches!(link.kind, DetectedLinkKind::WebUrl))
        .map(|link| DetectedUrl {
            url: link.target,
            spans: link.spans,
        })
        .collect()
}

/// Detects clickable HTTP(S) URLs, `file://` URIs, and local file paths (`path:line:col`) across visible rows.
pub fn detect_links_in_snapshot(snapshot: &TerminalSnapshot) -> Vec<DetectedLink> {
    let cols = snapshot.cols as usize;
    let rows = snapshot.rows as usize;
    if cols == 0 || rows == 0 {
        return Vec::new();
    }

    let mut links = Vec::new();
    let mut logical_line = String::new();
    let mut byte_coords: Vec<(usize, usize, bool)> = Vec::new();

    for row in 0..rows {
        let wrapped = snapshot
            .cell(cols - 1, row)
            .is_some_and(|cell| cell.flags.contains(CellFlags::WRAPLINE));

        let mut row_end = cols;
        if !wrapped {
            while row_end > 0 {
                let Some(cell) = snapshot.cell(row_end - 1, row) else {
                    break;
                };
                if (cell.c == ' ' || cell.c == '\0')
                    && !cell
                        .flags
                        .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER)
                {
                    row_end -= 1;
                } else {
                    break;
                }
            }
        }

        for col in 0..row_end {
            let Some(cell) = snapshot.cell(col, row) else {
                continue;
            };
            if cell
                .flags
                .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let ch = if cell.c == '\0' { ' ' } else { cell.c };
            let is_wide = cell.flags.contains(CellFlags::WIDE_CHAR);
            logical_line.push(ch);
            for _ in 0..ch.len_utf8() {
                byte_coords.push((col, row, is_wide));
            }
        }

        if !wrapped || row + 1 == rows {
            scan_links_in_logical_line(&logical_line, &byte_coords, &mut links);
            logical_line.clear();
            byte_coords.clear();
        }
    }

    links
}

/// Returns the detected URL covering viewport cell `(col, row)`, if any.
pub fn url_at_cell(snapshot: &TerminalSnapshot, col: usize, row: usize) -> Option<DetectedUrl> {
    detect_urls_in_snapshot(snapshot)
        .into_iter()
        .find(|detected| detected.contains_cell(col, row))
}

/// Returns any clickable link (web URL or local file path) covering viewport cell `(col, row)`.
pub fn link_at_cell(snapshot: &TerminalSnapshot, col: usize, row: usize) -> Option<DetectedLink> {
    detect_links_in_snapshot(snapshot)
        .into_iter()
        .find(|detected| detected.contains_cell(col, row))
}

fn scan_links_in_logical_line(
    line: &str,
    byte_coords: &[(usize, usize, bool)],
    out: &mut Vec<DetectedLink>,
) {
    if line.is_empty() {
        return;
    }

    let mut occupied: Vec<(usize, usize)> = Vec::new();

    for matched in URL_REGEX.find_iter(line) {
        let trimmed = trim_url_suffix(matched.as_str());
        if trimmed.len() <= "https://".len() && trimmed.starts_with("https://")
            || trimmed.len() <= "http://".len() && trimmed.starts_with("http://")
        {
            continue;
        }
        let start = matched.start();
        let end = start + trimmed.len();
        occupied.push((start, end));
        let spans = coalesce_url_spans(&byte_coords[start..end]);
        if !spans.is_empty() {
            out.push(DetectedLink {
                target: trimmed.to_string(),
                kind: DetectedLinkKind::WebUrl,
                spans,
            });
        }
    }

    for matched in FILE_URI_REGEX.find_iter(line) {
        let trimmed = trim_url_suffix(matched.as_str());
        if trimmed.len() <= "file://".len() {
            continue;
        }
        let start = matched.start();
        let end = start + trimmed.len();
        if overlaps_any(&occupied, start, end) {
            continue;
        }
        occupied.push((start, end));
        let raw_path = trimmed.strip_prefix("file://").unwrap_or(trimmed);
        let (path, line_no, col_no) = split_path_line_col(raw_path);
        let spans = coalesce_url_spans(&byte_coords[start..end]);
        if !spans.is_empty() {
            out.push(DetectedLink {
                target: trimmed.to_string(),
                kind: DetectedLinkKind::LocalFile {
                    path,
                    line: line_no,
                    col: col_no,
                },
                spans,
            });
        }
    }

    for caps in LOCAL_PATH_REGEX.captures_iter(line) {
        let Some(full) = caps.get(1) else {
            continue;
        };
        let raw = trim_path_suffix(full.as_str());
        if raw.len() < 2 || raw.starts_with("//") {
            continue;
        }
        let start = full.start();
        let end = start + raw.len();
        if overlaps_any(&occupied, start, end) {
            continue;
        }
        let (path, line_no, col_no) = split_path_line_col(raw);
        let has_line = line_no.is_some();
        let has_ext = Path::new(&path).extension().is_some();
        let starts_explicit = path.starts_with('/')
            || path.starts_with("./")
            || path.starts_with("../")
            || path.starts_with("~/");
        if !has_line && !has_ext && !starts_explicit {
            continue;
        }
        occupied.push((start, end));
        let spans = coalesce_url_spans(&byte_coords[start..end]);
        if !spans.is_empty() {
            out.push(DetectedLink {
                target: raw.to_string(),
                kind: DetectedLinkKind::LocalFile {
                    path,
                    line: line_no,
                    col: col_no,
                },
                spans,
            });
        }
    }
}

fn overlaps_any(occupied: &[(usize, usize)], start: usize, end: usize) -> bool {
    occupied
        .iter()
        .any(|&(s, e)| start < e && end > s)
}

fn trim_path_suffix(mut raw: &str) -> &str {
    loop {
        let Some(last) = raw.chars().next_back() else {
            break;
        };
        if matches!(last, '.' | ',' | ';' | '!' | '?' | '\'' | '"' | ')' | ']') {
            raw = &raw[..raw.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    raw
}

fn split_path_line_col(raw: &str) -> (String, Option<u32>, Option<u32>) {
    let parts: Vec<&str> = raw.rsplitn(3, ':').collect();
    match parts.as_slice() {
        [c_str, l_str, path]
            if !c_str.is_empty()
                && !l_str.is_empty()
                && c_str.bytes().all(|b| b.is_ascii_digit())
                && l_str.bytes().all(|b| b.is_ascii_digit()) =>
        {
            if let (Ok(line), Ok(col)) = (l_str.parse::<u32>(), c_str.parse::<u32>()) {
                return ((*path).to_string(), Some(line), Some(col));
            }
        }
        [l_str, path]
            if !l_str.is_empty() && l_str.bytes().all(|b| b.is_ascii_digit()) =>
        {
            if let Ok(line) = l_str.parse::<u32>() {
                return ((*path).to_string(), Some(line), None);
            }
        }
        _ => {}
    }
    (raw.to_string(), None, None)
}

fn trim_url_suffix(mut url: &str) -> &str {
    loop {
        let Some(last) = url.chars().next_back() else {
            break;
        };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            ')' => url.matches(')').count() > url.matches('(').count(),
            ']' => url.matches(']').count() > url.matches('[').count(),
            _ => false,
        };
        if strip {
            url = &url[..url.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    url
}

fn coalesce_url_spans(coords: &[(usize, usize, bool)]) -> Vec<UrlSpan> {
    let mut spans: Vec<UrlSpan> = Vec::new();
    for &(col, row, is_wide) in coords {
        let cell_end = if is_wide { col + 1 } else { col };
        match spans.last_mut() {
            Some(last) if last.row == row => {
                last.start_col = last.start_col.min(col);
                last.end_col = last.end_col.max(cell_end);
            }
            _ => spans.push(UrlSpan {
                row,
                start_col: col,
                end_col: cell_end,
            }),
        }
    }
    spans
}

/// Formats a filesystem `path` as a POSIX shell-safe token for inserting into a terminal `Session`.
pub fn shell_escape_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if !raw.is_empty()
        && raw.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'+' | b'@' | b',')
        })
    {
        return raw.into_owned();
    }

    let mut escaped = String::with_capacity(raw.len() + 2);
    escaped.push('\'');
    for ch in raw.chars() {
        if ch == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

/// Formats multiple filesystem paths as space-separated POSIX shell-escaped tokens.
pub fn shell_escape_paths<I, P>(paths: I) -> String
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    paths
        .into_iter()
        .map(|p| shell_escape_path(p.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Collects all non-overlapping matches for `query` across scrollback history and visible screen.
pub(crate) fn search_term_matches<T>(term: &Term<T>, query: &str) -> Vec<GridSearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }

    let mut regex = match RegexSearch::new(query) {
        Ok(re) => re,
        Err(_) => match RegexSearch::new(&regex::escape(query)) {
            Ok(re) => re,
            Err(_) => return Vec::new(),
        },
    };

    let grid_end = Point::new(term.bottommost_line(), term.last_column());
    let mut cursor = Point::new(term.topmost_line(), Column(0));
    let mut matches = Vec::new();

    while cursor <= grid_end {
        let Some(found) = term.regex_search_right(&mut regex, cursor, grid_end) else {
            break;
        };
        let start = *found.start();
        let end = *found.end();
        matches.push(GridSearchMatch {
            start: GridCoord {
                line: start.line.0,
                col: start.column.0,
            },
            end: GridCoord {
                line: end.line.0,
                col: end.column.0,
            },
        });

        let expanded_end = term.expand_wide(end, Direction::Right);
        if expanded_end >= grid_end {
            break;
        }
        let next_cursor = expanded_end.add(term, Boundary::Grid, 1);
        if next_cursor <= expanded_end {
            break;
        }
        cursor = next_cursor;
    }

    matches
}

/// Mouse button forwarded to PTY applications in mouse tracking mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyMouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// Mouse interaction phase forwarded to PTY applications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyMouseEventKind {
    Press,
    Release,
    Drag,
}

/// Encodes a viewport mouse event (`0`-based `col`, `row`) into xterm SGR 1006 or normal X10 bytes.
pub fn encode_mouse_event(
    button: PtyMouseButton,
    kind: PtyMouseEventKind,
    col: usize,
    row: usize,
    shift: bool,
    alt: bool,
    ctrl: bool,
    sgr: bool,
) -> Vec<u8> {
    let base: u8 = match (button, kind) {
        (PtyMouseButton::WheelUp, _) => 64,
        (PtyMouseButton::WheelDown, _) => 65,
        (_, PtyMouseEventKind::Release) if !sgr => 3,
        (PtyMouseButton::Left, _) => 0,
        (PtyMouseButton::Middle, _) => 1,
        (PtyMouseButton::Right, _) => 2,
    };
    let mut cb = base;
    if matches!(kind, PtyMouseEventKind::Drag) {
        cb = cb.saturating_add(32);
    }
    if shift {
        cb = cb.saturating_add(4);
    }
    if alt {
        cb = cb.saturating_add(8);
    }
    if ctrl {
        cb = cb.saturating_add(16);
    }

    if sgr {
        let suffix = match kind {
            PtyMouseEventKind::Release => 'm',
            PtyMouseEventKind::Press | PtyMouseEventKind::Drag => 'M',
        };
        format!("\x1b[<{cb};{};{}{suffix}", col + 1, row + 1).into_bytes()
    } else {
        let bx = ((col + 1).min(223) as u8).saturating_add(32);
        let by = ((row + 1).min(223) as u8).saturating_add(32);
        vec![0x1b, b'[', b'M', cb.saturating_add(32), bx, by]
    }
}

/// Visual style resolved from a matching [`HighlightRule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightStyle {
    pub fg: Option<RgbColor>,
    pub bg: Option<RgbColor>,
    pub bold: bool,
}

/// Pre-compiled regex highlight rule ready for fast per-frame matching.
#[derive(Debug, Clone)]
pub struct CompiledHighlightRule {
    regex: Regex,
    style: HighlightStyle,
}

/// Compiles enabled [`HighlightRule`] entries, skipping rules with empty or invalid patterns.
pub fn compile_highlight_rules(rules: &[HighlightRule]) -> Vec<CompiledHighlightRule> {
    rules
        .iter()
        .filter(|r| r.enabled && !r.pattern.is_empty())
        .filter_map(|r| {
            let regex = RegexBuilder::new(&r.pattern)
                .case_insensitive(r.case_insensitive)
                .build()
                .ok()?;
            let fg = r.foreground.as_deref().and_then(RgbColor::from_hex);
            let bg = r.background.as_deref().and_then(RgbColor::from_hex);
            if fg.is_none() && bg.is_none() && !r.bold {
                return None;
            }
            Some(CompiledHighlightRule {
                regex,
                style: HighlightStyle {
                    fg,
                    bg,
                    bold: r.bold,
                },
            })
        })
        .collect()
}

/// Computes a flat `cols * rows` highlight overlay grid for `snapshot`.
pub fn compute_highlight_grid(
    snapshot: &TerminalSnapshot,
    rules: &[CompiledHighlightRule],
) -> Vec<Option<HighlightStyle>> {
    let cols = snapshot.cols as usize;
    let rows = snapshot.rows as usize;
    let mut grid = vec![None; cols * rows];
    if rules.is_empty() || cols == 0 || rows == 0 {
        return grid;
    }

    for row in 0..rows {
        let mut line = String::new();
        let mut byte_to_col: Vec<usize> = Vec::new();
        for col in 0..cols {
            let Some(cell) = snapshot.cell(col, row) else {
                continue;
            };
            if cell
                .flags
                .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let ch = if cell.c == '\0' { ' ' } else { cell.c };
            line.push(ch);
            for _ in 0..ch.len_utf8() {
                byte_to_col.push(col);
            }
        }
        let trimmed_len = line.trim_end().len();
        let scan_slice = &line[..trimmed_len];
        if scan_slice.is_empty() {
            continue;
        }
        for rule in rules {
            for m in rule.regex.find_iter(scan_slice) {
                for &col in &byte_to_col[m.start()..m.end()] {
                    grid[row * cols + col] = Some(rule.style);
                }
            }
        }
    }

    grid
}
