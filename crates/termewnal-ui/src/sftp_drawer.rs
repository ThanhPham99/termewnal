use std::path::PathBuf;

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer::{self, Quad, Renderer as _};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::widget::{
    button, checkbox, column, container, progress_bar, row, rule, scrollable, space, text,
    text_input,
};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Length, Point, Rectangle, Size, Theme,
    Vector, mouse, window,
};
use termewnal_core::ThemePalette;
use termewnal_ssh::{
    ConflictResolution, RemoteDirEntry, RemoteEntryKind, RemoteListing, SftpStatusNotice,
    SftpTransportMode, TransferConflict, TransferDirection, TransferId, TransferNotice,
    TransferQueueSnapshot, TransferSnapshot, TransferStatus,
};

use crate::app::{Message, SessionId};
use crate::ui_theme::{
    ChromePalette, PillTone, badge_pill, card_container_style, modal_card_style, modal_overlay,
    pill_button_style, status_dot, styled_input_style,
};

/// Default width in logical pixels of the right-docked SFTP Browser & `Transfer` Drawer.
pub const DEFAULT_SFTP_DRAWER_WIDTH: f32 = 360.0;
/// Minimum width in logical pixels when resizing the SFTP Drawer.
pub const MIN_SFTP_DRAWER_WIDTH: f32 = 260.0;
/// Maximum width in logical pixels when resizing the SFTP Drawer.
pub const MAX_SFTP_DRAWER_WIDTH: f32 = 760.0;

const DIVIDER_THICKNESS: f32 = 4.0;
const DIVIDER_HIT_PADDING: f32 = 3.0;

/// UI state backing the toggleable (`Ctrl+Shift+E`), resizable SFTP Browser & `Transfer` Queue Drawer.
#[derive(Debug, Clone)]
pub struct SftpDrawerState {
    open: bool,
    width: f32,
    bound_session_id: Option<SessionId>,
    current_path: String,
    path_input: String,
    new_folder_name: String,
    entries: Vec<RemoteDirEntry>,
    loading: bool,
    browser_error: Option<String>,
    transport_notice: Option<SftpStatusNotice>,
    last_transfer_notice: Option<String>,
    active_conflict_id: Option<TransferId>,
    conflict_rename_input: String,
    conflict_apply_to_all: bool,
    download_dir: PathBuf,
    queue_snapshot: TransferQueueSnapshot,
    list_generation: u64,
}

impl Default for SftpDrawerState {
    fn default() -> Self {
        let download_dir = std::env::temp_dir().join("termewnal-downloads");
        Self {
            open: false,
            width: DEFAULT_SFTP_DRAWER_WIDTH,
            bound_session_id: None,
            current_path: ".".to_string(),
            path_input: ".".to_string(),
            new_folder_name: String::new(),
            entries: Vec::new(),
            loading: false,
            browser_error: None,
            transport_notice: None,
            last_transfer_notice: None,
            active_conflict_id: None,
            conflict_rename_input: String::new(),
            conflict_apply_to_all: false,
            download_dir,
            queue_snapshot: TransferQueueSnapshot::default(),
            list_generation: 0,
        }
    }
}

impl SftpDrawerState {
    /// Creates a closed [`SftpDrawerState`] saving downloads into `download_dir`.
    pub fn with_download_dir(download_dir: PathBuf) -> Self {
        Self {
            download_dir,
            ..Self::default()
        }
    }

    /// Returns `true` when the SFTP Drawer is visible.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens or closes the SFTP Drawer.
    pub fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    /// Toggles SFTP Drawer visibility and returns the updated state.
    pub fn toggle(&mut self) -> bool {
        self.open = !self.open;
        self.open
    }

    /// Returns the current drawer width in logical pixels.
    pub fn width(&self) -> f32 {
        self.width
    }

    /// Updates drawer width clamped to `[MIN_SFTP_DRAWER_WIDTH, MAX_SFTP_DRAWER_WIDTH]`.
    pub fn set_width(&mut self, width: f32) {
        self.width = width.clamp(MIN_SFTP_DRAWER_WIDTH, MAX_SFTP_DRAWER_WIDTH);
    }

    /// Returns the SSH `SessionId` currently bound to the SFTP Browser, if any.
    pub fn bound_session_id(&self) -> Option<SessionId> {
        self.bound_session_id
    }

