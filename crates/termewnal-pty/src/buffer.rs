use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::{Color as TermColor, Processor, StdSyncHandler};
use tokio::sync::{Notify, mpsc};

use crate::session::{CursorSnapshot, PtyEvent, TermDimensions, TerminalCell, TerminalSnapshot};

type SharedWriter = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

#[derive(Clone)]
struct BufferEventProxy {
    events_tx: mpsc::UnboundedSender<PtyEvent>,
    write_sink: SharedWriter,
}

impl EventListener for BufferEventProxy {
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
                if let Ok(mut guard) = self.write_sink.lock()
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

/// Thread-safe `alacritty_terminal::Term` state machine shared by local PTY and SSH `Session`s.
///
/// Feeds raw VTE/ANSI byte streams into the terminal grid, emits [`PtyEvent`] notifications,
/// supports terminal query responses via an optional write sink, and produces immutable
/// [`TerminalSnapshot`] values for rendering and testing.
pub struct TerminalBuffer {
    term: Mutex<Term<BufferEventProxy>>,
    processor: Mutex<Processor<StdSyncHandler>>,
    dimensions: Mutex<TermDimensions>,
    write_sink: SharedWriter,
    update_signal: Arc<(Mutex<(u64, bool, Option<i32>)>, Condvar)>,
    async_notify: Arc<Notify>,
    events_tx: mpsc::UnboundedSender<PtyEvent>,
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<PtyEvent>>>,
}

impl TerminalBuffer {
    /// Creates a new [`TerminalBuffer`] sized to `(cols, rows)` with `scrollback_lines` history.
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        let dims = TermDimensions::new(cols, rows);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let write_sink: SharedWriter = Arc::new(Mutex::new(None));
        let proxy = BufferEventProxy {
            events_tx: events_tx.clone(),
            write_sink: Arc::clone(&write_sink),
        };
        let term_config = TermConfig {
            scrolling_history: scrollback_lines,
            ..TermConfig::default()
        };

        Self {
            term: Mutex::new(Term::new(term_config, &dims, proxy)),
            processor: Mutex::new(Processor::new()),
            dimensions: Mutex::new(dims),
            write_sink,
            update_signal: Arc::new((Mutex::new((0_u64, false, None)), Condvar::new())),
            async_notify: Arc::new(Notify::new()),
            events_tx,
            events_rx: Mutex::new(Some(events_rx)),
        }
    }

    /// Attaches or clears the sink receiving terminal-initiated replies (`TermEvent::PtyWrite`).
    pub fn set_write_sink(&self, sink: Option<Box<dyn Write + Send>>) {
        let mut guard = self
            .write_sink
            .lock()
            .expect("terminal write sink mutex poisoned");
        *guard = sink;
    }

    /// Feeds raw terminal output bytes into the `alacritty_terminal` VTE parser and emits
    /// [`PtyEvent::Output`].
    pub fn feed_bytes(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        {
            let mut term = self.term.lock().expect("term mutex poisoned");
            let mut processor = self.processor.lock().expect("processor mutex poisoned");
            processor.advance(&mut *term, bytes);
        }
        {
            let (lock, cvar) = &*self.update_signal;
            let mut state = lock.lock().expect("signal mutex poisoned");
            state.0 = state.0.wrapping_add(1);
            cvar.notify_all();
        }
        self.async_notify.notify_waiters();
        let _ = self.events_tx.send(PtyEvent::Output);
    }

    /// Emits a wake-up [`PtyEvent::Output`] signal without feeding new bytes.
    pub fn notify_output(&self) {
        self.async_notify.notify_waiters();
        let _ = self.events_tx.send(PtyEvent::Output);
    }

    /// Marks the underlying session stream as exited and emits [`PtyEvent::Exited`].
    pub fn mark_exited(&self, exit_code: Option<i32>) {
        {
            let (lock, cvar) = &*self.update_signal;
            let mut state = lock.lock().expect("signal mutex poisoned");
            state.1 = true;
            state.2 = exit_code;
            cvar.notify_all();
        }
        self.async_notify.notify_waiters();
        let _ = self.events_tx.send(PtyEvent::Exited(exit_code));
    }

    /// Takes the async event receiver once for driving an external event loop or UI subscription.
    pub fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<PtyEvent>> {
        self.events_rx.lock().ok()?.take()
    }

    /// Returns the current `(cols, rows)` dimensions.
    pub fn dimensions(&self) -> (u16, u16) {
        let dims = *self.dimensions.lock().expect("dimensions mutex poisoned");
        (dims.cols, dims.rows)
    }

    /// Resizes the `alacritty_terminal` grid to `(cols, rows)` clamped to at least `1x1`.
    pub fn resize(&self, cols: u16, rows: u16) {
        let dims = TermDimensions::new(cols, rows);
        {
            let mut term = self.term.lock().expect("term mutex poisoned");
            term.resize(dims);
        }
        *self.dimensions.lock().expect("dimensions mutex poisoned") = dims;
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

    /// Asynchronously awaits up to `timeout` until `predicate(&snapshot)` evaluates to `true`
    /// without blocking the Tokio executor thread.
    pub async fn wait_for_output_async<F>(&self, timeout: Duration, mut predicate: F) -> bool
    where
        F: FnMut(&TerminalSnapshot) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.async_notify.notified();
            let snap = self.snapshot();
            if predicate(&snap) {
                return true;
            }
            let exited = {
                let (lock, _) = &*self.update_signal;
                lock.lock().expect("signal mutex poisoned").1
            };
            if exited {
                return predicate(&self.snapshot());
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return predicate(&self.snapshot());
            }
        }
    }
}

/// Captures an immutable [`TerminalSnapshot`] from any `alacritty_terminal::Term`.
pub fn snapshot_from_term<L: EventListener>(term: &Term<L>, dims: TermDimensions) -> TerminalSnapshot {
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
