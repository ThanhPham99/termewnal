//! Custom Remote File Editor state, search/replace engine, Undo/Redo history, and Iced widget.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Instant;

use iced::advanced::clipboard::{self, Clipboard};
use iced::advanced::graphics::text::{Editor as GraphicsEditor, cosmic_text};
use iced::advanced::input_method::{self, InputMethod};
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::text::editor::{
    Action, Cursor, Edit, Editor as _, Motion, Position, Selection,
};
use iced::advanced::text::highlighter::PlainText;
use iced::advanced::text::{LineHeight, Renderer as _, Shaping, Text, Wrapping};
use iced::advanced::widget::operation::{self, Operation};
use iced::advanced::widget::tree::{self, Tree};
use iced::advanced::{Renderer as _, Shell, Widget, mouse};
use iced::widget::Id;
use iced::{
    Border, Color, Element, Event, Font, Length, Pixels, Point, Rectangle, Size, Theme, Vector,
    keyboard, window,
};
use regex::RegexBuilder;
use termewnal_core::EditorIndentMode;

use crate::ui_theme::ChromePalette;

/// Focusable widget ID for the Remote File Editor text surface.
pub const REMOTE_EDITOR_WIDGET_ID: &str = "sftp-remote-file-editor";
/// Focusable widget ID for the Find query input inside Remote File Editor.
pub const REMOTE_EDITOR_FIND_INPUT_ID: &str = "sftp-remote-editor-find";
/// Focusable widget ID for the Replace input inside Remote File Editor.
pub const REMOTE_EDITOR_REPLACE_INPUT_ID: &str = "sftp-remote-editor-replace";
/// Focusable widget ID for the Go to Line input inside Remote File Editor.
pub const REMOTE_EDITOR_GOTO_INPUT_ID: &str = "sftp-remote-editor-goto";

const MAX_UNDO_STEPS: usize = 100;
const MAX_UNDO_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const TYPING_COALESCE_WINDOW_MS: u128 = 600;
const HORIZONTAL_SCROLL_UNBOUNDED_WIDTH: f32 = 1_000_000.0;
const VERTICAL_SCROLLBAR_WIDTH: f32 = 10.0;
const HORIZONTAL_SCROLLBAR_HEIGHT: f32 = 10.0;

pub fn remote_editor_id() -> Id {
    Id::new(REMOTE_EDITOR_WIDGET_ID)
}

pub fn remote_editor_find_id() -> Id {
    Id::new(REMOTE_EDITOR_FIND_INPUT_ID)
}

pub fn remote_editor_replace_id() -> Id {
    Id::new(REMOTE_EDITOR_REPLACE_INPUT_ID)
}

pub fn remote_editor_goto_id() -> Id {
    Id::new(REMOTE_EDITOR_GOTO_INPUT_ID)
}

/// Detected or selected line ending convention for the Remote File Editor buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorLineEnding {
    #[default]
    Lf,
    CrLf,
}

impl EditorLineEnding {
    /// Infers the dominant line ending from raw UTF-8 file text.
    pub fn detect(raw: &str) -> Self {
        let bytes = raw.as_bytes();
        let mut crlf = 0usize;
        let mut bare_lf = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                if i > 0 && bytes[i - 1] == b'\r' {
                    crlf += 1;
                } else {
                    bare_lf += 1;
                }
            }
        }
        if crlf > 0 && crlf >= bare_lf {
            Self::CrLf
        } else {
            Self::Lf
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
        }
    }

    pub const fn toggled(self) -> Self {
        match self {
            Self::Lf => Self::CrLf,
            Self::CrLf => Self::Lf,
        }
    }
}

/// Search matching flags toggled in the Remote File Editor Find bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditorSearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// Single search match spanning UTF-8 byte offsets and line/byte-column coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorMatch {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_col_byte: usize,
    pub end_line: usize,
    pub end_col_byte: usize,
}

/// Action queued from the Remote File Editor widget to `AppState`.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteEditorEvent {
    Action(Action),
    OpenFind,
    OpenReplace,
    OpenGoToLine,
    Save,
    Undo,
    Redo,
    FindNext,
    FindPrev,
    Indent,
    Unindent,
    Escape,
    ScrollHorizontal(f32),
    JumpToLine(usize),
}

/// Pending confirmation when attempting a destructive action with unsaved changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsavedConfirmAction {
    CloseModal,
    ReloadFromRemote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypingKind {
    Insert,
    Backspace,
    Delete,
}

#[derive(Debug, Clone)]
struct UndoSnapshot {
    text: String,
    cursor: Cursor,
    line_ending: EditorLineEnding,
    revision: u64,
}

/// Complete editing, search/replace, undo/redo, and viewport state for one Remote File Editor modal.
#[derive(Debug)]
pub struct RemoteEditorState {
    editor: RefCell<GraphicsEditor>,
    cached_text: String,
    line_starts: Vec<usize>,
    longest_line_chars: usize,
    pub line_ending: EditorLineEnding,
    saved_line_ending: EditorLineEnding,
    pub indent_mode: EditorIndentMode,
    pub word_wrap: bool,
    pub expanded: bool,
    scroll_x: Cell<f32>,
    max_visual_line_w: Cell<f32>,
    last_text_bounds: Cell<Size>,
    scroll_to_cursor_pending: Cell<bool>,
    scroll_center_pending: Cell<bool>,
    revision: u64,
    saved_revision: u64,
    saving_revision: Option<u64>,
    undo_stack: Vec<UndoSnapshot>,
    redo_stack: Vec<UndoSnapshot>,
    last_typing_coalesce: Option<(TypingKind, Instant)>,
    pub find_open: bool,
    pub replace_open: bool,
    pub find_query: String,
    pub replace_text: String,
    pub search_options: EditorSearchOptions,
    pub search_error: Option<String>,
    pub matches: Vec<EditorMatch>,
    pub active_match: Option<usize>,
    pub replace_feedback: Option<String>,
    pub goto_open: bool,
    pub goto_input: String,
    pub goto_error: Option<String>,
    pub pending_confirm: Option<UnsavedConfirmAction>,
    pub saved_status_note: Option<String>,
    pub restore_line_after_reload: Option<usize>,
}

impl Default for RemoteEditorState {
    fn default() -> Self {
        Self::new(false, false, EditorIndentMode::default())
    }
}

impl Clone for RemoteEditorState {
    fn clone(&self) -> Self {
        let mut editor = GraphicsEditor::with_text(&self.cached_text);
        editor.move_to(self.cursor());
        Self {
            editor: RefCell::new(editor),
            cached_text: self.cached_text.clone(),
            line_starts: self.line_starts.clone(),
            longest_line_chars: self.longest_line_chars,
            line_ending: self.line_ending,
            saved_line_ending: self.saved_line_ending,
            indent_mode: self.indent_mode,
            word_wrap: self.word_wrap,
            expanded: self.expanded,
            scroll_x: Cell::new(self.scroll_x.get()),
            max_visual_line_w: Cell::new(self.max_visual_line_w.get()),
            last_text_bounds: Cell::new(self.last_text_bounds.get()),
            scroll_to_cursor_pending: Cell::new(self.scroll_to_cursor_pending.get()),
            scroll_center_pending: Cell::new(self.scroll_center_pending.get()),
            revision: self.revision,
            saved_revision: self.saved_revision,
            saving_revision: self.saving_revision,
            undo_stack: self.undo_stack.clone(),
            redo_stack: self.redo_stack.clone(),
            last_typing_coalesce: self.last_typing_coalesce,
            find_open: self.find_open,
            replace_open: self.replace_open,
            find_query: self.find_query.clone(),
            replace_text: self.replace_text.clone(),
            search_options: self.search_options,
            search_error: self.search_error.clone(),
            matches: self.matches.clone(),
            active_match: self.active_match,
            replace_feedback: self.replace_feedback.clone(),
            goto_open: self.goto_open,
            goto_input: self.goto_input.clone(),
            goto_error: self.goto_error.clone(),
            pending_confirm: self.pending_confirm,
            saved_status_note: self.saved_status_note.clone(),
            restore_line_after_reload: self.restore_line_after_reload,
        }
    }
}

impl RemoteEditorState {
    /// Creates an empty editor initialized with persisted user preferences.
    pub fn new(word_wrap: bool, expanded: bool, indent_mode: EditorIndentMode) -> Self {
        Self {
            editor: RefCell::new(GraphicsEditor::with_text("")),
            cached_text: String::new(),
            line_starts: vec![0],
            longest_line_chars: 0,
            line_ending: EditorLineEnding::Lf,
            saved_line_ending: EditorLineEnding::Lf,
            indent_mode,
            word_wrap,
            expanded,
            scroll_x: Cell::new(0.0),
            max_visual_line_w: Cell::new(0.0),
            last_text_bounds: Cell::new(Size::ZERO),
            scroll_to_cursor_pending: Cell::new(false),
            scroll_center_pending: Cell::new(false),
            revision: 0,
            saved_revision: 0,
            saving_revision: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_typing_coalesce: None,
            find_open: false,
            replace_open: false,
            find_query: String::new(),
            replace_text: String::new(),
            search_options: EditorSearchOptions::default(),
            search_error: None,
            matches: Vec::new(),
            active_match: None,
            replace_feedback: None,
            goto_open: false,
            goto_input: String::new(),
            goto_error: None,
            pending_confirm: None,
            saved_status_note: None,
            restore_line_after_reload: None,
        }
    }

