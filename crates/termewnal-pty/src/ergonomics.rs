//! Scrollback grid search, URL detection, and shell path escaping helpers.

use std::path::Path;
use std::sync::LazyLock;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Column, Direction, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::search::RegexSearch;
use regex::Regex;

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

static URL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https?://[^\s<>"'`{}\[\]|\\^]+"#).expect("valid HTTP(S) URL regex")
});

/// Detects `http://` and `https://` URLs across visible terminal rows (including soft-wrapped lines).
pub fn detect_urls_in_snapshot(snapshot: &TerminalSnapshot) -> Vec<DetectedUrl> {
    let cols = snapshot.cols as usize;
    let rows = snapshot.rows as usize;
    if cols == 0 || rows == 0 {
        return Vec::new();
    }

    let mut urls = Vec::new();
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
            scan_urls_in_logical_line(&logical_line, &byte_coords, &mut urls);
            logical_line.clear();
            byte_coords.clear();
        }
    }

    urls
}

/// Returns the detected URL covering viewport cell `(col, row)`, if any.
pub fn url_at_cell(snapshot: &TerminalSnapshot, col: usize, row: usize) -> Option<DetectedUrl> {
    detect_urls_in_snapshot(snapshot)
        .into_iter()
        .find(|detected| detected.contains_cell(col, row))
}

fn scan_urls_in_logical_line(
    line: &str,
    byte_coords: &[(usize, usize, bool)],
    out: &mut Vec<DetectedUrl>,
) {
    if line.is_empty() {
        return;
    }

    for matched in URL_REGEX.find_iter(line) {
        let trimmed = trim_url_suffix(matched.as_str());
        if trimmed.len() <= "https://".len() && trimmed.starts_with("https://")
            || trimmed.len() <= "http://".len() && trimmed.starts_with("http://")
        {
            continue;
        }

        let start = matched.start();
        let end = start + trimmed.len();
        let coords = &byte_coords[start..end];
        let spans = coalesce_url_spans(coords);
        if !spans.is_empty() {
            out.push(DetectedUrl {
                url: trimmed.to_string(),
                spans,
            });
        }
    }
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