    /// Binds the SFTP Browser to `session_id`, resetting path/entries when switching sessions.
    pub fn bind_session(&mut self, session_id: Option<SessionId>) {
        if self.bound_session_id != session_id {
            self.bound_session_id = session_id;
            self.current_path = ".".to_string();
            self.path_input = ".".to_string();
            self.entries.clear();
            self.browser_error = None;
            self.transport_notice = None;
            self.loading = false;
        }
    }

    /// Returns the current remote directory path displayed in the SFTP Browser.
    pub fn current_path(&self) -> &str {
        &self.current_path
    }

    /// Returns the editable path bar draft text.
    pub fn path_input(&self) -> &str {
        &self.path_input
    }

    /// Updates the editable path bar draft text.
    pub fn set_path_input(&mut self, value: String) {
        self.path_input = value;
    }

    /// Updates the target path immediately (useful before async navigation completes).
    pub fn set_current_path(&mut self, path: impl Into<String>) {
        let p = path.into();
        let normalized = if p.trim().is_empty() {
            ".".to_string()
        } else {
            p.trim().to_string()
        };
        self.current_path = normalized.clone();
        self.path_input = normalized;
    }

    /// Returns the remote directory path used when uploading dropped files for `session_id`.
    pub fn target_remote_dir_for(&self, session_id: SessionId) -> String {
        if self.bound_session_id == Some(session_id) && !self.current_path.trim().is_empty() {
            self.current_path.clone()
        } else {
            ".".to_string()
        }
    }

    /// Returns the draft folder name for inline `mkdir`.
    pub fn new_folder_name(&self) -> &str {
        &self.new_folder_name
    }

    /// Updates the draft folder name for inline `mkdir`.
    pub fn set_new_folder_name(&mut self, name: String) {
        self.new_folder_name = name;
    }

    /// Returns the cached remote directory entries.
    pub fn entries(&self) -> &[RemoteDirEntry] {
        &self.entries
    }

    /// Returns `true` while an SFTP directory listing request is in flight.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Marks the browser as loading and advances the request generation counter.
    pub fn begin_listing(&mut self, session_id: SessionId, requested_path: &str) -> u64 {
        if self.bound_session_id != Some(session_id) {
            self.bound_session_id = Some(session_id);
        }
        if !requested_path.trim().is_empty() {
            self.current_path = requested_path.trim().to_string();
            self.path_input = self.current_path.clone();
        }
        self.list_generation = self.list_generation.wrapping_add(1);
        self.loading = true;
        self.browser_error = None;
        self.list_generation
    }

    /// Applies an asynchronous [`RemoteListing`] or error result if `generation` is current.
    pub fn apply_browse_result(
        &mut self,
        session_id: SessionId,
        generation: u64,
        result: Result<RemoteListing, String>,
    ) {
        if self.bound_session_id != Some(session_id) || generation != self.list_generation {
            return;
        }
        self.loading = false;
        match result {
            Ok(listing) => {
                self.current_path = listing.resolved_path.clone();
                self.path_input = listing.resolved_path;
                self.entries = listing.entries;
                self.browser_error = None;
                self.transport_notice = Some(SftpStatusNotice {
                    mode: listing.transport_mode,
                    message: listing.notice,
                });
            }
            Err(err) => {
                self.browser_error = Some(err);
            }
        }
    }

    /// Returns the most recent non-blocking SFTP browser error, if any.
    pub fn browser_error(&self) -> Option<&str> {
        self.browser_error.as_deref()
    }

    /// Sets or clears the non-blocking SFTP browser error message.
    pub fn set_browser_error(&mut self, error: Option<String>) {
        self.browser_error = error;
    }

    /// Returns the active SFTP transport mode & notice (e.g., `MuxFallback`), if known.
    pub fn transport_notice(&self) -> Option<&SftpStatusNotice> {
        self.transport_notice.as_ref()
    }

    /// Updates the surfaced SFTP transport mode & notice.
    pub fn set_transport_notice(&mut self, notice: Option<SftpStatusNotice>) {
        self.transport_notice = notice;
    }

    /// Returns the latest non-blocking `Transfer` status notice (e.g., `.part` fallback or
    /// skipped symlink notice), if any.
    pub fn last_transfer_notice(&self) -> Option<&str> {
        self.last_transfer_notice.as_deref()
    }