    /// Loads freshly fetched remote text, resetting undo history and preserving optional reload line.
    pub fn load_remote_text(&mut self, raw: &str) {
        let detected = EditorLineEnding::detect(raw);
        let normalized = raw.replace("\r\n", "\n");
        let target_line = self.restore_line_after_reload.take();

        self.line_ending = detected;
        self.saved_line_ending = detected;
        self.revision = 0;
        self.saved_revision = 0;
        self.saving_revision = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_typing_coalesce = None;
        self.pending_confirm = None;
        self.replace_feedback = None;
        self.saved_status_note = None;
        self.scroll_x.set(0.0);

        *self.editor.borrow_mut() = GraphicsEditor::with_text(&normalized);
        self.rebuild_text_cache();

        if let Some(line_idx) = target_line {
            let clamped = line_idx.min(self.line_count().saturating_sub(1));
            self.set_cursor_without_selection(Position {
                line: clamped,
                column: 0,
            });
            self.scroll_to_cursor_pending.set(true);
            self.scroll_center_pending.set(true);
        }
        self.recompute_matches(false);
    }

    /// Returns normalized (`\n`) buffer text.
    pub fn text(&self) -> &str {
        &self.cached_text
    }

    /// Returns text formatted with the active [`EditorLineEnding`] for writing back over SFTP.
    pub fn text_for_save(&self) -> String {
        match self.line_ending {
            EditorLineEnding::Lf => self.cached_text.clone(),
            EditorLineEnding::CrLf => self.cached_text.replace('\n', "\r\n"),
        }
    }

