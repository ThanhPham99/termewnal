use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::{
    Color as TermColor, CursorShape, Processor, StdSyncHandler,
};
use alacritty_terminal::vte::{Parser as VteParser, Perform};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::error::PtyError;
use crate::shell::{ShellSpec, detect_default_shell};

/// Configuration for spawning a local shell [`PtySession`].
#[derive(Debug, Clone)]
pub struct PtySpawnConfig {
    pub shell: ShellSpec,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_lines: usize,
    pub working_dir: Option<PathBuf>,
}

impl Default for PtySpawnConfig {
    fn default() -> Self {
        Self {
            shell: detect_default_shell(),
            cols: 80,
            rows: 24,
            scrollback_lines: 10_000,
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

/// Immutable snapshot of the visible terminal grid and cursor for rendering and testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<TerminalCell>,
    pub cursor: Option<CursorSnapshot>,
}

impl TerminalSnapshot {
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

#[derive(Clone)]
struct TermEventProxy {
    events_tx: mpsc::UnboundedSender<PtyEvent>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
}

impl EventListener for TermEventProxy {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Wakeup => {
                let _ = self.events_tx.send(PtyEvent::Output);
            }
            TermEvent::Title(title) => {
                let _ = self.events_tx.send(PtyEvent::Title(title));
            }
            TermEvent::Bell => {
                let _ = self.events_tx.send(PtyEvent::Bell);
            }
            TermEvent::PtyWrite(response) => {
                if let Ok(mut guard) = self.writer.lock()
                    && let Some(writer) = guard.as_mut()
                {
                    let _ = writer.write_all(response.as_bytes());
                    let _ = writer.flush();
                }
            }
            _ => {}
        }
    }
}

/// Active local shell PTY backed by `portable-pty` and an `alacritty_terminal::Term` state machine.
pub struct PtySession {
    shell: ShellSpec,
    child_pid: Option<u32>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    term: Arc<Mutex<Term<TermEventProxy>>>,
    dimensions: Mutex<TermDimensions>,
    last_working_dir: Arc<Mutex<Option<PathBuf>>>,
    osc7_working_dir: Arc<Mutex<Option<PathBuf>>>,
    update_signal: Arc<(Mutex<(u64, bool, Option<i32>)>, Condvar)>,
    events_tx: mpsc::UnboundedSender<PtyEvent>,
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<PtyEvent>>>,
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
        let (events_tx, events_rx) = mpsc::unbounded_channel();

        let proxy = TermEventProxy {
            events_tx: events_tx.clone(),
            writer: Arc::clone(&writer),
        };
        let term_config = TermConfig {
            scrolling_history: config.scrollback_lines,
            ..TermConfig::default()
        };
        let term = Arc::new(Mutex::new(Term::new(term_config, &dims, proxy)));
        let update_signal = Arc::new((Mutex::new((0_u64, false, None)), Condvar::new()));
        let last_working_dir = Arc::new(Mutex::new(config.working_dir.clone()));
        let osc7_working_dir = Arc::new(Mutex::new(None));

        spawn_reader_thread(
            reader,
            child,
            child_pid,
            Arc::clone(&term),
            Arc::clone(&last_working_dir),
            Arc::clone(&osc7_working_dir),
            Arc::clone(&update_signal),
            events_tx.clone(),
        );

        Ok(Self {
            shell: config.shell,
            child_pid,
            master: Mutex::new(pair.master),
            writer,
            killer: Mutex::new(killer),
            term,
            dimensions: Mutex::new(dims),
            last_working_dir,
            osc7_working_dir,
            update_signal,
            events_tx,
            events_rx: Mutex::new(Some(events_rx)),
        })
    }

    /// Returns the shell specification used to launch this session.
    pub fn shell(&self) -> &ShellSpec {
        &self.shell
    }

    /// Captures the current working directory of this local shell `Session`.
    ///
    /// On Linux, queries `/proc/<pid>/cwd` for the live shell (or foreground process group leader)
    /// and caches the result, falling back to OSC 7 reports or the last retained directory once
    /// the child exits.
    pub fn current_working_dir(&self) -> Option<PathBuf> {
        let exited = self
            .update_signal
            .0
            .lock()
            .map(|state| state.1)
            .unwrap_or(false);

        if !exited {
            let fg_pid = self
                .master
                .lock()
                .ok()
                .and_then(|master| master.process_group_leader())
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
        self.events_rx.lock().ok()?.take()
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
        let dims = *self.dimensions.lock().expect("dimensions mutex poisoned");
        (dims.cols, dims.rows)
    }

    /// Resizes both the OS PTY device (`TIOCSWINSZ`) and the `alacritty_terminal` grid.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), PtyError> {
        let dims = TermDimensions::new(cols, rows);
        {
            let current = *self.dimensions.lock().expect("dimensions mutex poisoned");
            if current == dims {
                return Ok(());
            }
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

        {
            let mut term = self.term.lock().expect("term mutex poisoned");
            term.resize(dims);
        }

        *self.dimensions.lock().expect("dimensions mutex poisoned") = dims;
        Ok(())
    }

    /// Captures an immutable snapshot of the current visible grid and cursor.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let term = self.term.lock().expect("term mutex poisoned");
        let dims = *self.dimensions.lock().expect("dimensions mutex poisoned");
        snapshot_from_term(&term, dims)
    }