    /// Sets or clears the latest non-blocking `Transfer` status notice.
    pub fn set_last_transfer_notice(&mut self, notice: Option<String>) {
        self.last_transfer_notice = notice;
    }

    /// Returns the currently focused pending conflict prompt, if any.
    pub fn active_conflict(&self) -> Option<&TransferConflict> {
        let conflicts = self.queue_snapshot.pending_conflicts();
        if let Some(active_id) = self.active_conflict_id
            && let Some(found) = conflicts.iter().find(|c| c.transfer_id == active_id)
        {
            return Some(*found);
        }
        conflicts.into_iter().next()
    }

    /// Returns the editable target filename shown in the conflict resolution dialog.
    pub fn conflict_rename_input(&self) -> &str {
        &self.conflict_rename_input
    }

    /// Updates the conflict rename input field text.
    pub fn set_conflict_rename_input(&mut self, input: String) {
        self.conflict_rename_input = input;
    }

    /// Returns whether "Apply to all" is toggled in the conflict resolution dialog.
    pub fn conflict_apply_to_all(&self) -> bool {
        self.conflict_apply_to_all
    }

    /// Updates the "Apply to all" checkbox state in the conflict resolution dialog.
    pub fn set_conflict_apply_to_all(&mut self, apply_to_all: bool) {
        self.conflict_apply_to_all = apply_to_all;
    }

    /// Returns the local directory used as default destination for SFTP downloads.
    pub fn download_dir(&self) -> &PathBuf {
        &self.download_dir
    }

    /// Overrides the local directory used as default destination for SFTP downloads.
    pub fn set_download_dir(&mut self, dir: PathBuf) {
        self.download_dir = dir;
    }

    /// Replaces the cached [`TransferQueueSnapshot`] reflected in the drawer UI.
    pub fn set_queue_snapshot(&mut self, snapshot: TransferQueueSnapshot) {
        if self.transport_notice.is_none()
            && let Some(latest_notice) = snapshot.transfers.iter().rev().find_map(|t| {
                t.transport_mode.map(|mode| SftpStatusNotice {
                    mode,
                    message: t.status_notice.clone(),
                })
            })
        {
            self.transport_notice = Some(latest_notice);
        }
        if let Some(latest_transfer_notice) = snapshot.transfers.iter().rev().find_map(|t| {
            t.notice
                .as_ref()
                .map(TransferNotice::summary)
                .or_else(|| t.status_notice.clone())
        }) {
            self.last_transfer_notice = Some(latest_transfer_notice);
        }
        let next_conflict = snapshot.pending_conflicts().into_iter().next().cloned();
        match next_conflict {
            Some(conflict) => {
                if self.active_conflict_id != Some(conflict.transfer_id) {
                    self.active_conflict_id = Some(conflict.transfer_id);
                    self.conflict_rename_input = conflict.suggested_name;
                    self.conflict_apply_to_all = false;
                }
            }
            None => {
                self.active_conflict_id = None;
                self.conflict_rename_input.clear();
                self.conflict_apply_to_all = false;
            }
        }
        self.queue_snapshot = snapshot;
    }

    /// Returns the latest [`TransferQueueSnapshot`].
    pub fn queue_snapshot(&self) -> &TransferQueueSnapshot {
        &self.queue_snapshot
    }

    /// Returns all tracked `Transfer` snapshots currently displayed in the drawer.
    pub fn transfers(&self) -> &[TransferSnapshot] {
        &self.queue_snapshot.transfers
    }
}

#[derive(Debug, Default)]
struct DrawerDragState {
    dragging: bool,
    hovered: bool,
}

/// Right-docked container pairing `main` workspace content with a resizable SFTP Drawer
/// that also captures OS file drops landed anywhere inside the drawer bounds.
pub struct SftpDrawerDock<'a> {
    drawer_width: f32,
    children: [Element<'a, Message, Theme, iced::Renderer>; 2],
}

