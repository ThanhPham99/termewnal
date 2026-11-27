use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Side;
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi::{Color as TermColor, CursorShape};
use alacritty_terminal::vte::{Parser as VteParser, Perform};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::buffer::TerminalBuffer;
use crate::ergonomics::GridSearchMatch;
use crate::error::PtyError;
use crate::shell::{ShellSpec, detect_default_shell};

/// Configuration for spawning a local shell [`PtySession`].
#[derive(Debug, Clone)]
pub struct PtySpawnConfig {
    pub shell: ShellSpec,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_lines: usize,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    pub working_dir: Option<PathBuf>,
    pub word_separators: Option<String>,
}

impl Default for PtySpawnConfig {
    fn default() -> Self {
        Self {
            shell: detect_default_shell(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
            cursor_shape: CursorShape::Block,
            cursor_blink: false,
            working_dir: None,
            word_separators: None,
        }
    }
}

/// Progress indicator reported via `OSC 9;4;<state>;<progress>` (ConEmu / Windows Terminal / Tabby).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalProgressState {
    #[default]
    Hidden,
    Normal(u8),
    Indeterminate,
    Error(u8),
    Paused(u8),
}

/// Lifecycle and grid notifications emitted by a running [`PtySession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    Output,
    Title(String),
    Bell,
    Progress(TerminalProgressState),
    Exited(Option<i32>),
}

/// Terminal grid dimensions adapter for `alacritty_terminal::Term`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermDimensions {
    pub cols: u16,
    pub rows: u16,
}

impl TermDimensions {
    /// Clamps dimensions to at least `1x1` so `alacritty_terminal` invariants hold.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// Rendered cell snapshot extracted from `alacritty_terminal::RenderableContent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCell {
    pub c: char,
    pub fg: TermColor,
    pub bg: TermColor,
    pub flags: CellFlags,
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: TermColor::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
            bg: TermColor::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            flags: CellFlags::empty(),
        }
    }
}

/// Visible cursor location and shape inside [`TerminalSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub col: usize,
    pub row: usize,
    pub shape: CursorShape,
}

/// Canonical inclusive grid span for selection highlighting inside [`TerminalSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridRange {
    pub start_line: i32,
    pub start_col: usize,
    pub end_line: i32,
    pub end_col: usize,
    pub is_block: bool,
}

impl GridRange {
    /// Returns `true` when grid coordinates `(line, col)` fall within this span.
    pub fn contains(&self, line: i32, col: usize) -> bool {
        self.start_line <= line
            && self.end_line >= line
            && (self.start_col <= col || (self.start_line != line && !self.is_block))
            && (self.end_col >= col || (self.end_line != line && !self.is_block))
    }
}

/// Immutable snapshot of the visible terminal grid and cursor for rendering and testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<TerminalCell>,
    pub cursor: Option<CursorSnapshot>,
    pub display_offset: usize,
    pub history_size: usize,
    pub top_line: i32,
    pub selection: Option<GridRange>,
    pub selection_type: Option<SelectionType>,
    pub bracketed_paste: bool,
    pub mouse_mode: bool,
    pub mouse_sgr: bool,
    pub mouse_motion: bool,
}

impl TerminalSnapshot {
    /// Converts viewport `(col, row)` into canonical grid `(line, col)` coordinates.
    pub fn viewport_to_grid(&self, col: usize, row: usize) -> (i32, usize) {
        (self.top_line + row as i32, col)
    }

    /// Converts canonical grid `(line, col)` to visible viewport `(col, row)` if on screen.
    pub fn grid_to_viewport(&self, line: i32, col: usize) -> Option<(usize, usize)> {
        let rel = line - self.top_line;
        if rel >= 0 && (rel as usize) < self.rows as usize && col < self.cols as usize {
            Some((col, rel as usize))
        } else {
            None
        }
    }