    /// Returns `true` when unsaved edits or line-ending changes exist relative to the saved baseline.
    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision || self.line_ending != self.saved_line_ending
    }

    /// Marks the current in-flight revision before sending an async SFTP save request.
    pub fn begin_save(&mut self) -> String {
        self.saving_revision = Some(self.revision);
        self.saved_status_note = None;
        self.text_for_save()
    }

    /// Finalizes save completion, clearing `dirty` only if no newer edits occurred in flight.
    pub fn finish_save_ok(&mut self, timestamp_label: Option<String>) {
        if let Some(rev) = self.saving_revision.take() {
            self.saved_revision = rev;
        } else {
            self.saved_revision = self.revision;
        }
        self.saved_line_ending = self.line_ending;
        self.saved_status_note = Some(match timestamp_label {
            Some(ts) => format!("Saved {ts}"),
            None => "Saved".to_string(),
        });
    }

    /// Clears pending saving revision after a failed save.
    pub fn finish_save_err(&mut self) {
        self.saving_revision = None;
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len().max(1)
    }

    pub fn byte_len(&self) -> usize {
        self.text_for_save().len()
    }

    pub fn cursor(&self) -> Cursor {
        self.editor.borrow().cursor()
    }

    pub fn selection(&self) -> Option<String> {
        self.editor.borrow().copy()
    }

    /// Returns 1-based `(line, character_column)` for the status bar.
    pub fn cursor_line_col_1based(&self) -> (usize, usize) {
        let pos = self.cursor().position;
        let line_text = self.line_str(pos.line);
        let clamped = clamp_to_char_boundary(line_text, pos.column);
        let col_chars = line_text[..clamped].chars().count();
        (pos.line + 1, col_chars + 1)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Applies an Iced editor [`Action`], recording undo history for edits when `editable` is true.
    pub fn perform(&mut self, action: Action, editable: bool) -> bool {
        let is_edit = action.is_edit();
        if is_edit && !editable {
            return false;
        }

        match &action {
            Action::Edit(edit) => {
                self.record_undo_for_edit(edit);
                self.editor.borrow_mut().perform(action);
                self.advance_revision();
                self.rebuild_text_cache();
                self.recompute_matches(false);
                self.scroll_to_cursor_pending.set(true);
                true
            }
            Action::Scroll { .. } => {
                self.editor.borrow_mut().perform(action);
                false
            }
            Action::Drag(_) => {
                self.editor.borrow_mut().perform(action);
                false
            }
            _ => {
                self.last_typing_coalesce = None;
                self.editor.borrow_mut().perform(action);
                self.scroll_to_cursor_pending.set(true);
                false
            }
        }
    }

    /// Undoes the last editing operation, restoring buffer text, cursor, and revision.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop() else {
            return false;
        };
        let current = self.capture_snapshot();
        self.redo_stack.push(current);
        self.restore_snapshot(prev);
        true
    }

    /// Redoes the most recently undone operation.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        let current = self.capture_snapshot();
        self.undo_stack.push(current);
        self.restore_snapshot(next);
        true
    }

    /// Toggles `LF` <-> `CRLF` line endings cleanly as an undoable edit.
    pub fn toggle_line_ending(&mut self) {
        self.push_atomic_undo();
        self.line_ending = self.line_ending.toggled();
        self.advance_revision();
    }

    /// Cycles between `Spaces: 4`, `Spaces: 2`, and `Tab (\t)`.
    pub fn cycle_indent_mode(&mut self) -> EditorIndentMode {
        self.indent_mode = self.indent_mode.next();
        self.indent_mode
    }

    /// Indents the current cursor or selected line block according to `indent_mode`.
    pub fn indent(&mut self, editable: bool) -> bool {
        if !editable {
            return false;
        }
        let cursor = self.cursor();
        if let Some(sel) = cursor.selection
            && sel.line != cursor.position.line
        {
            return self.indent_multiline_block(cursor.position, sel);
        }

        self.push_atomic_undo();
        let insert = match self.indent_mode {
            EditorIndentMode::Tab => "\t".to_string(),
            EditorIndentMode::Spaces2 | EditorIndentMode::Spaces4 => {
                let width = self.indent_mode.width();
                let line_text = self.line_str(cursor.position.line);
                let col_byte = clamp_to_char_boundary(line_text, cursor.position.column);
                let char_col = line_text[..col_byte].chars().count();
                let count = width - (char_col % width);
                " ".repeat(count)
            }
        };

        self.editor
            .borrow_mut()
            .perform(Action::Edit(Edit::Paste(Arc::new(insert))));
        self.advance_revision();
        self.rebuild_text_cache();
        self.recompute_matches(false);
        self.scroll_to_cursor_pending.set(true);
        true
    }

    /// Unindents the current line or selected line block according to `indent_mode`.
    pub fn unindent(&mut self, editable: bool) -> bool {
        if !editable {
            return false;
        }
        let cursor = self.cursor();
        let (start_pos, end_pos) = ordered_positions(cursor.position, cursor.selection);
        let end_line = if cursor.selection.is_some()
            && end_pos.line > start_pos.line
            && end_pos.column == 0
        {
            end_pos.line - 1
        } else {
            end_pos.line
        };

        let mut lines: Vec<String> = self
            .cached_text
            .split('\n')
            .map(ToString::to_string)
            .collect();
        let max_spaces = self.indent_mode.width();
        let mut removed_per_line = vec![0usize; lines.len()];
        let mut any_changed = false;

        for line_i in start_pos.line..=end_line.min(lines.len().saturating_sub(1)) {
            let line = &mut lines[line_i];
            let remove_bytes = if line.starts_with('\t') {
                1
            } else {
                line.bytes().take(max_spaces).take_while(|&b| b == b' ').count()
            };
            if remove_bytes > 0 {
                line.drain(..remove_bytes);
                removed_per_line[line_i] = remove_bytes;
                any_changed = true;
            }
        }

        if !any_changed {
            return false;
        }

        self.push_atomic_undo();
        let new_text = lines.join("\n");
        let new_pos = Position {
            line: cursor.position.line,
            column: cursor
                .position
                .column
                .saturating_sub(removed_per_line.get(cursor.position.line).copied().unwrap_or(0)),
        };
        let new_sel = cursor.selection.map(|s| Position {
            line: s.line,
            column: s
                .column
                .saturating_sub(removed_per_line.get(s.line).copied().unwrap_or(0)),
        });
        self.replace_buffer_preserving_scroll(&new_text, Cursor {
            position: new_pos,
            selection: new_sel,
        });
        self.advance_revision();
        self.rebuild_text_cache();
        self.recompute_matches(false);
        self.scroll_to_cursor_pending.set(true);
        true
    }

    /// Opens the Find bar (and optionally seeds query from a single-line editor selection).
    pub fn open_find(&mut self, with_replace: bool, editable: bool) {
        self.goto_open = false;
        self.goto_error = None;
        self.find_open = true;
        self.replace_open = with_replace && editable;
        self.replace_feedback = None;

        if let Some(sel) = self.selection()
            && !sel.is_empty()
            && !sel.contains('\n')
            && !sel.contains('\r')
        {
            self.find_query = sel;
        }
        self.recompute_matches(true);
        if self.active_match.is_some() {
            self.focus_active_match();
        }
    }

    pub fn close_find(&mut self) {
        self.find_open = false;
        self.replace_open = false;
        self.search_error = None;
        self.replace_feedback = None;
    }

    pub fn set_find_query(&mut self, query: String) {
        self.find_query = query;
        self.replace_feedback = None;
        self.recompute_matches(true);
        if self.active_match.is_some() {
            self.focus_active_match();
        }
    }

    pub fn toggle_case_sensitive(&mut self) {
        self.search_options.case_sensitive = !self.search_options.case_sensitive;
        self.replace_feedback = None;
        self.recompute_matches(true);
        if self.active_match.is_some() {
            self.focus_active_match();
        }
    }

    pub fn toggle_whole_word(&mut self) {
        self.search_options.whole_word = !self.search_options.whole_word;
        self.replace_feedback = None;
        self.recompute_matches(true);
        if self.active_match.is_some() {
            self.focus_active_match();
        }
    }

    pub fn toggle_regex(&mut self) {
        self.search_options.regex = !self.search_options.regex;
        self.replace_feedback = None;
        self.recompute_matches(true);
        if self.active_match.is_some() {
            self.focus_active_match();
        }
    }

    /// Advances to the next match (wrapping around) and highlights/scrolls to it.
    pub fn find_next(&mut self) -> bool {
        if self.matches.is_empty() {
            self.active_match = None;
            return false;
        }
        let cursor_byte = self.cursor_abs_byte();
        let next = match self.active_match {
            Some(idx) if idx < self.matches.len() => (idx + 1) % self.matches.len(),
            _ => self
                .matches
                .iter()
                .position(|m| m.start_byte >= cursor_byte)
                .unwrap_or(0),
        };
        self.active_match = Some(next);
        self.focus_active_match();
        true
    }

    /// Moves to the previous match (wrapping around) and highlights/scrolls to it.
    pub fn find_prev(&mut self) -> bool {
        if self.matches.is_empty() {
            self.active_match = None;
            return false;
        }
        let prev = match self.active_match {
            Some(0) | None => self.matches.len() - 1,
            Some(idx) => idx - 1,
        };
        self.active_match = Some(prev);
        self.focus_active_match();
        true
    }

    /// Replaces the currently active match and jumps to the next match.
    pub fn replace_current(&mut self, editable: bool) -> bool {
        if !editable || self.matches.is_empty() {
            return false;
        }
        let idx = match self.active_match {
            Some(i) if i < self.matches.len() => i,
            _ => {
                self.active_match = Some(0);
                self.focus_active_match();
                return false;
            }
        };
        let target = self.matches[idx];
        let Some(replacement) = self.expand_replacement_for_match(target) else {
            return false;
        };

        self.push_atomic_undo();
        let mut updated = String::with_capacity(
            self.cached_text.len() - (target.end_byte - target.start_byte) + replacement.len(),
        );
        updated.push_str(&self.cached_text[..target.start_byte]);
        updated.push_str(&replacement);
        updated.push_str(&self.cached_text[target.end_byte..]);

        let after_byte = target.start_byte + replacement.len();
        self.apply_replaced_text(&updated, after_byte);
        self.advance_revision();
        self.rebuild_text_cache();
        self.recompute_matches(false);
        self.replace_feedback = Some("Replaced 1 occurrence".to_string());

        if !self.matches.is_empty() {
            let next_idx = self
                .matches
                .iter()
                .position(|m| m.start_byte >= after_byte)
                .unwrap_or(0);
            self.active_match = Some(next_idx);
            self.focus_active_match();
        }
        true
    }

    /// Replaces all matches across the entire file as a single atomic Undo step.
    pub fn replace_all(&mut self, editable: bool) -> usize {
        if !editable || self.matches.is_empty() {
            return 0;
        }
        let Some(replacements) = self.compute_all_replacements() else {
            return 0;
        };

        let count = replacements.len();
        self.push_atomic_undo();
        let cursor_byte = self.cursor_abs_byte();
        let mut updated = String::with_capacity(self.cached_text.len());
        let mut last = 0usize;
        for (m, repl) in &replacements {
            updated.push_str(&self.cached_text[last..m.start_byte]);
            updated.push_str(repl);
            last = m.end_byte;
        }
        updated.push_str(&self.cached_text[last..]);

        let clamped_byte = cursor_byte.min(updated.len());
        self.apply_replaced_text(&updated, clamped_byte);
        self.advance_revision();
        self.rebuild_text_cache();
        self.recompute_matches(true);
        self.replace_feedback = Some(if count == 1 {
            "Replaced 1 occurrence".to_string()
        } else {
            format!("Replaced {count} occurrences")
        });
        count
    }

    /// Formats the match counter shown in the Find bar (`"1 / 4"` or `"No matches"`).
    pub fn find_status_label(&self) -> String {
        if self.find_query.is_empty() {
            String::new()
        } else if self.search_error.is_some() {
            "Invalid regex".to_string()
        } else if let Some(idx) = self.active_match {
            format!("{} / {}", idx + 1, self.matches.len())
        } else {
            "No matches".to_string()
        }
    }

    /// Opens the inline Go to Line bar (`Ctrl+G`).
    pub fn open_goto_line(&mut self) {
        self.goto_open = true;
        self.goto_error = None;
        self.goto_input.clear();
    }

    pub fn close_goto_line(&mut self) {
        self.goto_open = false;
        self.goto_error = None;
    }

    /// Parses `line` or `line:col` (1-based) and jumps the cursor there, centering in viewport.
    pub fn submit_goto_line(&mut self) -> bool {
        let raw = self.goto_input.trim().trim_start_matches(':');
        if raw.is_empty() {
            self.goto_error = Some("Enter line or line:col".to_string());
            return false;
        }
        let (line_raw, col_raw) = match raw.split_once(':') {
            Some((l, c)) => (l.trim(), Some(c.trim())),
            None => (raw, None),
        };
        let Ok(line_1based) = line_raw.parse::<usize>() else {
            self.goto_error = Some("Invalid line number".to_string());
            return false;
        };
        if line_1based == 0 {
            self.goto_error = Some("Line must be >= 1".to_string());
            return false;
        }
        let col_1based = match col_raw {
            Some(c) if !c.is_empty() => match c.parse::<usize>() {
                Ok(v) if v >= 1 => v,
                _ => {
                    self.goto_error = Some("Invalid column number".to_string());
                    return false;
                }
            },
            _ => 1,
        };

        let line_idx = line_1based.saturating_sub(1).min(self.line_count() - 1);
        let line_text = self.line_str(line_idx);
        let col_byte = char_idx_to_byte_offset(line_text, col_1based.saturating_sub(1));

        self.set_cursor_without_selection(Position {
            line: line_idx,
            column: col_byte,
        });
        self.scroll_to_cursor_pending.set(true);
        self.scroll_center_pending.set(true);
        self.goto_open = false;
        self.goto_error = None;
        true
    }

    /// Jumps cursor to the start of `line_idx` (e.g. clicking a line number in the gutter).
    pub fn jump_to_line_start(&mut self, line_idx: usize) {
        let clamped = line_idx.min(self.line_count().saturating_sub(1));
        self.set_cursor_without_selection(Position {
            line: clamped,
            column: 0,
        });
        self.scroll_to_cursor_pending.set(true);
    }

    /// Recomputes search matches over the current text buffer.
    pub fn recompute_matches(&mut self, reset_active: bool) {
        self.matches.clear();
        self.search_error = None;
        if self.find_query.is_empty() {
            self.active_match = None;
            return;
        }

        let pattern = if self.search_options.regex {
            self.find_query.clone()
        } else {
            regex::escape(&self.find_query)
        };
        let compiled = RegexBuilder::new(&pattern)
            .case_insensitive(!self.search_options.case_sensitive)
            .multi_line(true)
            .build();

        let re = match compiled {
            Ok(r) => r,
            Err(err) => {
                self.search_error = Some(err.to_string());
                self.active_match = None;
                return;
            }
        };

        for m in re.find_iter(&self.cached_text) {
            if self.search_options.whole_word
                && !is_whole_word_boundary(&self.cached_text, m.start(), m.end())
            {
                continue;
            }
            let (start_line, start_col_byte) = self.byte_offset_to_line_col(m.start());
            let (end_line, end_col_byte) = self.byte_offset_to_line_col(m.end());
            self.matches.push(EditorMatch {
                start_byte: m.start(),
                end_byte: m.end(),
                start_line,
                start_col_byte,
                end_line,
                end_col_byte,
            });
        }

        if self.matches.is_empty() {
            self.active_match = None;
        } else if reset_active {
            let cursor_byte = self.cursor_abs_byte();
            let idx = self
                .matches
                .iter()
                .position(|m| m.start_byte >= cursor_byte)
                .unwrap_or(0);
            self.active_match = Some(idx);
        } else {
            self.active_match = Some(
                self.active_match
                    .map(|idx| idx.min(self.matches.len() - 1))
                    .unwrap_or(0),
            );
        }
    }

    fn focus_active_match(&mut self) {
        let Some(idx) = self.active_match else {
            return;
        };
        let Some(m) = self.matches.get(idx).copied() else {
            return;
        };
        let start = Position {
            line: m.start_line,
            column: m.start_col_byte,
        };
        let end = Position {
            line: m.end_line,
            column: m.end_col_byte,
        };
        if start == end {
            self.set_cursor_without_selection(end);
        } else {
            self.editor.borrow_mut().move_to(Cursor {
                position: end,
                selection: Some(start),
            });
        }
        self.scroll_to_cursor_pending.set(true);
        self.scroll_center_pending.set(true);
    }

    fn expand_replacement_for_match(&self, m: EditorMatch) -> Option<String> {
        if !self.search_options.regex {
            return Some(self.replace_text.clone());
        }
        let re = RegexBuilder::new(&self.find_query)
            .case_insensitive(!self.search_options.case_sensitive)
            .multi_line(true)
            .build()
            .ok()?;
        let caps = re.captures(&self.cached_text[m.start_byte..m.end_byte])?;
        let mut expanded = String::new();
        caps.expand(&self.replace_text, &mut expanded);
        Some(expanded)
    }

    fn compute_all_replacements(&self) -> Option<Vec<(EditorMatch, String)>> {
        if !self.search_options.regex {
            return Some(
                self.matches
                    .iter()
                    .copied()
                    .map(|m| (m, self.replace_text.clone()))
                    .collect(),
            );
        }
        let re = RegexBuilder::new(&self.find_query)
            .case_insensitive(!self.search_options.case_sensitive)
            .multi_line(true)
            .build()
            .ok()?;
        let mut out = Vec::with_capacity(self.matches.len());
        for &m in &self.matches {
            let caps = re.captures(&self.cached_text[m.start_byte..m.end_byte])?;
            let mut expanded = String::new();
            caps.expand(&self.replace_text, &mut expanded);
            out.push((m, expanded));
        }
        Some(out)
    }

    fn indent_multiline_block(&mut self, pos: Position, sel: Position) -> bool {
        let (start_pos, end_pos) = ordered_positions(pos, Some(sel));
        let end_line = if end_pos.line > start_pos.line && end_pos.column == 0 {
            end_pos.line - 1
        } else {
            end_pos.line
        };

        let mut lines: Vec<String> = self
            .cached_text
            .split('\n')
            .map(ToString::to_string)
            .collect();
        let indent_str = self.indent_mode.indent_str();
        let added = indent_str.len();
        let mut added_per_line = vec![0usize; lines.len()];

        for line_i in start_pos.line..=end_line.min(lines.len().saturating_sub(1)) {
            lines[line_i].insert_str(0, indent_str);
            added_per_line[line_i] = added;
        }

        self.push_atomic_undo();
        let new_text = lines.join("\n");
        let new_pos = Position {
            line: pos.line,
            column: pos.column + added_per_line.get(pos.line).copied().unwrap_or(0),
        };
        let new_sel = Position {
            line: sel.line,
            column: sel.column + added_per_line.get(sel.line).copied().unwrap_or(0),
        };
        self.replace_buffer_preserving_scroll(
            &new_text,
            Cursor {
                position: new_pos,
                selection: Some(new_sel),
            },
        );
        self.advance_revision();
        self.rebuild_text_cache();
        self.recompute_matches(false);
        self.scroll_to_cursor_pending.set(true);
        true
    }

    fn record_undo_for_edit(&mut self, edit: &Edit) {
        let now = Instant::now();
        let kind = match edit {
            Edit::Insert(c) if !c.is_whitespace() => Some(TypingKind::Insert),
            Edit::Backspace => Some(TypingKind::Backspace),
            Edit::Delete => Some(TypingKind::Delete),
            _ => None,
        };

        let should_coalesce = matches!(
            (kind, self.last_typing_coalesce),
            (Some(k), Some((prev_k, prev_at)))
                if k == prev_k && now.duration_since(prev_at).as_millis() <= TYPING_COALESCE_WINDOW_MS
        );

        if should_coalesce && !self.undo_stack.is_empty() {
            self.last_typing_coalesce = kind.map(|k| (k, now));
            self.redo_stack.clear();
            return;
        }

        self.push_snapshot_to_undo();
        self.last_typing_coalesce = kind.map(|k| (k, now));
    }

    fn push_atomic_undo(&mut self) {
        self.last_typing_coalesce = None;
        self.push_snapshot_to_undo();
    }

    fn push_snapshot_to_undo(&mut self) {
        let snap = self.capture_snapshot();
        self.undo_stack.push(snap);
        self.redo_stack.clear();
        self.trim_undo_stack();
    }

    fn trim_undo_stack(&mut self) {
        while self.undo_stack.len() > MAX_UNDO_STEPS {
            self.undo_stack.remove(0);
        }
        let mut total_bytes: usize = self.undo_stack.iter().map(|s| s.text.len()).sum();
        while total_bytes > MAX_UNDO_TOTAL_BYTES && self.undo_stack.len() > 1 {
            let removed = self.undo_stack.remove(0);
            total_bytes = total_bytes.saturating_sub(removed.text.len());
        }
    }

    fn capture_snapshot(&self) -> UndoSnapshot {
        UndoSnapshot {
            text: self.cached_text.clone(),
            cursor: self.cursor(),
            line_ending: self.line_ending,
            revision: self.revision,
        }
    }

    fn restore_snapshot(&mut self, snap: UndoSnapshot) {
        self.last_typing_coalesce = None;
        self.line_ending = snap.line_ending;
        self.revision = snap.revision;
        self.saved_status_note = None;
        self.replace_buffer_preserving_scroll(&snap.text, snap.cursor);
        self.rebuild_text_cache();
        self.recompute_matches(false);
        self.scroll_to_cursor_pending.set(true);
    }

    fn advance_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.replace_feedback = None;
        self.saved_status_note = None;
    }

    fn apply_replaced_text(&mut self, new_text: &str, abs_cursor_byte: usize) {
        let clamped = clamp_to_char_boundary(new_text, abs_cursor_byte);
        let pos = byte_offset_to_line_col_in(new_text, clamped);
        self.replace_buffer_preserving_scroll(
            new_text,
            Cursor {
                position: pos,
                selection: None,
            },
        );
    }

    fn replace_buffer_preserving_scroll(&mut self, new_text: &str, cursor: Cursor) {
        let prev_scroll_line = self.editor.borrow().buffer().scroll().line;
        let mut next = GraphicsEditor::with_text(new_text);
        if prev_scroll_line > 0 {
            next.perform(Action::Scroll {
                lines: prev_scroll_line as i32,
            });
        }
        if cursor.selection.is_some() {
            next.move_to(cursor);
        } else {
            next.move_to(Cursor {
                position: cursor.position,
                selection: None,
            });
        }
        *self.editor.borrow_mut() = next;
    }

    fn set_cursor_without_selection(&mut self, pos: Position) {
        let mut editor = self.editor.borrow_mut();
        if editor.copy().is_some() {
            editor.perform(Action::Move(Motion::Home));
        }
        editor.move_to(Cursor {
            position: pos,
            selection: None,
        });
    }

    fn rebuild_text_cache(&mut self) {
        let editor = self.editor.borrow();
        let line_count = editor.line_count().max(1);
        let mut out = String::new();
        let mut starts = Vec::with_capacity(line_count);
        let mut longest = 0usize;
        for i in 0..line_count {
            starts.push(out.len());
            if let Some(line) = editor.line(i) {
                longest = longest.max(line.text.chars().count());
                out.push_str(&line.text);
            }
            if i + 1 < line_count {
                out.push('\n');
            }
        }
        drop(editor);
        self.cached_text = out;
        self.line_starts = starts;
        self.longest_line_chars = longest;
    }

    fn line_str(&self, line_idx: usize) -> &str {
        let Some(&start) = self.line_starts.get(line_idx) else {
            return "";
        };
        let end = if let Some(&next_start) = self.line_starts.get(line_idx + 1) {
            next_start.saturating_sub(1)
        } else {
            self.cached_text.len()
        };
        &self.cached_text[start..end.max(start)]
    }

    fn cursor_abs_byte(&self) -> usize {
        let pos = self.cursor().position;
        let line_start = self.line_starts.get(pos.line).copied().unwrap_or(0);
        let line_text = self.line_str(pos.line);
        let col = clamp_to_char_boundary(line_text, pos.column);
        line_start + col
    }

    fn byte_offset_to_line_col(&self, offset: usize) -> (usize, usize) {
        let clamped = offset.min(self.cached_text.len());
        let line = match self.line_starts.binary_search(&clamped) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line_start = self.line_starts.get(line).copied().unwrap_or(0);
        (line, clamped.saturating_sub(line_start))
    }

    pub fn set_scroll_x(&self, x: f32) {
        let max_scroll = self.max_scroll_x();
        self.scroll_x.set(x.clamp(0.0, max_scroll));
    }

    pub fn scroll_x(&self) -> f32 {
        if self.word_wrap {
            0.0
        } else {
            self.scroll_x.get().clamp(0.0, self.max_scroll_x())
        }
    }

    fn max_scroll_x(&self) -> f32 {
        if self.word_wrap {
            return 0.0;
        }
        let text_w = self.last_text_bounds.get().width.max(1.0);
        let approx_max_w = (self.longest_line_chars as f32 * 8.2).max(self.max_visual_line_w.get());
        (approx_max_w - text_w + 32.0).max(0.0)
    }
}