impl<'a> SftpDrawerDock<'a> {
    /// Wraps `main` on the left and `drawer` on the right with a draggable divider.
    pub fn new(
        drawer_width: f32,
        main: Element<'a, Message, Theme, iced::Renderer>,
        drawer: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self {
            drawer_width: drawer_width.clamp(MIN_SFTP_DRAWER_WIDTH, MAX_SFTP_DRAWER_WIDTH),
            children: [main, drawer],
        })
    }

    fn effective_drawer_width(&self, total_width: f32) -> f32 {
        let max_allowed = (total_width - DIVIDER_THICKNESS - 120.0).max(120.0);
        let min_allowed = MIN_SFTP_DRAWER_WIDTH.min(max_allowed);
        self.drawer_width.clamp(min_allowed, max_allowed)
    }

    fn layout_rects(&self, bounds: Rectangle) -> (Rectangle, Rectangle, Rectangle, Rectangle) {
        let drawer_w = self.effective_drawer_width(bounds.width);
        let main_w = (bounds.width - drawer_w - DIVIDER_THICKNESS).max(1.0);
        let visual = Rectangle::new(
            Point::new(bounds.x + main_w, bounds.y),
            Size::new(DIVIDER_THICKNESS, bounds.height),
        );
        let hit = Rectangle::new(
            Point::new(visual.x - DIVIDER_HIT_PADDING, visual.y),
            Size::new(visual.width + DIVIDER_HIT_PADDING * 2.0, visual.height),
        );
        let drawer_rect = Rectangle::new(
            Point::new(bounds.x + main_w + DIVIDER_THICKNESS, bounds.y),
            Size::new(drawer_w, bounds.height),
        );
        let main_rect = Rectangle::new(bounds.position(), Size::new(main_w, bounds.height));
        (main_rect, visual, hit, drawer_rect)
    }
}