    /// Returns `true` when the cell at viewport `(col, row)` is inside the active selection.
    pub fn is_cell_selected(&self, col: usize, row: usize) -> bool {
        let Some(sel) = self.selection else {
            return false;
        };
        let (line, _) = self.viewport_to_grid(col, row);
        if sel.contains(line, col) {
            return true;
        }
        self.cell(col, row)
            .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR))
            && sel.contains(line, col + 1)
    }

    /// Returns the inclusive rightmost visual highlight column for multi-line/`Lines` selections on `row`.
    ///
    /// Non-empty rows extend to `(last_non_space_col + 1)` clamped to `cols - 1` (representing the
    /// trailing newline cell), soft-wrapped rows extend to `cols - 1`, and blank rows highlight col `0`.
    pub fn row_visual_selection_end_col(&self, row: usize) -> usize {
        if row >= self.rows as usize || self.cols == 0 {
            return 0;
        }
        let cols = self.cols as usize;
        if self
            .cell(cols - 1, row)
            .is_some_and(|cell| cell.flags.contains(CellFlags::WRAPLINE))
        {
            return cols - 1;
        }
        let start = row * cols;
        let mut last_non_space = None;
        for col in 0..cols {
            let cell = &self.cells[start + col];
            if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                if last_non_space == col.checked_sub(1) {
                    last_non_space = Some(col);
                }
            } else if !cell.flags.contains(CellFlags::LEADING_WIDE_CHAR_SPACER)
                && cell.c != ' '
                && cell.c != '\0'
            {
                last_non_space = Some(col);
            }
        }
        match last_non_space {
            Some(c) => (c + 1).min(cols - 1),
            None => 0,
        }
    }

    /// Returns `true` when `(col, row)` should be highlighted visually for the active selection.
    ///
    /// Single-line `Simple` and rectangular `Block` selections highlight exact grid bounds, while
    /// multi-line `Simple`, `Lines`, and multi-line `Semantic` selections clamp trailing blank screen
    /// padding on each row to [`Self::row_visual_selection_end_col`].
    pub fn is_cell_visually_selected(&self, col: usize, row: usize) -> bool {
        let Some(sel) = self.selection else {
            return false;
        };
        if !self.is_cell_selected(col, row) {
            return false;
        }
        if sel.is_block
            || (self.selection_type == Some(SelectionType::Simple)
                && sel.start_line == sel.end_line)
        {
            return true;
        }
        let max_col = self.row_visual_selection_end_col(row);
        if col <= max_col {
            return true;
        }
        let (line, _) = self.viewport_to_grid(col, row);
        line == sel.start_line && col == sel.start_col
    }

    /// Returns the cell at `(col, row)` if within visible viewport bounds.
    pub fn cell(&self, col: usize, row: usize) -> Option<&TerminalCell> {
        if col >= self.cols as usize || row >= self.rows as usize {
            return None;
        }
        self.cells.get(row * self.cols as usize + col)
    }

    /// Returns the trimmed text content of visible `row`.
    pub fn row_text(&self, row: usize) -> String {
        if row >= self.rows as usize {
            return String::new();
        }
        let cols = self.cols as usize;
        let start = row * cols;
        let line: String = self.cells[start..start + cols]
            .iter()
            .filter(|cell| !cell.flags.contains(CellFlags::WIDE_CHAR_SPACER))
            .map(|cell| cell.c)
            .collect();
        line.trim_end().to_string()
    }

    /// Returns all visible rows joined by newlines (trailing empty rows omitted).
    pub fn visible_text(&self) -> String {
        let mut lines: Vec<String> = (0..self.rows as usize).map(|r| self.row_text(r)).collect();
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

struct SharedPtyWriter(Arc<Mutex<Option<Box<dyn Write + Send>>>>);

impl Write for SharedPtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut guard) = self.0.lock()
            && let Some(writer) = guard.as_mut()
        {
            return writer.write(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Ok(mut guard) = self.0.lock()
            && let Some(writer) = guard.as_mut()
        {
            return writer.flush();
        }
        Ok(())
    }
}

/// Active local shell PTY backed by `portable-pty` and a shared [`TerminalBuffer`].
pub struct PtySession {
    shell: ShellSpec,
    child_pid: Option<u32>,
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    buffer: Arc<TerminalBuffer>,
    last_working_dir: Arc<Mutex<Option<PathBuf>>>,
    osc7_working_dir: Arc<Mutex<Option<PathBuf>>>,
}

impl PtySession {
    /// Spawns a local shell attached to a newly allocated OS PTY and starts the VTE reader loop.
    pub fn spawn(config: PtySpawnConfig) -> Result<Self, PtyError> {
        let dims = TermDimensions::new(config.cols, config.rows);
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: dims.rows,
                cols: dims.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| PtyError::OpenPty(err.to_string()))?;