/// Custom Iced code/text editor widget for the SFTP+ Remote File Editor.
pub struct RemoteEditor<'a, Message> {
    id: Option<Id>,
    state: &'a RemoteEditorState,
    editable: bool,
    chrome: ChromePalette,
    font: Font,
    text_size: Pixels,
    line_height: LineHeight,
    height: Length,
    on_event: Box<dyn Fn(RemoteEditorEvent) -> Message + 'a>,
}

impl<'a, Message> RemoteEditor<'a, Message> {
    pub fn new(
        state: &'a RemoteEditorState,
        editable: bool,
        chrome: ChromePalette,
        font: Font,
        text_size: impl Into<Pixels>,
        height: impl Into<Length>,
        on_event: impl Fn(RemoteEditorEvent) -> Message + 'a,
    ) -> Self {
        Self {
            id: Some(remote_editor_id()),
            state,
            editable,
            chrome,
            font,
            text_size: text_size.into(),
            line_height: LineHeight::Relative(1.38),
            height: height.into(),
            on_event: Box::new(on_event),
        }
    }
}

#[derive(Debug, Default)]
struct WidgetState {
    focus: Option<Focus>,
    preedit: Option<input_method::Preedit>,
    last_click: Option<mouse::Click>,
    drag_click: Option<mouse::click::Kind>,
    dragging_vscroll: bool,
    dragging_hscroll: bool,
    partial_scroll: f32,
}

#[derive(Debug, Clone)]
struct Focus {
    updated_at: Instant,
    now: Instant,
    is_window_focused: bool,
}

impl Focus {
    const CURSOR_BLINK_INTERVAL_MILLIS: u128 = 500;

    fn now() -> Self {
        let now = Instant::now();
        Self {
            updated_at: now,
            now,
            is_window_focused: true,
        }
    }

    fn is_cursor_visible(&self) -> bool {
        self.is_window_focused
            && ((self.now - self.updated_at).as_millis() / Self::CURSOR_BLINK_INTERVAL_MILLIS)
                .is_multiple_of(2)
    }
}

impl operation::Focusable for WidgetState {
    fn is_focused(&self) -> bool {
        self.focus.is_some()
    }

    fn focus(&mut self) {
        self.focus = Some(Focus::now());
    }

    fn unfocus(&mut self) {
        self.focus = None;
    }
}

#[derive(Debug, Clone, Copy)]
struct EditorGeometry {
    gutter_rect: Rectangle,
    editor_rect: Rectangle,
    text_bounds: Rectangle,
    vscroll_rect: Rectangle,
    hscroll_rect: Option<Rectangle>,
}