impl Widget<Message, Theme, iced::Renderer> for SftpDrawerDock<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<DrawerDragState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(DrawerDragState::default())
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let total = limits.max();
        let drawer_w = self.effective_drawer_width(total.width);
        let main_w = (total.width - drawer_w - DIVIDER_THICKNESS).max(1.0);
        let main_limits = layout::Limits::new(Size::ZERO, Size::new(main_w, total.height.max(1.0)));
        let drawer_limits =
            layout::Limits::new(Size::ZERO, Size::new(drawer_w, total.height.max(1.0)));

        let [main, drawer] = &mut self.children;
        let [main_tree, drawer_tree] = &mut tree.children[..2] else {
            unreachable!("SftpDrawerDock always has two child trees");
        };

        let main_node = main
            .as_widget_mut()
            .layout(main_tree, renderer, &main_limits);
        let drawer_node = drawer
            .as_widget_mut()
            .layout(drawer_tree, renderer, &drawer_limits)
            .move_to(Point::new(main_w + DIVIDER_THICKNESS, 0.0));

        layout::Node::with_children(total, vec![main_node, drawer_node])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |op| {
            for ((child, child_tree), child_layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child
                    .as_widget_mut()
                    .operate(child_tree, child_layout, renderer, op);
            }
        });
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
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<DrawerDragState>();
        let bounds = layout.bounds();
        let (_main_rect, _visual_rect, hit_rect, drawer_rect) = self.layout_rects(bounds);

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.is_over(hit_rect) =>
            {
                state.dragging = true;
                shell.capture_event();
                shell.request_redraw();
                return;
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                shell.capture_event();
                shell.request_redraw();
                return;
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let is_hovered = cursor.is_over(hit_rect);
                if state.hovered != is_hovered {
                    state.hovered = is_hovered;
                    shell.request_redraw();
                }
                if state.dragging {
                    let right_edge = bounds.x + bounds.width;
                    let new_width = (right_edge - position.x)
                        .clamp(MIN_SFTP_DRAWER_WIDTH, MAX_SFTP_DRAWER_WIDTH);
                    shell.publish(Message::ResizeSftpDrawer(new_width));
                    shell.capture_event();
                    return;
                }
            }
            Event::Window(window::Event::FileDropped(path))
                if cursor.is_over(drawer_rect) =>
            {
                shell.publish(Message::SftpDrawerFileDropped { path: path.clone() });
                shell.capture_event();
                return;
            }
            _ => {}
        }

        let mut layouts = layout.children();
        let (Some(main_layout), Some(drawer_layout)) = (layouts.next(), layouts.next()) else {
            return;
        };
        let [main, drawer] = &mut self.children;
        let [main_tree, drawer_tree] = &mut tree.children[..2] else {
            return;
        };

        drawer.as_widget_mut().update(
            drawer_tree,
            event,
            drawer_layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if shell.is_event_captured() {
            return;
        }
        main.as_widget_mut().update(
            main_tree,
            event,
            main_layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<DrawerDragState>();
        let (_main_rect, _visual_rect, hit_rect, _drawer_rect) =
            self.layout_rects(layout.bounds());
        if state.dragging || cursor.is_over(hit_rect) {
            return mouse::Interaction::ResizingHorizontally;
        }

        for ((child, child_tree), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            let interaction = child.as_widget().mouse_interaction(
                child_tree,
                child_layout,
                cursor,
                viewport,
                renderer,
            );
            if interaction != mouse::Interaction::Idle {
                return interaction;
            }
        }
        mouse::Interaction::Idle
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<DrawerDragState>();
        let (_main_rect, visual_rect, _hit_rect, _drawer_rect) =
            self.layout_rects(layout.bounds());

        for ((child, child_tree), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().draw(
                child_tree,
                renderer,
                theme,
                style,
                child_layout,
                cursor,
                viewport,
            );
        }

        let ext = theme.extended_palette();
        let divider_color = if state.dragging || state.hovered {
            ext.primary.base.color
        } else {
            ext.background.strong.color
        };
        renderer.fill_quad(
            Quad {
                bounds: visual_rect,
                ..Quad::default()
            },
            divider_color,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        let overlays: Vec<_> = self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
            .filter_map(|((child, child_tree), child_layout)| {
                child.as_widget_mut().overlay(
                    child_tree,
                    child_layout,
                    renderer,
                    viewport,
                    translation,
                )
            })
            .collect();
        if overlays.is_empty() {
            None
        } else {
            Some(overlay::Group::with_children(overlays).overlay())
        }
    }
}

/// Renders the SFTP Browser & `Transfer` Queue Drawer content panel.
pub fn render_sftp_drawer<'a>(
    state: &'a SftpDrawerState,
    has_connected_ssh: bool,
    palette: &ThemePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let chrome = ChromePalette::from_palette(palette);

    let header = row![
        text("SFTP & Transfers")
            .size(13)
            .color(chrome.text_primary),
        space::horizontal(),
        button(text("Up").size(11))
            .padding([3, 8])
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::SftpNavigateUp),
        button(text("Refresh").size(11))
            .padding([3, 8])
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::SftpRefresh),
        button(text("×").size(13))
            .padding([2, 7])
            .style(pill_button_style(chrome, PillTone::Ghost))
            .on_press(Message::SetSftpDrawerOpen(false)),
    ]
    .spacing(4)
    .align_y(Alignment::Center);

    let mut content = column![header].spacing(8);

    if let Some(notice) = state.transport_notice() {
        let (dot_color, soft_bg, copy) = match notice.mode {
            SftpTransportMode::IsolatedTransferTransport => (
                chrome.success,
                chrome.success_soft,
                "Isolated Transfer Transport active".to_string(),
            ),
            SftpTransportMode::MuxFallback => (
                chrome.warning,
                chrome.warning_soft,
                notice
                    .message
                    .clone()
                    .unwrap_or_else(|| "Mux fallback active on Primary Transport".to_string()),
            ),
        };
        content = content.push(
            container(
                row![status_dot(dot_color), text(copy).size(11).color(dot_color)]
                    .spacing(6)
                    .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding([6, 8])
            .style(move |_| container::Style {
                background: Some(Background::Color(soft_bg)),
                border: Border {
                    color: dot_color,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            }),
        );
    }

    let path_row = row![
        text_input("Remote path...", state.path_input())
            .on_input(Message::SftpPathInputChanged)
            .on_submit(Message::SftpNavigateTo(state.path_input().to_string()))
            .size(12)
            .padding([5, 8])
            .style(styled_input_style(chrome))
            .width(Length::Fill),
        button(text("Go").size(11))
            .padding([5, 10])
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::SftpNavigateTo(state.path_input().to_string())),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    content = content.push(path_row);

    let mkdir_row = row![
        text_input("New folder...", state.new_folder_name())
            .on_input(Message::SftpNewFolderInputChanged)
            .on_submit(Message::SftpCreateFolder)
            .size(12)
            .padding([5, 8])
            .style(styled_input_style(chrome))
            .width(Length::Fill),
        button(text("+ Dir").size(11))
            .padding([5, 9])
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::SftpCreateFolder),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    content = content.push(mkdir_row);

    if let Some(notice) = state.last_transfer_notice() {
        content = content.push(
            container(text(notice.to_string()).size(11).color(chrome.warning))
                .width(Length::Fill)
                .padding([5, 8])
                .style(move |_| container::Style {
                    background: Some(Background::Color(chrome.warning_soft)),
                    border: Border {
                        color: chrome.warning,
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    ..Default::default()
                }),
        );
    }

    if let Some(err) = state.browser_error() {
        content = content.push(text(err.to_string()).size(11).color(chrome.danger));
    }

    let browser_list = render_browser_entries(state, has_connected_ssh, chrome);
    content = content.push(
        scrollable(browser_list)
            .width(Length::Fill)
            .height(Length::FillPortion(3)),
    );

    content = content.push(rule::horizontal(1));
    content = content.push(render_transfer_queue_section(state, chrome));

    let body = container(content.padding(10))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(chrome.chrome_bg)),
            text_color: Some(chrome.text_primary),
            ..Default::default()
        });

    let left_hairline = container(space::vertical().width(1))
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.border_subtle)),
            ..Default::default()
        });

    row![left_hairline, body]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn render_browser_entries<'a>(
    state: &'a SftpDrawerState,
    has_connected_ssh: bool,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let mut col = column![].spacing(5);

    if state.is_loading() {
        col = col.push(
            text("Loading remote directory...")
                .size(12)
                .color(chrome.text_muted),
        );
    } else if !has_connected_ssh && state.entries().is_empty() {
        col = col.push(
            text("Connect an SSH Session to browse remote files. Drop files here to upload.")
                .size(12)
                .color(chrome.text_muted),
        );
        return col.into();
    } else if state.entries().is_empty() {
        col = col.push(
            text("Directory is empty (drop files here to upload)")
                .size(12)
                .color(chrome.text_muted),
        );
        return col.into();
    }

    for entry in state.entries() {
        let (kind_badge, badge_fg, badge_bg) = match entry.kind {
            RemoteEntryKind::Dir => ("DIR", chrome.accent, chrome.accent_soft),
            RemoteEntryKind::Symlink => ("LNK", chrome.warning, chrome.warning_soft),
            RemoteEntryKind::File | RemoteEntryKind::Other => {
                ("FILE", chrome.text_secondary, chrome.card_hover)
            }
        };
        let meta_label = if entry.is_dir() {
            "Directory".to_string()
        } else {
            format_bytes(entry.size)
        };

        let name_col = column![
            row![
                badge_pill(kind_badge, badge_fg, badge_bg, Color::TRANSPARENT),
                text(entry.name.clone())
                    .size(12)
                    .color(chrome.text_primary),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            text(meta_label).size(10).color(chrome.text_muted),
        ]
        .spacing(2)
        .width(Length::Fill);

        let leading: Element<'a, Message, Theme, iced::Renderer> = if entry.is_dir() {
            button(name_col)
                .padding([3, 4])
                .width(Length::Fill)
                .style(pill_button_style(chrome, PillTone::Ghost))
                .on_press(Message::SftpNavigateTo(entry.path.clone()))
                .into()
        } else {
            container(name_col).padding([3, 4]).width(Length::Fill).into()
        };

        let mut row_el = row![leading].spacing(4).align_y(Alignment::Center);

        if entry.is_dir() {
            row_el = row_el
                .push(
                    button(text("Open").size(10))
                        .padding([3, 7])
                        .style(pill_button_style(chrome, PillTone::Subtle))
                        .on_press(Message::SftpNavigateTo(entry.path.clone())),
                )
                .push(
                    button(text("Get Dir").size(10))
                        .padding([3, 7])
                        .style(pill_button_style(chrome, PillTone::Primary))
                        .on_press(Message::SftpDownloadDir {
                            remote_path: entry.path.clone(),
                            dir_name: entry.name.clone(),
                        }),
                );
        } else {
            row_el = row_el.push(
                button(text("Get").size(10))
                    .padding([3, 8])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SftpDownloadFile {
                        remote_path: entry.path.clone(),
                        file_name: entry.name.clone(),
                    }),
            );
        }

        row_el = row_el.push(
            button(text("Del").size(10))
                .padding([3, 7])
                .style(pill_button_style(chrome, PillTone::Danger))
                .on_press(Message::SftpDeleteEntry {
                    path: entry.path.clone(),
                    is_dir: entry.is_dir(),
                }),
        );

        col = col.push(
            container(row_el)
                .padding([4, 6])
                .width(Length::Fill)
                .style(move |_| card_container_style(chrome, false)),
        );
    }

    col.into()
}

