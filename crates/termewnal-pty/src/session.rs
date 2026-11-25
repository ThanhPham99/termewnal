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
    pub working_dir: Option<PathBuf>,
}

impl Default for PtySpawnConfig {
    fn default() -> Self {
        Self {
            shell: detect_default_shell(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
            cursor_shape: CursorShape::Block,
            working_dir: None,
        }
    }
}

/// Lifecycle and grid notifications emitted by a running [`PtySession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    Output,
    Title(String),
    Bell,
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
    pub bracketed_paste: bool,
    pub mouse_mode: bool,
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
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    buffer: Arc<TerminalBuffer>,
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
        let buffer = Arc::new(TerminalBuffer::new_with_cursor_shape(
            dims.cols,
            dims.rows,
            config.scrollback_lines,
            config.cursor_shape,
        ));
        buffer.set_write_sink(Some(Box::new(SharedPtyWriter(Arc::clone(&writer)))));

        spawn_reader_thread(reader, child, Arc::clone(&buffer));

        Ok(Self {
            shell: config.shell,
            master: Mutex::new(pair.master),
            writer,
            killer: Mutex::new(killer),
            buffer,
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
            let master = self.master.lock().expect("master pty mutex poisoned");
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

fn spawn_reader_thread(
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    buffer: Arc<TerminalBuffer>,
) {
    thread::Builder::new()
        .name("termewnal-pty-reader".to_string())
        .spawn(move || {
            let mut buf = [0_u8; 8192];

            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        debug!("PTY reader hit EOF");
                        break;
                    }
                    Ok(n) => {
                        buffer.feed_bytes(&buf[..n]);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        warn!(error = %err, "PTY reader stopped with error");
                        break;
                    }
                }
            }

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