impl EditorGeometry {
    fn compute(
        bounds: Rectangle,
        line_count: usize,
        text_size: f32,
        has_hscroll: bool,
    ) -> Self {
        let digits = line_count.max(1).to_string().len().max(3);
        let char_w = (text_size * 0.62).max(7.0);
        let gutter_w = (digits as f32 * char_w + 18.0).min((bounds.width * 0.25).max(40.0));
        let gutter_rect = Rectangle::new(bounds.position(), Size::new(gutter_w, bounds.height));
        let editor_rect = Rectangle::new(
            Point::new(bounds.x + gutter_w, bounds.y),
            Size::new((bounds.width - gutter_w).max(1.0), bounds.height),
        );
        let bottom_pad = if has_hscroll {
            HORIZONTAL_SCROLLBAR_HEIGHT + 4.0
        } else {
            8.0
        };
        let text_bounds = Rectangle::new(
            Point::new(editor_rect.x + 10.0, editor_rect.y + 8.0),
            Size::new(
                (editor_rect.width - 10.0 - VERTICAL_SCROLLBAR_WIDTH - 4.0).max(1.0),
                (editor_rect.height - 8.0 - bottom_pad).max(1.0),
            ),
        );
        let vscroll_rect = Rectangle::new(
            Point::new(
                editor_rect.x + editor_rect.width - VERTICAL_SCROLLBAR_WIDTH - 2.0,
                editor_rect.y + 4.0,
            ),
            Size::new(
                VERTICAL_SCROLLBAR_WIDTH,
                (editor_rect.height - 8.0 - if has_hscroll { HORIZONTAL_SCROLLBAR_HEIGHT } else { 0.0 })
                    .max(1.0),
            ),
        );
        let hscroll_rect = has_hscroll.then(|| {
            Rectangle::new(
                Point::new(
                    editor_rect.x + 6.0,
                    editor_rect.y + editor_rect.height - HORIZONTAL_SCROLLBAR_HEIGHT - 2.0,
                ),
                Size::new(
                    (editor_rect.width - VERTICAL_SCROLLBAR_WIDTH - 12.0).max(1.0),
                    HORIZONTAL_SCROLLBAR_HEIGHT,
                ),
            )
        });
        Self {
            gutter_rect,
            editor_rect,
            text_bounds,
            vscroll_rect,
            hscroll_rect,
        }
    }
}