fn render_transfer_queue_section<'a>(
    state: &'a SftpDrawerState,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let snap = state.queue_snapshot();
    let header = row![
        text(format!(
            "Transfers ({} active, {} queued)",
            snap.active_count, snap.queued_count
        ))
        .size(12)
        .color(chrome.text_primary),
        space::horizontal(),
        button(text("Clear").size(10))
            .padding([3, 8])
            .style(pill_button_style(chrome, PillTone::Ghost))
            .on_press(Message::ClearFinishedTransfers),
    ]
    .align_y(Alignment::Center);

    let mut list = column![].spacing(6);
    if snap.transfers.is_empty() {
        list = list.push(
            text("No transfers queued. Drop files onto drawer or Shift+Drop on SSH pane.")
                .size(11)
                .color(chrome.text_muted),
        );
    } else {
        for item in &snap.transfers {
            let dir = item.direction.label();
            let rate = if item.throughput_bytes_per_sec > 0 {
                format!(" · {}/s", format_bytes(item.throughput_bytes_per_sec))
            } else {
                String::new()
            };
            let mode = item
                .transport_mode
                .map(|m| format!(" · {}", m.label()))
                .unwrap_or_default();
            let subtitle = format!(
                "{dir} · {} · {} / {}{rate}{mode}",
                item.status.label(),
                format_bytes(item.bytes_transferred),
                format_bytes(item.total_bytes),
            );

            let mut controls = row![].spacing(4).align_y(Alignment::Center);
            match &item.status {
                TransferStatus::Queued | TransferStatus::InProgress => {
                    controls = controls
                        .push(
                            button(text("Pause").size(10))
                                .padding([2, 7])
                                .style(pill_button_style(chrome, PillTone::Subtle))
                                .on_press(Message::PauseTransfer(item.id)),
                        )
                        .push(
                            button(text("Cancel").size(10))
                                .padding([2, 7])
                                .style(pill_button_style(chrome, PillTone::Danger))
                                .on_press(Message::CancelTransfer(item.id)),
                        );
                }
                TransferStatus::Paused => {
                    controls = controls
                        .push(
                            button(text("Resume").size(10))
                                .padding([2, 7])
                                .style(pill_button_style(chrome, PillTone::Primary))
                                .on_press(Message::ResumeTransfer(item.id)),
                        )
                        .push(
                            button(text("Cancel").size(10))
                                .padding([2, 7])
                                .style(pill_button_style(chrome, PillTone::Danger))
                                .on_press(Message::CancelTransfer(item.id)),
                        );
                }
                TransferStatus::Failed { .. } | TransferStatus::Cancelled => {
                    controls = controls.push(
                        button(text("Retry").size(10))
                            .padding([2, 7])
                            .style(pill_button_style(chrome, PillTone::Primary))
                            .on_press(Message::RetryTransfer(item.id)),
                    );
                }
                TransferStatus::AwaitingConflict => {
                    controls = controls
                        .push(
                            button(text("Overwrite").size(10))
                                .padding([2, 7])
                                .style(pill_button_style(chrome, PillTone::Primary))
                                .on_press(Message::ResolveTransferConflict {
                                    transfer_id: item.id,
                                    resolution: ConflictResolution::Overwrite,
                                    apply_to_all: false,
                                }),
                        )
                        .push(
                            button(text("Skip").size(10))
                                .padding([2, 7])
                                .style(pill_button_style(chrome, PillTone::Subtle))
                                .on_press(Message::ResolveTransferConflict {
                                    transfer_id: item.id,
                                    resolution: ConflictResolution::Skip,
                                    apply_to_all: false,
                                }),
                        );
                }
                TransferStatus::Completed | TransferStatus::Skipped => {}
            }

            let mut item_col = column![
                row![
                    text(item.file_name.clone())
                        .size(12)
                        .color(chrome.text_primary),
                    space::horizontal(),
                    controls,
                ]
                .align_y(Alignment::Center),
                progress_bar(0.0..=1.0, item.progress_fraction()),
                text(subtitle).size(10).color(chrome.text_muted),
            ]
            .spacing(4);

            if let Some(notice) = item
                .notice
                .as_ref()
                .map(TransferNotice::summary)
                .or_else(|| item.status_notice.clone())
            {
                item_col = item_col.push(text(notice).size(10).color(chrome.warning));
            }

            if let TransferStatus::Failed { error } = &item.status {
                item_col = item_col.push(text(error.clone()).size(10).color(chrome.danger));
            }

            list = list.push(
                container(item_col)
                    .padding([6, 8])
                    .width(Length::Fill)
                    .style(move |_| card_container_style(chrome, false)),
            );
        }
    }

    column![
        header,
        scrollable(list)
            .width(Length::Fill)
            .height(Length::FillPortion(2)),
    ]
    .spacing(6)
    .height(Length::FillPortion(2))
    .into()
}

