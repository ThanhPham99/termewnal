use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode, viewport_to_point};
use alacritty_terminal::vte::ansi::{
    Color as TermColor, CursorShape, CursorStyle as VteCursorStyle, Processor, StdSyncHandler,
};
use tokio::sync::{Notify, mpsc};

use crate::ergonomics::{GridSearchMatch, search_term_matches};
use crate::session::{
    CursorSnapshot, GridRange, PtyEvent, TermDimensions, TerminalCell, TerminalSnapshot,
};

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
/// supports scrollback search, text selection, terminal query responses, and produces immutable
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
    /// Creates a new [`TerminalBuffer`] sized to `(cols, rows)` with `scrollback_lines` history
    /// and a default block cursor.
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        Self::new_with_cursor_shape(cols, rows, scrollback_lines, CursorShape::Block)
    }

    /// Creates a new [`TerminalBuffer`] with an explicit initial [`CursorShape`].
    pub fn new_with_cursor_shape(
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
        cursor_shape: CursorShape,
    ) -> Self {
        let dims = TermDimensions::new(cols, rows);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let write_sink: SharedWriter = Arc::new(Mutex::new(None));
        let proxy = BufferEventProxy {
            events_tx: events_tx.clone(),
            write_sink: Arc::clone(&write_sink),
        };
        let term_config = TermConfig {
            scrolling_history: scrollback_lines,
            default_cursor_style: VteCursorStyle {
                shape: cursor_shape,
                blinking: false,
            },
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

    /// Scrolls the viewport display offset within retained scrollback history and returns the new offset.
    pub fn scroll_display(&self, scroll: Scroll) -> usize {
        let mut term = self.term.lock().expect("term mutex poisoned");
        term.scroll_display(scroll);
        term.grid().display_offset()
    }

    /// Resets the viewport scroll offset back to the live terminal bottom (`0`).
    pub fn scroll_to_bottom(&self) -> usize {
        self.scroll_display(Scroll::Bottom)
    }

    /// Returns the current number of lines scrolled above the live bottom.
    pub fn display_offset(&self) -> usize {
        let term = self.term.lock().expect("term mutex poisoned");
        term.grid().display_offset()
    }

    /// Returns the number of scrollback lines currently stored in history.
    pub fn history_size(&self) -> usize {
        let term = self.term.lock().expect("term mutex poisoned");
        term.grid().history_size()
    }

    /// Applies updated scrollback capacity and default cursor shape to the running terminal.
    pub fn apply_options(&self, scrollback_lines: usize, cursor_shape: CursorShape) {
        let mut term = self.term.lock().expect("term mutex poisoned");
        term.set_options(TermConfig {
            scrolling_history: scrollback_lines,
            default_cursor_style: VteCursorStyle {
                shape: cursor_shape,
                blinking: false,
            },
            ..TermConfig::default()
        });
    }

    /// Returns `true` when the active terminal application has enabled bracketed paste mode.
    pub fn is_bracketed_paste(&self) -> bool {
        let term = self.term.lock().expect("term mutex poisoned");
        term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Returns `true` when the active terminal application captures mouse events.
    pub fn is_mouse_mode(&self) -> bool {
        let term = self.term.lock().expect("term mutex poisoned");
        term.mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Searches scrollback history and visible screen for `query`, returning ordered matches.
    pub fn search_scrollback(&self, query: &str) -> Vec<GridSearchMatch> {
        let term = self.term.lock().expect("term mutex poisoned");
        search_term_matches(&term, query)
    }

    /// Scrolls the viewport if needed so `target` becomes visible, returning the new offset.
    pub fn scroll_to_match(&self, target: GridSearchMatch) -> usize {
        let mut term = self.term.lock().expect("term mutex poisoned");
        let display_offset = term.grid().display_offset();
        let screen_lines = term.screen_lines() as i32;
        let history_size = term.grid().history_size() as i32;
        let vis_top = -(display_offset as i32);
        let vis_bottom = vis_top + screen_lines - 1;

        if target.start.line >= vis_top && target.start.line <= vis_bottom {
            return display_offset;
        }

        let target_offset = ((screen_lines / 2) - target.start.line).clamp(0, history_size) as usize;
        let delta = target_offset as i32 - display_offset as i32;
        term.scroll_display(Scroll::Delta(delta));
        term.grid().display_offset()
    }

    /// Starts a terminal grid selection at viewport `(col, row)`.
    pub fn start_selection(&self, col: usize, row: usize, side: Side, ty: SelectionType) {
        let mut term = self.term.lock().expect("term mutex poisoned");
        let point = clamp_viewport_point(&term, col, row);
        term.selection = Some(Selection::new(ty, point, side));
    }

    /// Extends the active terminal selection to viewport `(col, row)`.
    pub fn update_selection(&self, col: usize, row: usize, side: Side) {
        let mut term = self.term.lock().expect("term mutex poisoned");
        let point = clamp_viewport_point(&term, col, row);
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    /// Finalizes the active selection, clearing degenerate click-only selections and returning text.
    pub fn finish_selection(&self) -> Option<String> {
        let mut term = self.term.lock().expect("term mutex poisoned");
        if term.selection.as_ref().is_some_and(Selection::is_empty) {
            term.selection = None;
            return None;
        }
        term.selection_to_string().filter(|text| !text.is_empty())
    }

    /// Extracts the currently selected text without modifying selection state.
    pub fn selection_text(&self) -> Option<String> {
        let term = self.term.lock().expect("term mutex poisoned");
        term.selection_to_string().filter(|text| !text.is_empty())
    }

    /// Clears any active selection in the terminal grid.
    pub fn clear_selection(&self) {
        let mut term = self.term.lock().expect("term mutex poisoned");
        term.selection = None;
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

fn clamp_viewport_point<L: EventListener>(
    term: &Term<L>,
    col: usize,
    row: usize,
) -> GridPoint {
    let max_col = term.columns().saturating_sub(1);
    let max_row = term.screen_lines().saturating_sub(1);
    let viewport_pt = GridPoint::new(row.min(max_row), Column(col.min(max_col)));
    viewport_to_point(term.grid().display_offset(), viewport_pt)
}

/// Captures an immutable [`TerminalSnapshot`] from any `alacritty_terminal::Term`.
pub fn snapshot_from_term<L: EventListener>(term: &Term<L>, dims: TermDimensions) -> TerminalSnapshot {
    let content = term.renderable_content();
    let display_offset = content.display_offset;
    let history_size = term.grid().history_size();
    let mode = *term.mode();
    let selection = content.selection.map(|sel| GridRange {
        start_line: sel.start.line.0,
        start_col: sel.start.column.0,
        end_line: sel.end.line.0,
        end_col: sel.end.column.0,
        is_block: sel.is_block,
    });

    let cols = dims.cols as usize;
    let rows = dims.rows as usize;
    let mut cells = vec![TerminalCell::default(); cols * rows];

    for indexed in content.display_iter {
        let row = indexed.point.line.0 + display_offset as i32;
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

    let cursor_row = content.cursor.point.line.0 + display_offset as i32;
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
        display_offset,
        history_size,
        top_line: -(display_offset as i32),
        selection,
        bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
        mouse_mode: mode.intersects(TermMode::MOUSE_MODE),
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