impl<Message: 'static> Widget<Message, Theme, iced::Renderer> for RemoteEditor<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<WidgetState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(WidgetState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, self.height)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let limits = limits.width(Length::Fill).height(self.height);
        let bounds_size = limits.max();
        let has_hscroll = !self.state.word_wrap && self.state.max_scroll_x() > 0.0;
        let geom = EditorGeometry::compute(
            Rectangle::new(Point::ORIGIN, bounds_size),
            self.state.line_count(),
            self.text_size.0,
            has_hscroll,
        );
        self.state.last_text_bounds.set(geom.text_bounds.size());

        let wrap = if self.state.word_wrap {
            Wrapping::WordOrGlyph
        } else {
            Wrapping::None
        };
        let editor_layout_w = if self.state.word_wrap {
            geom.text_bounds.width
        } else {
            HORIZONTAL_SCROLL_UNBOUNDED_WIDTH
        };

        let mut editor = self.state.editor.borrow_mut();
        let mut hl = PlainText;
        editor.update(
            Size::new(editor_layout_w, geom.text_bounds.height),
            self.font,
            self.text_size,
            self.line_height,
            wrap,
            &mut hl,
        );

        let line_h_px = f32::from(self.line_height.to_absolute(self.text_size)).max(1.0);
        if self.state.scroll_center_pending.replace(false) {
            if let Some(y_in_vp) = cursor_y_in_viewport(&editor) {
                let h = geom.text_bounds.height;
                if y_in_vp < h * 0.15 || y_in_vp > h * 0.85 {
                    let delta_lines = ((y_in_vp - h * 0.45) / line_h_px).round() as i32;
                    if delta_lines != 0 {
                        editor.perform(Action::Scroll { lines: delta_lines });
                    }
                }
            }
        }

        let max_run_w = editor
            .buffer()
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0f32, f32::max);
        if max_run_w > self.state.max_visual_line_w.get() {
            self.state.max_visual_line_w.set(max_run_w);
        }

        if self.state.scroll_to_cursor_pending.replace(false) && !self.state.word_wrap {
            ensure_cursor_visible_x(&editor, &self.state.scroll_x, geom.text_bounds.width, self.state.max_scroll_x());
        } else {
            self.state.set_scroll_x(self.state.scroll_x.get());
        }

        layout::Node::new(bounds_size)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<WidgetState>();
        let bounds = layout.bounds();
        let has_hscroll = !self.state.word_wrap && self.state.max_scroll_x() > 0.0;
        let geom = EditorGeometry::compute(
            bounds,
            self.state.line_count(),
            self.text_size.0,
            has_hscroll,
        );
        let is_redraw = matches!(event, Event::Window(window::Event::RedrawRequested(_)));

        match event {
            Event::Window(window::Event::Unfocused) => {
                if let Some(focus) = &mut state.focus {
                    focus.is_window_focused = false;
                }
            }
            Event::Window(window::Event::Focused) => {
                if let Some(focus) = &mut state.focus {
                    focus.is_window_focused = true;
                    focus.updated_at = Instant::now();
                    shell.request_redraw();
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                if let Some(focus) = &mut state.focus
                    && focus.is_window_focused
                {
                    focus.now = *now;
                    let millis_until_redraw = Focus::CURSOR_BLINK_INTERVAL_MILLIS
                        - (focus.now - focus.updated_at).as_millis()
                            % Focus::CURSOR_BLINK_INTERVAL_MILLIS;
                    shell.request_redraw_at(
                        focus.now + std::time::Duration::from_millis(millis_until_redraw as u64),
                    );
                }
            }
            Event::Mouse(mouse_ev) => {
                if self.handle_mouse_event(state, mouse_ev, bounds, geom, cursor, shell) {
                    return;
                }
            }
            Event::InputMethod(ime_ev) => {
                if self.editable && state.focus.is_some() {
                    match ime_ev {
                        input_method::Event::Opened => {
                            state.preedit = Some(input_method::Preedit::new());
                            shell.request_redraw();
                        }
                        input_method::Event::Closed => {
                            state.preedit = None;
                            shell.request_redraw();
                        }
                        input_method::Event::Preedit(content, selection) => {
                            state.preedit = Some(input_method::Preedit {
                                content: content.clone(),
                                selection: selection.clone(),
                                text_size: Some(self.text_size),
                            });
                            shell.request_redraw();
                        }
                        input_method::Event::Commit(text) => {
                            state.preedit = None;
                            shell.publish((self.on_event)(RemoteEditorEvent::Action(
                                Action::Edit(Edit::Paste(Arc::new(text.clone()))),
                            )));
                        }
                    }
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modified_key,
                physical_key,
                modifiers,
                text,
                ..
            }) => {
                if state.focus.is_some() {
                    if let Some(ev) = self.map_keyboard_event(
                        key,
                        modified_key,
                        *physical_key,
                        *modifiers,
                        text.as_deref(),
                        clipboard,
                    ) {
                        if let Some(focus) = &mut state.focus {
                            focus.updated_at = Instant::now();
                        }
                        if let Some(msg) = ev {
                            shell.publish((self.on_event)(msg));
                        }
                        shell.capture_event();
                    }
                }
            }
            _ => {}
        }

        if is_redraw {
            let im = self.input_method(state, renderer, geom);
            shell.request_input_method(&im);
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<WidgetState>();
        let bounds = layout.bounds();
        let has_hscroll = !self.state.word_wrap && self.state.max_scroll_x() > 0.0;
        let geom = EditorGeometry::compute(
            bounds,
            self.state.line_count(),
            self.text_size.0,
            has_hscroll,
        );
        if state.dragging_vscroll || state.dragging_hscroll {
            return mouse::Interaction::Grabbing;
        }
        if cursor.is_over(geom.vscroll_rect)
            || geom.hscroll_rect.is_some_and(|r| cursor.is_over(r))
        {
            return mouse::Interaction::Pointer;
        }
        if cursor.is_over(geom.gutter_rect) {
            return mouse::Interaction::Pointer;
        }
        if cursor.is_over(geom.editor_rect) {
            return mouse::Interaction::Text;
        }
        mouse::Interaction::default()
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _defaults: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let has_hscroll = !self.state.word_wrap && self.state.max_scroll_x() > 0.0;
        let geom = EditorGeometry::compute(
            bounds,
            self.state.line_count(),
            self.text_size.0,
            has_hscroll,
        );
        let state = tree.state.downcast_ref::<WidgetState>();
        let chrome = self.chrome;
        let scroll_x = self.state.scroll_x();

        // Outer container background + border
        renderer.fill_quad(
            Quad {
                bounds,
                border: Border {
                    color: if state.focus.is_some() {
                        chrome.accent
                    } else {
                        chrome.border_subtle
                    },
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Quad::default()
            },
            chrome.well_bg,
        );

        // Gutter background & divider hairline
        let gutter_bg = mix_color(chrome.well_bg, chrome.bg_more, 0.55);
        renderer.fill_quad(
            Quad {
                bounds: Rectangle::new(
                    Point::new(geom.gutter_rect.x + 1.0, geom.gutter_rect.y + 1.0),
                    Size::new(
                        (geom.gutter_rect.width - 1.0).max(0.0),
                        (geom.gutter_rect.height - 2.0).max(0.0),
                    ),
                ),
                border: Border {
                    radius: iced::border::Radius {
                        top_left: 5.0,
                        top_right: 0.0,
                        bottom_right: 0.0,
                        bottom_left: 5.0,
                    },
                    ..Default::default()
                },
                ..Quad::default()
            },
            gutter_bg,
        );
        renderer.fill_quad(
            Quad {
                bounds: Rectangle::new(
                    Point::new(geom.gutter_rect.x + geom.gutter_rect.width - 1.0, geom.gutter_rect.y + 1.0),
                    Size::new(1.0, (geom.gutter_rect.height - 2.0).max(0.0)),
                ),
                ..Quad::default()
            },
            chrome.border_subtle,
        );

        let editor = self.state.editor.borrow();
        let buffer = editor.buffer();
        let active_line = editor.cursor().position.line;
        let text_bounds = geom.text_bounds;
        let translation = text_bounds.position() - Point::ORIGIN - Vector::new(scroll_x, 0.0);

        // Draw search match background quads and gutter line numbers from visible cosmic_text runs
        let mut prev_line_i: Option<usize> = None;
        let line_num_clip = Rectangle::new(
            Point::new(geom.gutter_rect.x + 2.0, text_bounds.y),
            Size::new((geom.gutter_rect.width - 8.0).max(1.0), text_bounds.height),
        );

        for run in buffer.layout_runs() {
            // Gutter line number only on first visual row of a logical line
            if prev_line_i != Some(run.line_i) {
                prev_line_i = Some(run.line_i);
                let num_y = text_bounds.y + run.line_top;
                if num_y + run.line_height >= text_bounds.y && num_y <= text_bounds.y + text_bounds.height {
                    let is_current = run.line_i == active_line;
                    let num_color = if is_current {
                        chrome.text_primary
                    } else {
                        chrome.text_muted
                    };
                    renderer.fill_text(
                        Text {
                            content: (run.line_i + 1).to_string(),
                            bounds: Size::new(line_num_clip.width, run.line_height),
                            size: Pixels((self.text_size.0 - 1.0).max(10.0)),
                            line_height: LineHeight::Absolute(Pixels(run.line_height)),
                            font: self.font,
                            align_x: iced::advanced::text::Alignment::Right,
                            align_y: iced::alignment::Vertical::Top,
                            shaping: Shaping::Basic,
                            wrapping: Wrapping::None,
                        },
                        Point::new(line_num_clip.x, num_y),
                        num_color,
                        line_num_clip,
                    );
                }
            }

            // Active line subtle row tint
            if run.line_i == active_line {
                let row_rect = Rectangle::new(
                    Point::new(text_bounds.x, text_bounds.y + run.line_top),
                    Size::new(text_bounds.width, run.line_height),
                );
                if let Some(clipped) = text_bounds.intersection(&row_rect) {
                    renderer.fill_quad(
                        Quad {
                            bounds: clipped,
                            ..Quad::default()
                        },
                        Color {
                            a: 0.05,
                            ..chrome.text_primary
                        },
                    );
                }
            }

            // Search match highlights
            if !self.state.matches.is_empty() {
                for (m_idx, m) in self.state.matches.iter().enumerate() {
                    if m.end_line < run.line_i {
                        continue;
                    }
                    if m.start_line > run.line_i {
                        break;
                    }
                    let from_byte = if run.line_i == m.start_line {
                        m.start_col_byte
                    } else {
                        0
                    };
                    let to_byte = if run.line_i == m.end_line {
                        m.end_col_byte
                    } else {
                        run.text.len()
                    };
                    let (rx, rw) = if from_byte == to_byte {
                        let seg_start = run.glyphs.first().map(|g| g.start).unwrap_or(0);
                        let seg_end = run.glyphs.last().map(|g| g.end).unwrap_or(run.text.len());
                        if from_byte >= seg_start && from_byte <= seg_end {
                            (glyph_x_at_byte(&run, from_byte), 2.0)
                        } else {
                            (0.0, 0.0)
                        }
                    } else {
                        run_byte_span_x(&run, from_byte, to_byte)
                    };
                    if rw > 0.0 {
                        let rect = Rectangle::new(
                            Point::new(rx + translation.x, text_bounds.y + run.line_top),
                            Size::new(rw, run.line_height),
                        );
                        if let Some(clipped) = text_bounds.intersection(&rect) {
                            let is_active = self.state.active_match == Some(m_idx);
                            let bg = if is_active {
                                Color {
                                    a: 0.42,
                                    ..chrome.accent
                                }
                            } else {
                                Color {
                                    a: 0.28,
                                    ..chrome.warning
                                }
                            };
                            let border_col = if is_active {
                                chrome.accent
                            } else {
                                Color {
                                    a: 0.65,
                                    ..chrome.warning
                                }
                            };
                            renderer.fill_quad(
                                Quad {
                                    bounds: clipped,
                                    border: Border {
                                        color: border_col,
                                        width: if is_active { 1.0 } else { 0.5 },
                                        radius: 2.0.into(),
                                    },
                                    ..Quad::default()
                                },
                                bg,
                            );
                        }
                    }
                }
            }
        }

        // Draw selection / caret behind or over text
        match editor.selection() {
            Selection::Range(ranges) => {
                for range in ranges {
                    let shifted = range + translation;
                    if let Some(clipped) = text_bounds.intersection(&shifted) {
                        renderer.fill_quad(
                            Quad {
                                bounds: clipped,
                                ..Quad::default()
                            },
                            chrome.accent_soft,
                        );
                    }
                }
            }
            Selection::Caret(_) => {}
        }

        // Draw editor glyphs clipped to text_bounds, shifted left by scroll_x
        if !editor.is_empty() {
            renderer.fill_editor(
                &editor,
                Point::new(text_bounds.x - scroll_x, text_bounds.y),
                chrome.text_primary,
                text_bounds,
            );
        }

        // Draw caret on top when focused
        if let Some(focus) = state.focus.as_ref()
            && let Selection::Caret(position) = editor.selection()
            && focus.is_cursor_visible()
        {
            let cursor_rect = Rectangle::new(
                position + translation,
                Size::new(
                    1.5,
                    f32::from(self.line_height.to_absolute(self.text_size)),
                ),
            );
            if let Some(clipped) = text_bounds.intersection(&cursor_rect) {
                renderer.fill_quad(
                    Quad {
                        bounds: clipped,
                        ..Quad::default()
                    },
                    chrome.text_primary,
                );
            }
        }

        // Draw vertical & horizontal scrollbars
        self.draw_scrollbars(renderer, &editor, geom);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<WidgetState>();
        operation.focusable(self.id.as_ref(), layout.bounds(), state);
    }
}

impl<'a, Message: 'static> From<RemoteEditor<'a, Message>>
    for Element<'a, Message, Theme, iced::Renderer>
{
    fn from(editor: RemoteEditor<'a, Message>) -> Self {
        Self::new(editor)
    }
}

impl<Message: 'static> RemoteEditor<'_, Message> {
    fn handle_mouse_event(
        &self,
        state: &mut WidgetState,
        mouse_ev: &mouse::Event,
        bounds: Rectangle,
        geom: EditorGeometry,
        cursor: mouse::Cursor,
        shell: &mut Shell<'_, Message>,
    ) -> bool {
        let scroll_x = self.state.scroll_x();
        match mouse_ev {
            mouse::Event::ButtonPressed(mouse::Button::Left) => {
                if let Some(pos) = cursor.position_in(geom.vscroll_rect) {
                    let frac = (pos.y / geom.vscroll_rect.height.max(1.0)).clamp(0.0, 1.0);
                    let target_line =
                        (frac * self.state.line_count().saturating_sub(1) as f32).round() as usize;
                    state.dragging_vscroll = true;
                    state.focus = Some(Focus::now());
                    self.scroll_to_line(target_line);
                    shell.capture_event();
                    shell.request_redraw();
                    return true;
                }
                if let Some(hrect) = geom.hscroll_rect
                    && let Some(pos) = cursor.position_in(hrect)
                {
                    let frac = (pos.x / hrect.width.max(1.0)).clamp(0.0, 1.0);
                    state.dragging_hscroll = true;
                    state.focus = Some(Focus::now());
                    shell.publish((self.on_event)(RemoteEditorEvent::ScrollHorizontal(
                        frac * self.state.max_scroll_x(),
                    )));
                    shell.capture_event();
                    shell.request_redraw();
                    return true;
                }
                if let Some(pos) = cursor.position_in(geom.gutter_rect) {
                    state.focus = Some(Focus::now());
                    if let Some(line_idx) =
                        self.line_at_gutter_y(pos.y - (geom.text_bounds.y - bounds.y))
                    {
                        shell.publish((self.on_event)(RemoteEditorEvent::JumpToLine(line_idx)));
                    }
                    shell.capture_event();
                    shell.request_redraw();
                    return true;
                }
                if let Some(pos) = cursor.position()
                    && geom.editor_rect.contains(pos)
                {
                    let local = Point::new(
                        pos.x - geom.text_bounds.x + scroll_x,
                        pos.y - geom.text_bounds.y,
                    );
                    let click = mouse::Click::new(local, mouse::Button::Left, state.last_click);
                    let action = match click.kind() {
                        mouse::click::Kind::Single => Action::Click(click.position()),
                        mouse::click::Kind::Double => Action::SelectWord,
                        mouse::click::Kind::Triple => Action::SelectLine,
                    };
                    state.focus = Some(Focus::now());
                    state.last_click = Some(click);
                    state.drag_click = Some(click.kind());
                    shell.publish((self.on_event)(RemoteEditorEvent::Action(action)));
                    shell.capture_event();
                    return true;
                } else if state.focus.is_some() {
                    state.focus = None;
                    state.drag_click = None;
                    shell.request_redraw();
                }
                false
            }
            mouse::Event::ButtonReleased(mouse::Button::Left) => {
                let was = state.drag_click.take().is_some()
                    || state.dragging_vscroll
                    || state.dragging_hscroll;
                state.dragging_vscroll = false;
                state.dragging_hscroll = false;
                was
            }
            mouse::Event::CursorMoved { .. } => {
                if state.dragging_vscroll
                    && let Some(pos) = cursor.position()
                {
                    let frac = ((pos.y - geom.vscroll_rect.y) / geom.vscroll_rect.height.max(1.0))
                        .clamp(0.0, 1.0);
                    let target_line =
                        (frac * self.state.line_count().saturating_sub(1) as f32).round() as usize;
                    self.scroll_to_line(target_line);
                    shell.capture_event();
                    shell.request_redraw();
                    return true;
                }
                if state.dragging_hscroll
                    && let Some(hrect) = geom.hscroll_rect
                    && let Some(pos) = cursor.position()
                {
                    let frac = ((pos.x - hrect.x) / hrect.width.max(1.0)).clamp(0.0, 1.0);
                    shell.publish((self.on_event)(RemoteEditorEvent::ScrollHorizontal(
                        frac * self.state.max_scroll_x(),
                    )));
                    shell.capture_event();
                    return true;
                }
                if matches!(state.drag_click, Some(mouse::click::Kind::Single))
                    && let Some(pos) = cursor.position()
                {
                    let local = Point::new(
                        pos.x - geom.text_bounds.x + scroll_x,
                        pos.y - geom.text_bounds.y,
                    );
                    shell.publish((self.on_event)(RemoteEditorEvent::Action(Action::Drag(
                        local,
                    ))));
                    return true;
                }
                false
            }
            mouse::Event::WheelScrolled { delta } if cursor.is_over(bounds) => {
                let (dx, dy) = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => (
                        if x.abs() > 0.0 { -x * 28.0 } else { 0.0 },
                        if y.abs() > 0.0 {
                            y.signum() * -(y.abs() * 4.0).max(1.0)
                        } else {
                            0.0
                        },
                    ),
                    mouse::ScrollDelta::Pixels { x, y } => (-x, -y / 4.0),
                };
                if dx.abs() > 0.1 && !self.state.word_wrap {
                    shell.publish((self.on_event)(RemoteEditorEvent::ScrollHorizontal(
                        self.state.scroll_x() + dx,
                    )));
                }
                if dy.abs() > 0.0 {
                    let lines = dy + state.partial_scroll;
                    state.partial_scroll = lines.fract();
                    if lines as i32 != 0 {
                        shell.publish((self.on_event)(RemoteEditorEvent::Action(
                            Action::Scroll {
                                lines: lines as i32,
                            },
                        )));
                    }
                }
                shell.capture_event();
                true
            }
            _ => false,
        }
    }

    fn line_at_gutter_y(&self, rel_y: f32) -> Option<usize> {
        let editor = self.state.editor.borrow();
        let mut last = None;
        for run in editor.buffer().layout_runs() {
            if rel_y >= run.line_top && rel_y <= run.line_top + run.line_height {
                return Some(run.line_i);
            }
            last = Some(run.line_i);
        }
        last
    }

    fn scroll_to_line(&self, target_line: usize) {
        let mut editor = self.state.editor.borrow_mut();
        let line_h = editor.buffer().metrics().line_height.max(1.0);
        let cur = editor.buffer().scroll().line as f32 + editor.buffer().scroll().vertical / line_h;
        let delta = (target_line as f32 - cur).round() as i32;
        if delta != 0 {
            editor.perform(Action::Scroll { lines: delta });
        }
    }

    fn draw_scrollbars(
        &self,
        renderer: &mut iced::Renderer,
        editor: &GraphicsEditor,
        geom: EditorGeometry,
    ) {
        let chrome = self.chrome;
        let line_count = self.state.line_count().max(1);
        let line_h = editor.buffer().metrics().line_height.max(1.0);
        let visible_lines = (geom.text_bounds.height / line_h).max(1.0);
        if (line_count as f32) > visible_lines {
            let track = geom.vscroll_rect;
            let ratio = (visible_lines / line_count as f32).clamp(0.08, 1.0);
            let thumb_h = (track.height * ratio).max(20.0);
            let scroll = editor.buffer().scroll();
            let current_line = (scroll.line as f32 + scroll.vertical / line_h).max(0.0);
            let max_top = (line_count as f32 - visible_lines).max(1.0);
            let progress = (current_line / max_top).clamp(0.0, 1.0);
            let thumb_y = track.y + progress * (track.height - thumb_h);
            renderer.fill_quad(
                Quad {
                    bounds: Rectangle::new(
                        Point::new(track.x + 2.0, thumb_y),
                        Size::new(5.0, thumb_h),
                    ),
                    border: Border {
                        radius: 3.0.into(),
                        ..Default::default()
                    },
                    ..Quad::default()
                },
                Color {
                    a: 0.45,
                    ..chrome.text_muted
                },
            );
        }

        if let Some(htrack) = geom.hscroll_rect {
            let max_sx = self.state.max_scroll_x();
            if max_sx > 0.0 {
                let total_w = geom.text_bounds.width + max_sx;
                let ratio = (geom.text_bounds.width / total_w).clamp(0.08, 1.0);
                let thumb_w = (htrack.width * ratio).max(24.0);
                let progress = (self.state.scroll_x() / max_sx).clamp(0.0, 1.0);
                let thumb_x = htrack.x + progress * (htrack.width - thumb_w);
                renderer.fill_quad(
                    Quad {
                        bounds: Rectangle::new(
                            Point::new(thumb_x, htrack.y + 2.0),
                            Size::new(thumb_w, 5.0),
                        ),
                        border: Border {
                            radius: 3.0.into(),
                            ..Default::default()
                        },
                        ..Quad::default()
                    },
                    Color {
                        a: 0.45,
                        ..chrome.text_muted
                    },
                );
            }
        }
    }

    fn map_keyboard_event(
        &self,
        key: &keyboard::Key,
        modified_key: &keyboard::Key,
        physical_key: keyboard::key::Physical,
        modifiers: keyboard::Modifiers,
        text: Option<&str>,
        clipboard: &mut dyn Clipboard,
    ) -> Option<Option<RemoteEditorEvent>> {
        if let Some(latin) = key.to_latin(physical_key)
            && modifiers.command()
            && !modifiers.alt()
        {
            match latin {
                'f' if !modifiers.shift() => return Some(Some(RemoteEditorEvent::OpenFind)),
                'h' if !modifiers.shift() => return Some(Some(RemoteEditorEvent::OpenReplace)),
                'g' if !modifiers.shift() => return Some(Some(RemoteEditorEvent::OpenGoToLine)),
                's' if !modifiers.shift() => return Some(Some(RemoteEditorEvent::Save)),
                'z' if !modifiers.shift() => return Some(Some(RemoteEditorEvent::Undo)),
                'z' if modifiers.shift() => return Some(Some(RemoteEditorEvent::Redo)),
                'y' if !modifiers.shift() => return Some(Some(RemoteEditorEvent::Redo)),
                'a' => return Some(Some(RemoteEditorEvent::Action(Action::SelectAll))),
                'c' => {
                    if let Some(sel) = self.state.selection() {
                        clipboard.write(clipboard::Kind::Standard, sel);
                    }
                    return Some(None);
                }
                'x' => {
                    if self.editable
                        && let Some(sel) = self.state.selection()
                    {
                        clipboard.write(clipboard::Kind::Standard, sel);
                        return Some(Some(RemoteEditorEvent::Action(Action::Edit(Edit::Delete))));
                    }
                    return Some(None);
                }
                'v' => {
                    if self.editable
                        && let Some(contents) = clipboard.read(clipboard::Kind::Standard)
                    {
                        return Some(Some(RemoteEditorEvent::Action(Action::Edit(
                            Edit::Paste(Arc::new(contents)),
                        ))));
                    }
                    return Some(None);
                }
                _ => {}
            }
        }

        match modified_key.as_ref() {
            keyboard::Key::Named(keyboard::key::Named::Escape) if modifiers.is_empty() => {
                return Some(Some(RemoteEditorEvent::Escape));
            }
            keyboard::Key::Named(keyboard::key::Named::F3) => {
                return Some(Some(if modifiers.shift() {
                    RemoteEditorEvent::FindPrev
                } else {
                    RemoteEditorEvent::FindNext
                }));
            }
            keyboard::Key::Named(keyboard::key::Named::Tab)
                if !modifiers.control() && !modifiers.alt() && !modifiers.logo() =>
            {
                return Some(if self.editable {
                    Some(if modifiers.shift() {
                        RemoteEditorEvent::Unindent
                    } else {
                        RemoteEditorEvent::Indent
                    })
                } else {
                    None
                });
            }
            keyboard::Key::Named(keyboard::key::Named::Enter)
                if !modifiers.control() && !modifiers.alt() && !modifiers.logo() =>
            {
                return Some(self.editable.then_some(RemoteEditorEvent::Action(
                    Action::Edit(Edit::Enter),
                )));
            }
            keyboard::Key::Named(keyboard::key::Named::Backspace) => {
                return Some(self.editable.then_some(RemoteEditorEvent::Action(
                    Action::Edit(Edit::Backspace),
                )));
            }
            keyboard::Key::Named(keyboard::key::Named::Delete)
                if text.is_none() || text == Some("\u{7f}") =>
            {
                return Some(self.editable.then_some(RemoteEditorEvent::Action(
                    Action::Edit(Edit::Delete),
                )));
            }
            _ => {}
        }

        if let Some(raw_text) = text
            && !modifiers.control()
            && !modifiers.alt()
            && !modifiers.logo()
            && let Some(ch) = raw_text.chars().find(|c| !c.is_control())
        {
            return Some(
                self.editable
                    .then_some(RemoteEditorEvent::Action(Action::Edit(Edit::Insert(ch)))),
            );
        }

        if let keyboard::Key::Named(named) = key.as_ref()
            && let Some(motion) = map_named_motion(named)
        {
            let motion = if modifiers.macos_command() {
                match motion {
                    Motion::Left => Motion::Home,
                    Motion::Right => Motion::End,
                    _ => motion,
                }
            } else {
                motion
            };
            let motion = if modifiers.jump() {
                motion.widen()
            } else {
                motion
            };
            let action = if modifiers.shift() {
                Action::Select(motion)
            } else {
                Action::Move(motion)
            };
            return Some(Some(RemoteEditorEvent::Action(action)));
        }

        None
    }

    fn input_method<'b>(
        &self,
        state: &'b WidgetState,
        _renderer: &iced::Renderer,
        geom: EditorGeometry,
    ) -> InputMethod<&'b str> {
        let Some(Focus {
            is_window_focused: true,
            ..
        }) = &state.focus
        else {
            return InputMethod::Disabled;
        };

        let editor = self.state.editor.borrow();
        let translation =
            geom.text_bounds.position() - Point::ORIGIN - Vector::new(self.state.scroll_x(), 0.0);
        let cursor = match editor.selection() {
            Selection::Caret(position) => position,
            Selection::Range(ranges) => ranges.first().cloned().unwrap_or_default().position(),
        };
        let line_height = self.line_height.to_absolute(self.text_size);
        InputMethod::Enabled {
            cursor: Rectangle::new(cursor + translation, Size::new(1.0, f32::from(line_height))),
            purpose: input_method::Purpose::Normal,
            preedit: state.preedit.as_ref().map(input_method::Preedit::as_ref),
        }
    }
}