/// Formats a byte count or throughput cleanly (`B`, `KiB`, `MiB`, `GiB`).
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Renders a modal prompt when a `Transfer` pauses in [`TransferStatus::AwaitingConflict`].
pub fn render_conflict_resolution_modal<'a>(
    state: &'a SftpDrawerState,
    palette: &ThemePalette,
) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
    let conflict = state.active_conflict()?;
    let chrome = ChromePalette::from_palette(palette);
    let transfer_id = conflict.transfer_id;
    let apply_to_all = state.conflict_apply_to_all();
    let rename_value = state.conflict_rename_input();
    let trimmed_rename = rename_value.trim().to_string();

    let heading = row![
        status_dot(chrome.warning),
        text(format!("File conflict: {}", conflict.file_name))
            .size(15)
            .color(chrome.text_primary),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut details = format!(
        "Target already exists ({} vs source {}).",
        format_bytes(conflict.existing_target_size),
        format_bytes(conflict.source_size),
    );
    if let Some(part_len) = conflict.part_file_size {
        details.push_str(&format!(
            " Staged .termewnal.part: {}.",
            format_bytes(part_len)
        ));
    }

    let detail_box = container(text(details).size(12).color(chrome.text_secondary))
        .width(Length::Fill)
        .padding([8, 10])
        .style(move |_theme| card_container_style(chrome, false));

    let rename_btn = button(text("Rename").size(12))
        .padding([5, 10])
        .style(pill_button_style(chrome, PillTone::Subtle))
        .on_press_maybe((!trimmed_rename.is_empty()).then(|| {
            Message::ResolveTransferConflict {
                transfer_id,
                resolution: ConflictResolution::Rename {
                    new_name: trimmed_rename,
                },
                apply_to_all,
            }
        }));

    let rename_row = row![
        text_input("New filename...", rename_value)
            .on_input(Message::TransferConflictRenameInputChanged)
            .size(12)
            .padding([6, 8])
            .style(styled_input_style(chrome))
            .width(Length::Fill),
        rename_btn,
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let apply_toggle = checkbox(apply_to_all)
        .label("Apply to all conflicts in this batch")
        .on_toggle(Message::TransferConflictApplyToAllToggled)
        .size(15);

    let actions = row![
        button(text("Skip").size(12))
            .padding([5, 12])
            .style(pill_button_style(chrome, PillTone::Ghost))
            .on_press(Message::ResolveTransferConflict {
                transfer_id,
                resolution: ConflictResolution::Skip,
                apply_to_all,
            }),
        button(text("Resume").size(12))
            .padding([5, 12])
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::ResolveTransferConflict {
                transfer_id,
                resolution: ConflictResolution::Resume,
                apply_to_all,
            }),
        button(text("Overwrite").size(12))
            .padding([5, 14])
            .style(pill_button_style(chrome, PillTone::Primary))
            .on_press(Message::ResolveTransferConflict {
                transfer_id,
                resolution: ConflictResolution::Overwrite,
                apply_to_all,
            }),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let dest_label = match conflict.direction {
        TransferDirection::Upload => conflict.remote_path.clone(),
        TransferDirection::Download => conflict.local_path.display().to_string(),
    };
    let card = container(
        column![
            heading,
            text(format!("Destination: {dest_label}"))
                .size(11)
                .color(chrome.text_muted),
            detail_box,
            rename_row,
            apply_toggle,
            rule::horizontal(1),
            row![space::horizontal(), actions],
        ]
        .spacing(10),
    )
    .width(Length::Fixed(480.0))
    .padding(16)
    .style(move |_theme| modal_card_style(chrome, true));

    Some(modal_overlay(card.into(), None, 24.0))
}