    /// Blocks up to `timeout` until `predicate(&snapshot)` evaluates to `true`.
    pub fn wait_for_output<F>(&self, timeout: Duration, mut predicate: F) -> bool
    where
        F: FnMut(&TerminalSnapshot) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let (lock, cvar) = &*self.update_signal;
        let mut state = lock.lock().expect("signal mutex poisoned");

        loop {
            let snap = self.snapshot();
            if predicate(&snap) {
                return true;
            }
            if state.1 {
                // Child exited; re-check one last time and exit wait immediately.
                return predicate(&self.snapshot());
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next_state, wait_res) = cvar
                .wait_timeout(state, remaining)
                .expect("signal condvar wait");
            state = next_state;
            if wait_res.timed_out() {
                return predicate(&self.snapshot());
            }
        }
    }

    /// Sends a termination signal to the child process and closes the PTY writer.
    pub fn terminate(&self) {
        if let Ok(mut guard) = self.writer.lock() {
            *guard = None;
        }
        if let Ok(mut killer) = self.killer.lock() {
            let _ = killer.kill();
        }
        let _ = self.events_tx.send(PtyEvent::Output);
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
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    child_pid: Option<u32>,
    term: Arc<Mutex<Term<TermEventProxy>>>,
    last_working_dir: Arc<Mutex<Option<PathBuf>>>,
    osc7_working_dir: Arc<Mutex<Option<PathBuf>>>,
    update_signal: Arc<(Mutex<(u64, bool, Option<i32>)>, Condvar)>,
    events_tx: mpsc::UnboundedSender<PtyEvent>,
) {
    thread::Builder::new()
        .name("termewnal-pty-reader".to_string())
        .spawn(move || {
            let mut processor: Processor<StdSyncHandler> = Processor::new();
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
                        {
                            let mut guard = term.lock().expect("term mutex poisoned");
                            processor.advance(&mut *guard, &buf[..n]);
                        }
                        if let Some(cwd) = read_proc_cwd(child_pid)
                            && let Ok(mut guard) = last_working_dir.lock()
                        {
                            *guard = Some(cwd);
                        }
                        {
                            let (lock, cvar) = &*update_signal;
                            let mut state = lock.lock().expect("signal mutex poisoned");
                            state.0 = state.0.wrapping_add(1);
                            cvar.notify_all();
                        }
                        let _ = events_tx.send(PtyEvent::Output);
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

            {
                let (lock, cvar) = &*update_signal;
                let mut state = lock.lock().expect("signal mutex poisoned");
                state.1 = true;
                state.2 = exit_code;
                cvar.notify_all();
            }
            let _ = events_tx.send(PtyEvent::Exited(exit_code));
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

fn snapshot_from_term(term: &Term<TermEventProxy>, dims: TermDimensions) -> TerminalSnapshot {
    let content = term.renderable_content();
    let cols = dims.cols as usize;
    let rows = dims.rows as usize;
    let mut cells = vec![TerminalCell::default(); cols * rows];

    for indexed in content.display_iter {
        let row = indexed.point.line.0 + content.display_offset as i32;
        let col = indexed.point.column.0;
        if row >= 0 && (row as usize) < rows && col < cols {
            let fg = resolve_override(indexed.cell.fg, content.colors);
            let bg = resolve_override(indexed.cell.bg, content.colors);
            cells[row as usize * cols + col] = TerminalCell {
                c: indexed.cell.c,
                fg,
                bg,
                flags: indexed.cell.flags,
            };
        }
    }

    let cursor_row = content.cursor.point.line.0 + content.display_offset as i32;
    let cursor_col = content.cursor.point.column.0;
    let cursor = if cursor_row >= 0 && (cursor_row as usize) < rows && cursor_col < cols {
        Some(CursorSnapshot {
            col: cursor_col,
            row: cursor_row as usize,
            shape: content.cursor.shape,
        })
    } else {
        None
    };

    TerminalSnapshot {
        cols: dims.cols,
        rows: dims.rows,
        cells,
        cursor,
    }
}

fn resolve_override(
    color: TermColor,
    overrides: &alacritty_terminal::term::color::Colors,
) -> TermColor {
    match color {
        TermColor::Named(named) => overrides[named].map(TermColor::Spec).unwrap_or(color),
        TermColor::Indexed(idx) => overrides[idx as usize].map(TermColor::Spec).unwrap_or(color),
        TermColor::Spec(_) => color,
    }
}