fn map_named_motion(key: keyboard::key::Named) -> Option<Motion> {
    use keyboard::key::Named;
    Some(match key {
        Named::ArrowLeft => Motion::Left,
        Named::ArrowRight => Motion::Right,
        Named::ArrowUp => Motion::Up,
        Named::ArrowDown => Motion::Down,
        Named::Home => Motion::Home,
        Named::End => Motion::End,
        Named::PageUp => Motion::PageUp,
        Named::PageDown => Motion::PageDown,
        _ => return None,
    })
}

fn cursor_y_in_viewport(editor: &GraphicsEditor) -> Option<f32> {
    match editor.selection() {
        Selection::Caret(pt) => Some(pt.y),
        Selection::Range(ranges) => ranges.first().map(|r| r.y),
    }
}

fn ensure_cursor_visible_x(
    editor: &GraphicsEditor,
    scroll_x: &Cell<f32>,
    viewport_w: f32,
    max_scroll_x: f32,
) {
    let caret_x = match editor.selection() {
        Selection::Caret(pt) => Some(pt.x),
        Selection::Range(ranges) => ranges.first().map(|r| r.x),
    };
    let Some(x) = caret_x else {
        return;
    };
    let mut sx = scroll_x.get();
    let margin = 24.0_f32.min(viewport_w * 0.25);
    if x < sx + margin {
        sx = (x - margin).max(0.0);
    } else if x > sx + viewport_w - margin {
        sx = (x - viewport_w + margin).clamp(0.0, max_scroll_x);
    }
    scroll_x.set(sx.clamp(0.0, max_scroll_x));
}