        let mut cmd = CommandBuilder::new(&config.shell.program);
        cmd.args(&config.shell.args);
        if let Some(dir) = &config.working_dir {
            cmd.cwd(dir);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| PtyError::SpawnShell {
                program: config.shell.program.clone(),
                reason: err.to_string(),
            })?;
        let child_pid = child.process_id();

        // CRITICAL ON UNIX: Drop slave in parent immediately so reader receives EOF when child exits.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| PtyError::OpenPty(err.to_string()))?;
        let raw_writer = pair
            .master
            .take_writer()
            .map_err(|err| PtyError::OpenPty(err.to_string()))?;
        let killer = child.clone_killer();

        let writer: Arc<Mutex<Option<Box<dyn Write + Send>>>> =
            Arc::new(Mutex::new(Some(raw_writer)));
        let buffer = Arc::new(TerminalBuffer::new_with_full_options(
            dims.cols,
            dims.rows,
            config.scrollback_lines,
            config.cursor_shape,
            config.cursor_blink,
            config.word_separators.as_deref(),
        ));
        buffer.set_write_sink(Some(Box::new(SharedPtyWriter(Arc::clone(&writer)))));
        let last_working_dir = Arc::new(Mutex::new(config.working_dir.clone()));
        let osc7_working_dir = Arc::new(Mutex::new(None));

        let master = Arc::new(Mutex::new(Some(pair.master)));

        spawn_reader_thread(
            reader,
            child,
            child_pid,
            Arc::clone(&master),
            Arc::clone(&writer),
            Arc::clone(&buffer),
            Arc::clone(&last_working_dir),
            Arc::clone(&osc7_working_dir),
        );

        Ok(Self {
            shell: config.shell,
            child_pid,
            master,
            writer,
            killer: Mutex::new(killer),
            buffer,
            last_working_dir,
            osc7_working_dir,
        })
    }

    /// Returns the shell specification used to launch this session.
    pub fn shell(&self) -> &ShellSpec {
        &self.shell
    }

    /// Returns the shared [`TerminalBuffer`] backing this session.
    pub fn buffer(&self) -> &Arc<TerminalBuffer> {
        &self.buffer
    }

    /// Captures the current working directory of this local shell `Session`.
    ///
    /// On Linux, queries `/proc/<pid>/cwd` for the live shell (or foreground process group leader)
    /// and caches the result, falling back to OSC 7 reports or the last retained directory once
    /// the child exits.
    pub fn current_working_dir(&self) -> Option<PathBuf> {
        #[cfg(target_os = "linux")]
        if !self.buffer.is_exited() {
            let fg_pid = self
                .master
                .lock()
                .ok()
                .and_then(|master| {
                    master
                        .as_ref()
                        .and_then(|inner| inner.process_group_leader())
                })
                .and_then(|pid| u32::try_from(pid).ok());

            if let Some(cwd) = read_proc_cwd(self.child_pid).or_else(|| read_proc_cwd(fg_pid)) {
                if let Ok(mut guard) = self.last_working_dir.lock() {
                    *guard = Some(cwd.clone());
                }
                return Some(cwd);
            }
        }

        if let Ok(guard) = self.osc7_working_dir.lock()
            && let Some(cwd) = guard.clone()
        {
            return Some(cwd);
        }

        self.last_working_dir
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Takes the async event receiver once for driving an external event loop or UI subscription.
    pub fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<PtyEvent>> {
        self.buffer.take_event_receiver()
    }

    /// Writes raw keyboard or paste bytes to the PTY master.
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), PtyError> {
        let mut guard = self.writer.lock().expect("pty writer mutex poisoned");
        let writer = guard.as_mut().ok_or(PtyError::WriterClosed)?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Returns the current `(cols, rows)` terminal dimensions.
    pub fn dimensions(&self) -> (u16, u16) {
        self.buffer.dimensions()
    }

    /// Resizes both the OS PTY device (`TIOCSWINSZ`) and the `alacritty_terminal` grid.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
        let dims = TermDimensions::new(cols, rows);
        if self.buffer.dimensions() == (dims.cols, dims.rows) {
            return Ok(());
        }

        {
            let guard = self.master.lock().expect("master pty mutex poisoned");
            if let Some(master) = guard.as_ref() {
                master
                    .resize(PtySize {
                        rows: dims.rows,
                        cols: dims.cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .map_err(|err| PtyError::Resize {
                        cols: dims.cols,
                        rows: dims.rows,
                        reason: err.to_string(),
                    })?;
            }
        }

        self.buffer.resize(dims.cols, dims.rows);
        Ok(())
    }

    /// Captures an immutable snapshot of the current visible grid and cursor.
    pub fn snapshot(&self) -> TerminalSnapshot {
        self.buffer.snapshot()
    }

    /// Scrolls the viewport display offset within retained scrollback history and returns the new offset.
    pub fn scroll_display(&self, scroll: Scroll) -> usize {
        self.buffer.scroll_display(scroll)
    }

    /// Resets the viewport scroll offset back to the live terminal bottom (`0`).
    pub fn scroll_to_bottom(&self) -> usize {
        self.buffer.scroll_to_bottom()
    }

    /// Returns the current number of lines scrolled above the live bottom.
    pub fn display_offset(&self) -> usize {
        self.buffer.display_offset()
    }

    /// Returns the number of scrollback lines currently stored in history.
    pub fn history_size(&self) -> usize {
        self.buffer.history_size()
    }

    /// Applies updated scrollback capacity and default cursor shape to the running terminal.
    pub fn apply_options(&self, scrollback_lines: usize, cursor_shape: CursorShape) {
        self.buffer.apply_options(scrollback_lines, cursor_shape);
    }

    /// Applies updated scrollback capacity, cursor style, blink flag, and semantic word separators.
    pub fn apply_options_full(
        &self,
        scrollback_lines: usize,
        cursor_shape: CursorShape,
        cursor_blink: bool,
        word_separators: Option<&str>,
    ) {
        self.buffer
            .apply_options_full(scrollback_lines, cursor_shape, cursor_blink, word_separators);
    }

    /// Returns `true` when the active terminal application has enabled bracketed paste mode.
    pub fn is_bracketed_paste(&self) -> bool {
        self.buffer.is_bracketed_paste()
    }

    /// Returns `true` when the active terminal application captures mouse events.
    pub fn is_mouse_mode(&self) -> bool {
        self.buffer.is_mouse_mode()
    }

    /// Searches scrollback history and visible screen for `query`, returning ordered matches.
    pub fn search_scrollback(&self, query: &str) -> Vec<GridSearchMatch> {
        self.buffer.search_scrollback(query)
    }

    /// Scrolls the viewport if needed so `target` becomes visible, returning the new offset.
    pub fn scroll_to_match(&self, target: GridSearchMatch) -> usize {
        self.buffer.scroll_to_match(target)
    }

    /// Starts a terminal grid selection at viewport `(col, row)`.
    pub fn start_selection(&self, col: usize, row: usize, side: Side, ty: SelectionType) {
        self.buffer.start_selection(col, row, side, ty);
    }

    /// Extends the active terminal selection to viewport `(col, row)`.
    pub fn update_selection(&self, col: usize, row: usize, side: Side) {
        self.buffer.update_selection(col, row, side);
    }

    /// Extends an active selection to viewport `(col, row)`, or starts a new `Simple` selection.
    pub fn extend_or_start_selection(&self, col: usize, row: usize, side: Side) {
        self.buffer.extend_or_start_selection(col, row, side);
    }

    /// Finalizes the active selection, clearing degenerate click-only selections and returning text.
    pub fn finish_selection(&self) -> Option<String> {
        self.buffer.finish_selection()
    }

    /// Extracts the currently selected text without modifying selection state.
    pub fn selection_text(&self) -> Option<String> {
        self.buffer.selection_text()
    }

    /// Clears any active selection in the terminal grid.
    pub fn clear_selection(&self) {
        self.buffer.clear_selection();
    }

    /// Blocks up to `timeout` until `predicate(&snapshot)` evaluates to `true`.
    pub fn wait_for_output<F>(&self, timeout: Duration, predicate: F) -> bool
    where
        F: FnMut(&TerminalSnapshot) -> bool,
    {
        self.buffer.wait_for_output(timeout, predicate)
    }

    /// Sends a termination signal to the child process and closes the PTY writer.
    pub fn terminate(&self) {
        if let Ok(mut guard) = self.writer.lock() {
            *guard = None;
        }
        if let Ok(mut killer) = self.killer.lock() {
            let _ = killer.kill();
        }
        self.buffer.notify_output();
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_reader_thread(
    mut reader: Box<dyn Read + Send>,
    #[cfg_attr(windows, allow(unused_mut))] mut child: Box<dyn portable_pty::Child + Send + Sync>,
    child_pid: Option<u32>,
    #[cfg_attr(not(windows), allow(unused_variables))] master: Arc<
        Mutex<Option<Box<dyn MasterPty + Send>>>,
    >,
    #[cfg_attr(not(windows), allow(unused_variables))] writer: Arc<
        Mutex<Option<Box<dyn Write + Send>>>,
    >,
    buffer: Arc<TerminalBuffer>,
    last_working_dir: Arc<Mutex<Option<PathBuf>>>,
    osc7_working_dir: Arc<Mutex<Option<PathBuf>>>,
) {
    thread::Builder::new()
        .name("termewnal-pty-reader".to_string())
        .spawn(move || {
            #[cfg(windows)]
            let waiter = thread::Builder::new()
                .name("termewnal-pty-waiter".to_string())
                .spawn(move || {
                    let code = match child.wait() {
                        Ok(status) => Some(status.exit_code() as i32),
                        Err(err) => {
                            warn!(error = %err, "failed to wait on PTY child");
                            None
                        }
                    };
                    // Close ConPTY input and release MasterPty (`ClosePseudoConsole`) once the child
                    // process exits so the reader pipe drains any final output and observes EOF.
                    let popped_writer = writer.lock().ok().and_then(|mut w| w.take());
                    drop(popped_writer);
                    let popped_master = master.lock().ok().and_then(|mut m| m.take());
                    drop(popped_master);
                    code
                })
                .expect("spawn PTY waiter thread");

            let mut osc7_parser = VteParser::new();
            let mut osc7_tracker = Osc7Tracker {
                osc7_working_dir: Arc::clone(&osc7_working_dir),
            };
            let mut buf = [0_u8; 8192];

            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        debug!("PTY reader hit EOF");
                        break;
                    }
                    Ok(n) => {
                        osc7_parser.advance(&mut osc7_tracker, &buf[..n]);
                        buffer.feed_bytes(&buf[..n]);
                        if let Some(cwd) = read_proc_cwd(child_pid)
                            && let Ok(mut guard) = last_working_dir.lock()
                        {
                            *guard = Some(cwd);
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        warn!(error = %err, "PTY reader stopped with error");
                        break;
                    }
                }
            }

            #[cfg(windows)]
            let exit_code = waiter.join().ok().flatten();
            #[cfg(not(windows))]
            let exit_code = match child.wait() {
                Ok(status) => Some(status.exit_code() as i32),
                Err(err) => {
                    warn!(error = %err, "failed to wait on PTY child");
                    None
                }
            };

            buffer.mark_exited(exit_code);
        })
        .expect("spawn PTY reader thread");
}

struct Osc7Tracker {
    osc7_working_dir: Arc<Mutex<Option<PathBuf>>>,
}

impl Perform for Osc7Tracker {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.len() >= 2
            && params[0] == b"7"
            && let Ok(uri) = std::str::from_utf8(params[1])
            && let Some(path) = parse_osc7_path(uri)
            && let Ok(mut guard) = self.osc7_working_dir.lock()
        {
            *guard = Some(path);
        }
    }
}