fn run_byte_span_x(run: &cosmic_text::LayoutRun<'_>, from: usize, to: usize) -> (f32, f32) {
    let x_start = glyph_x_at_byte(run, from);
    let x_end = glyph_x_at_byte(run, to);
    let left = x_start.min(x_end);
    (left, (x_end - x_start).abs())
}

fn glyph_x_at_byte(run: &cosmic_text::LayoutRun<'_>, byte_idx: usize) -> f32 {
    for glyph in run.glyphs {
        if byte_idx <= glyph.start {
            return glyph.x;
        }
        if byte_idx < glyph.end {
            let span = (glyph.end - glyph.start).max(1) as f32;
            let frac = (byte_idx - glyph.start) as f32 / span;
            return glyph.x + glyph.w * frac;
        }
    }
    run.glyphs.last().map(|g| g.x + g.w).unwrap_or(0.0)
}

fn clamp_to_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn char_idx_to_byte_offset(line: &str, char_idx: usize) -> usize {
    line.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(line.len())
}

fn ordered_positions(a: Position, b: Option<Position>) -> (Position, Position) {
    let Some(b) = b else {
        return (a, a);
    };
    if (a.line, a.column) <= (b.line, b.column) {
        (a, b)
    } else {
        (b, a)
    }
}

fn byte_offset_to_line_col_in(text: &str, abs_byte: usize) -> Position {
    let mut line = 0usize;
    let mut line_start = 0usize;
    for (i, b) in text.bytes().enumerate().take(abs_byte) {
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    Position {
        line,
        column: abs_byte.saturating_sub(line_start),
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_whole_word_boundary(text: &str, start: usize, end: usize) -> bool {
    let before_ok = text[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !is_word_char(c));
    let after_ok = text[end..]
        .chars()
        .next()
        .is_none_or(|c| !is_word_char(c));
    before_ok && after_ok
}

fn mix_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_case_whole_word_regex_and_wrap_around() {
        let mut ed = RemoteEditorState::new(false, false, EditorIndentMode::Spaces4);
        ed.load_remote_text("Foo foo_bar foo\n$foo foo\n");

        // Case-insensitive plain search matches Foo, foo_bar, foo, $foo, foo
        ed.open_find(false, true);
        ed.set_find_query("foo".to_string());
        assert_eq!(ed.matches.len(), 5);
        assert_eq!(ed.active_match, Some(0));

        // Case-sensitive (Aa) skips "Foo"
        ed.toggle_case_sensitive();
        assert_eq!(ed.matches.len(), 4);

        // Whole word (ab) skips "foo_bar", keeps "foo", "$foo", "foo"
        ed.toggle_whole_word();
        assert_eq!(ed.matches.len(), 3);
        assert_eq!(ed.find_status_label(), "1 / 3");
        assert!(ed.find_next());
        assert_eq!(ed.active_match, Some(1));
        assert!(ed.find_next());
        assert_eq!(ed.active_match, Some(2));
        assert!(ed.find_next());
        assert_eq!(ed.active_match, Some(0));
        assert!(ed.find_prev());
        assert_eq!(ed.active_match, Some(2));

        // Regex error displays inline error without panic
        ed.toggle_whole_word();
        ed.toggle_regex();
        ed.set_find_query("(unclosed".to_string());
        assert!(ed.search_error.is_some());
        assert_eq!(ed.find_status_label(), "Invalid regex");
    }

    #[test]
    fn replace_regex_capture_replace_all_and_undo_redo_restore_clean_state() {
        let mut ed = RemoteEditorState::new(false, false, EditorIndentMode::Spaces4);
        ed.load_remote_text("id=10\nid=20\nid=30\n");
        assert!(!ed.is_dirty());

        ed.open_find(true, true);
        ed.toggle_regex();
        ed.set_find_query(r"id=(\d+)".to_string());
        ed.replace_text = "num:$1".to_string();
        assert_eq!(ed.matches.len(), 3);

        assert!(ed.replace_current(true));
        assert!(ed.is_dirty());
        assert_eq!(ed.text(), "num:10\nid=20\nid=30\n");
        assert_eq!(ed.matches.len(), 2);

        let count = ed.replace_all(true);
        assert_eq!(count, 2);
        assert_eq!(ed.text(), "num:10\nnum:20\nnum:30\n");
        assert_eq!(ed.replace_feedback.as_deref(), Some("Replaced 2 occurrences"));

        // Undo Replace All -> Undo single Replace -> back to clean revision!
        assert!(ed.undo());
        assert_eq!(ed.text(), "num:10\nid=20\nid=30\n");
        assert!(ed.is_dirty());
        assert!(ed.undo());
        assert_eq!(ed.text(), "id=10\nid=20\nid=30\n");
        assert!(!ed.is_dirty());

        // Redo restores both operations cleanly
        assert!(ed.redo());
        assert_eq!(ed.text(), "num:10\nid=20\nid=30\n");
        assert!(ed.is_dirty());
        assert!(ed.redo());
        assert_eq!(ed.text(), "num:10\nnum:20\nnum:30\n");
    }

    #[test]
    fn indent_unindent_line_endings_goto_line_and_mid_save_revision() {
        let mut ed = RemoteEditorState::new(false, false, EditorIndentMode::Spaces2);
        ed.load_remote_text("alpha\r\nbeta\r\ngamma\r\n");
        assert_eq!(ed.line_ending, EditorLineEnding::CrLf);
        assert_eq!(ed.text_for_save(), "alpha\r\nbeta\r\ngamma\r\n");

        // Select lines 0..=1 and indent/unindent
        ed.editor.borrow_mut().move_to(Cursor {
            position: Position { line: 1, column: 4 },
            selection: Some(Position { line: 0, column: 0 }),
        });
        assert!(ed.indent(true));
        assert_eq!(ed.text(), "  alpha\n  beta\ngamma\n");
        assert!(ed.unindent(true));
        assert_eq!(ed.text(), "alpha\nbeta\ngamma\n");

        // Line ending toggle marks dirty and converts save payload
        ed.toggle_line_ending();
        assert_eq!(ed.line_ending, EditorLineEnding::Lf);
        assert!(ed.is_dirty());
        assert_eq!(ed.text_for_save(), "alpha\nbeta\ngamma\n");

        // Mid-save revision check: edit while save is in flight keeps dirty = true
        let _payload = ed.begin_save();
        ed.perform(Action::Edit(Edit::Insert('!')), true);
        ed.finish_save_ok(Some("12:00:00".to_string()));
        assert!(ed.is_dirty());

        // Go to line:col validation & jump
        ed.open_goto_line();
        ed.goto_input = "0".to_string();
        assert!(!ed.submit_goto_line());
        assert!(ed.goto_error.is_some());

        ed.goto_input = "2:3".to_string();
        assert!(ed.submit_goto_line());
        assert!(!ed.goto_open);
        assert_eq!(ed.cursor_line_col_1based(), (2, 3));
    }

    #[test]
    fn open_find_auto_seeds_single_line_selection_only() {
        let mut ed = RemoteEditorState::new(false, false, EditorIndentMode::Spaces4);
        ed.load_remote_text("hello world\nsecond line\n");
        ed.find_query = "previous".to_string();

        // Single-line selection seeds query
        ed.editor.borrow_mut().move_to(Cursor {
            position: Position { line: 0, column: 11 },
            selection: Some(Position { line: 0, column: 6 }),
        });
        ed.open_find(false, false);
        assert_eq!(ed.find_query, "world");

        // Multi-line selection keeps existing query
        ed.editor.borrow_mut().move_to(Cursor {
            position: Position { line: 1, column: 4 },
            selection: Some(Position { line: 0, column: 0 }),
        });
        ed.open_find(false, false);
        assert_eq!(ed.find_query, "world");
    }
}