fn parse_osc7_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    let encoded_path = if let Some(rest) = trimmed.strip_prefix("file://") {
        if rest.starts_with('/') {
            rest
        } else {
            let idx = rest.find('/')?;
            &rest[idx..]
        }
    } else if trimmed.starts_with('/') {
        trimmed
    } else {
        return None;
    };

    let decoded = percent_decode(encoded_path);
    #[cfg(windows)]
    let decoded = if decoded.starts_with('/')
        && decoded.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic)
        && decoded.as_bytes().get(2) == Some(&b':')
    {
        decoded[1..].to_string()
    } else {
        decoded
    };
    (!decoded.is_empty()).then(|| PathBuf::from(decoded))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn read_proc_cwd(pid: Option<u32>) -> Option<PathBuf> {
    let pid = pid?;
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(target_os = "linux"))]
fn read_proc_cwd(_pid: Option<u32>) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::parse_osc7_path;
    use std::path::PathBuf;

    #[test]
    fn parses_osc7_file_urls_cleanly() {
        assert_eq!(
            parse_osc7_path("file://localhost/tmp/work%20dir"),
            Some(PathBuf::from("/tmp/work dir"))
        );
        #[cfg(windows)]
        assert_eq!(
            parse_osc7_path("file://localhost/C:/Users/dev%20user/proj"),
            Some(PathBuf::from("C:/Users/dev user/proj"))
        );
    }
}
