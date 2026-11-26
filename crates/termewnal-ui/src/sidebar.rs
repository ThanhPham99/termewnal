use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::Widget;
use iced::widget::{Id, button, column, container, row, scrollable, space, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Rectangle, Shadow, Size, Theme, mouse};
use termewnal_core::{OpenSshHost, Profile, ProfileTarget, ThemePalette, TunnelDirection};

use crate::app::{LaunchDisposition, Message, SessionId};
use crate::context_menu::{SidebarRightClickCapture, TunnelRightClickCapture};
use crate::drag_drop::{DragDropState, DragPayload, DragSource, DropTarget, DropTargetZone, DropZoneKind};
use crate::ui_theme::{
    ChromePalette, FONT_BODY, FONT_META, badge_pill, hairline_h, hairline_v,
    list_row_button_style, mono_font, settings_nav_pill_style, sidebar_width_for_window,
    status_dot, styled_input_style, tab_bar_button_style, tabby_scrollable_style, ui_font,
    ui_font_medium, ui_font_semibold,
};
use crate::workspace::TabId;

/// Group heading used when a saved `Profile` has no explicit group assigned.
pub const UNGROUPED_LABEL: &str = "Ungrouped";

/// Returns the static widget [`Id`] for the Left Sidebar profile filter input.
pub const fn sidebar_filter_input_id() -> Id {
    Id::new("sidebar-filter-input")
}

/// Top-level collapsible sections inside the Left Sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SidebarSection {
    ActiveSessions,
    SavedProfiles,
    SystemSshProfiles,
    ActiveTunnels,
}

/// Filter scope for the saved profiles tree (`All` profiles or only `Active` sessions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarFilterScope {
    #[default]
    All,
    Active,
}

/// Running session summary displayed in the sidebar `ACTIVE SESSIONS` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarSessionEntry {
    pub session_id: SessionId,
    pub tab_id: TabId,
    pub profile_id: Option<String>,
    pub title: String,
    pub connected: bool,
    pub connecting: bool,
    pub auto_reconnecting: bool,
    pub reconnected: bool,
    pub active_tab: bool,
}

/// A named group of saved `Profile`s prepared for sidebar rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileGroupView<'a> {
    pub name: String,
    pub collapsed: bool,
    pub profiles: Vec<&'a Profile>,
}

/// Read-only OpenSSH host entry parsed from `~/.ssh/config` (populated by Ticket 08).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemSshProfile {
    pub alias: String,
    pub host_name: Option<String>,
    pub port: u16,
    pub user: Option<String>,
    pub identity_file: Option<String>,
    pub proxy_jump: Vec<String>,
    pub forward_agent: Option<bool>,
    pub forward_x11: Option<bool>,
    pub proxy_command: Option<String>,
}

impl SystemSshProfile {
    /// Returns a concise display target string (`user@host:port`).
    pub fn display_target(&self) -> String {
        let host = self.host_name.as_deref().unwrap_or(&self.alias);
        match self.user.as_deref().filter(|u| !u.is_empty()) {
            Some(user) => format!("{user}@{host}:{}", self.port),
            None => format!("{host}:{}", self.port),
        }
    }

    /// Converts this sidebar System SSH entry into an [`OpenSshHost`] for cloning to `profiles.toml`.
    pub fn to_openssh_host(&self) -> OpenSshHost {
        OpenSshHost {
            alias: self.alias.clone(),
            host_name: self.host_name.clone(),
            port: self.port,
            user: self.user.clone(),
            identity_files: self
                .identity_file
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| vec![PathBuf::from(p)])
                .unwrap_or_default(),
            proxy_jump: self.proxy_jump.clone(),
            forward_agent: self.forward_agent,
            forward_x11: self.forward_x11,
            proxy_command: self.proxy_command.clone(),
        }
    }
}

impl From<&OpenSshHost> for SystemSshProfile {
    fn from(host: &OpenSshHost) -> Self {
        Self {
            alias: host.alias.clone(),
            host_name: host.host_name.clone(),
            port: host.port,
            user: host.user.clone(),
            identity_file: host
                .primary_identity_file()
                .map(|p| p.to_string_lossy().into_owned()),
            proxy_jump: host.proxy_jump.clone(),
            forward_agent: host.forward_agent,
            forward_x11: host.forward_x11,
            proxy_command: host.proxy_command.clone(),
        }
    }
}

/// Lifecycle state of an active `Tunnel` shown in the Left Sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelRuntimeState {
    Stopped,
    Starting,
    Active,
    Holding,
    Reconnecting,
    Retrying {
        attempt: u32,
        next_retry_secs: u64,
        reason: String,
    },
    Error(String),
}

impl TunnelRuntimeState {
    /// Returns a concise human-readable label for this `Tunnel` state.
    pub fn label(&self) -> &str {
        match self {
            Self::Stopped => "Stopped",
            Self::Starting => "Starting",
            Self::Active => "Active",
            Self::Holding => "Holding",
            Self::Reconnecting => "Auto-reconnecting",
            Self::Retrying { .. } => "Auto-reconnecting",
            Self::Error(_) => "Error",
        }
    }

    /// Returns a concise status badge string including attempt & countdown when retrying.
    pub fn status_badge(&self) -> String {
        match self {
            Self::Retrying {
                attempt,
                next_retry_secs,
                ..
            } => format!("Auto-reconnecting #{attempt} ({next_retry_secs}s)"),
            _ => self.label().to_string(),
        }
    }
}

/// Managed `Tunnel` summary entry displayed in the Left Sidebar (populated by Tickets 11-12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTunnelEntry {
    pub id: String,
    pub name: String,
    pub direction: TunnelDirection,
    pub bind_addr: String,
    pub target_addr: String,
    pub state: TunnelRuntimeState,
    pub standalone: bool,
    pub session_id: Option<SessionId>,
    pub reconnect_attempt: Option<u32>,
    pub next_retry_secs: Option<u64>,
    pub last_error: Option<String>,
    pub reconnected_notice: Option<String>,
}

/// State backing the always-visible Left Sidebar.
#[derive(Debug, Clone)]
pub struct SidebarState {
    open: bool,
    filter_scope: SidebarFilterScope,
    filter_query: String,
    selected_tag: Option<String>,
    collapsed_groups: BTreeSet<String>,
    collapsed_sections: BTreeSet<SidebarSection>,
    system_ssh_profiles: Vec<SystemSshProfile>,
    active_tunnels: Vec<ActiveTunnelEntry>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            open: true,
            filter_scope: SidebarFilterScope::All,
            filter_query: String::new(),
            selected_tag: None,
            collapsed_groups: BTreeSet::new(),
            collapsed_sections: BTreeSet::new(),
            system_ssh_profiles: Vec::new(),
            active_tunnels: Vec::new(),
        }
    }
}

impl SidebarState {
    /// Creates a new [`SidebarState`] (always expanded).
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` (the Left Sidebar is always visible).
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Keeps the Left Sidebar open (toggling/collapsing is disabled).
    pub fn set_open(&mut self, _open: bool) {
        self.open = true;
    }

    /// Keeps the Left Sidebar open (toggling/collapsing is disabled).
    pub fn toggle(&mut self) -> bool {
        self.open = true;
        true
    }

    /// Returns the active profile tree filter scope (`All` or `Active`).
    pub fn filter_scope(&self) -> SidebarFilterScope {
        self.filter_scope
    }

    /// Sets the profile tree filter scope (`All` or `Active`).
    pub fn set_filter_scope(&mut self, scope: SidebarFilterScope) {
        self.filter_scope = scope;
    }

    /// Returns the active text filter query inside the Left Sidebar.
    pub fn filter_query(&self) -> &str {
        &self.filter_query
    }

    /// Updates the Left Sidebar profile filter query.
    pub fn set_filter_query(&mut self, query: impl Into<String>) {
        self.filter_query = query.into();
    }

    /// Returns the currently active tag filter, if any.
    pub fn selected_tag(&self) -> Option<&str> {
        self.selected_tag.as_deref()
    }

    /// Toggles `tag` as the active filter (clicking the active tag clears the filter).
    pub fn toggle_tag_filter(&mut self, tag: &str) {
        if self.selected_tag.as_deref() == Some(tag) {
            self.selected_tag = None;
        } else {
            self.selected_tag = Some(tag.to_string());
        }
    }

    /// Explicitly sets or clears the active tag filter.
    pub fn set_selected_tag(&mut self, tag: Option<String>) {
        self.selected_tag = tag;
    }

    /// Returns `true` if `group_name` is currently collapsed.
    pub fn is_group_collapsed(&self, group_name: &str) -> bool {
        self.collapsed_groups.contains(group_name)
    }

    /// Toggles collapsed state for saved `Profile` group `group_name`.
    pub fn toggle_group_collapsed(&mut self, group_name: &str) {
        if !self.collapsed_groups.remove(group_name) {
            self.collapsed_groups.insert(group_name.to_string());
        }
    }

    /// Explicitly expands or collapses saved `Profile` group `group_name`.
    pub fn set_group_collapsed(&mut self, group_name: &str, collapsed: bool) {
        if collapsed {
            self.collapsed_groups.insert(group_name.to_string());
        } else {
            self.collapsed_groups.remove(group_name);
        }
    }

    /// Returns `true` if `section` is collapsed.
    pub fn is_section_collapsed(&self, section: SidebarSection) -> bool {
        self.collapsed_sections.contains(&section)
    }

    /// Toggles collapsed state for a top-level [`SidebarSection`].
    pub fn toggle_section(&mut self, section: SidebarSection) {
        if !self.collapsed_sections.remove(&section) {
            self.collapsed_sections.insert(section);
        }
    }

    /// Explicitly sets collapsed state for a top-level [`SidebarSection`].
    pub fn set_section_collapsed(&mut self, section: SidebarSection, collapsed: bool) {
        if collapsed {
            self.collapsed_sections.insert(section);
        } else {
            self.collapsed_sections.remove(&section);
        }
    }

    /// Returns read-only System SSH `Profile` entries loaded for the sidebar.
    pub fn system_ssh_profiles(&self) -> &[SystemSshProfile] {
        &self.system_ssh_profiles
    }

    /// Replaces read-only System SSH `Profile` entries (Ticket 08 hook).
    pub fn set_system_ssh_profiles(&mut self, entries: Vec<SystemSshProfile>) {
        self.system_ssh_profiles = entries;
    }

    /// Returns active `Tunnel` entries displayed in the sidebar.
    pub fn active_tunnels(&self) -> &[ActiveTunnelEntry] {
        &self.active_tunnels
    }

    /// Replaces active `Tunnel` entries displayed in the sidebar (Tickets 11-12 hook).
    pub fn set_active_tunnels(&mut self, entries: Vec<ActiveTunnelEntry>) {
        self.active_tunnels = entries;
    }

    /// Returns all sorted, unique, non-empty tags declared across `profiles`.
    pub fn available_tags(&self, profiles: &[Profile]) -> Vec<String> {
        let mut tags = BTreeSet::new();
        for profile in profiles {
            for tag in &profile.tags {
                let trimmed = tag.trim();
                if !trimmed.is_empty() {
                    tags.insert(trimmed.to_string());
                }
            }
        }
        tags.into_iter().collect()
    }

    /// Groups and filters saved `Profile`s by group name and active tag/text filters.
    ///
    /// Named groups are ordered alphabetically (case-insensitive), followed by [`UNGROUPED_LABEL`].
    pub fn grouped_saved_profiles<'a>(
        &self,
        profiles: &'a [Profile],
    ) -> Vec<ProfileGroupView<'a>> {
        self.grouped_saved_profiles_with_groups(profiles, &[])
    }

    /// Groups saved `Profile`s together with explicit empty `saved_groups` when unfiltered.
    pub fn grouped_saved_profiles_with_groups<'a>(
        &self,
        profiles: &'a [Profile],
        saved_groups: &[String],
    ) -> Vec<ProfileGroupView<'a>> {
        let needle = self.filter_query.trim().to_ascii_lowercase();
        let active_tag = self.selected_tag.as_deref().map(str::to_ascii_lowercase);

        let mut grouped: BTreeMap<(bool, String, String), Vec<&'a Profile>> = BTreeMap::new();
        if needle.is_empty() && active_tag.is_none() {
            for group in saved_groups {
                let trimmed = group.trim();
                if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case(UNGROUPED_LABEL) {
                    let key = (false, trimmed.to_ascii_lowercase(), trimmed.to_string());
                    grouped.entry(key).or_default();
                }
            }
        }

        for profile in profiles {
            if let Some(tag_filter) = active_tag.as_deref()
                && !profile
                    .tags
                    .iter()
                    .any(|t| t.trim().to_ascii_lowercase() == tag_filter)
            {
                continue;
            }
            if !needle.is_empty() && !profile_matches_query(profile, &needle) {
                continue;
            }

            let group_name = normalized_group_name(profile);
            let is_ungrouped = group_name == UNGROUPED_LABEL;
            let key = (is_ungrouped, group_name.to_ascii_lowercase(), group_name);
            grouped.entry(key).or_default().push(profile);
        }

        grouped
            .into_iter()
            .map(|((_, _, name), group_profiles)| ProfileGroupView {
                collapsed: self.is_group_collapsed(&name),
                name,
                profiles: group_profiles,
            })
            .collect()
    }
}

fn normalized_group_name(profile: &Profile) -> String {
    profile
        .group
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .unwrap_or(UNGROUPED_LABEL)
        .to_string()
}

fn profile_matches_query(profile: &Profile, needle_lower: &str) -> bool {
    if profile.name.to_ascii_lowercase().contains(needle_lower) {
        return true;
    }
    if profile
        .group
        .as_deref()
        .is_some_and(|g| g.to_ascii_lowercase().contains(needle_lower))
    {
        return true;
    }
    if profile
        .tags
        .iter()
        .any(|t| t.to_ascii_lowercase().contains(needle_lower))
    {
        return true;
    }
    match &profile.target {
        ProfileTarget::LocalShell {
            shell, working_dir, ..
        } => {
            shell
                .as_deref()
                .is_some_and(|s| s.to_ascii_lowercase().contains(needle_lower))
                || working_dir
                    .as_ref()
                    .is_some_and(|p| p.to_string_lossy().to_ascii_lowercase().contains(needle_lower))
        }
        ProfileTarget::Ssh { host, username, .. } => {
            host.to_ascii_lowercase().contains(needle_lower)
                || username.to_ascii_lowercase().contains(needle_lower)
        }
        ProfileTarget::Serial { port, .. } => port.to_ascii_lowercase().contains(needle_lower),
        ProfileTarget::Telnet { host, .. } => host.to_ascii_lowercase().contains(needle_lower),
        ProfileTarget::Docker { container, .. } => {
            container.to_ascii_lowercase().contains(needle_lower)
        }
    }
}

/// Formats a concise one-line target summary for a saved `Profile`.
pub fn profile_target_summary(profile: &Profile) -> String {
    match &profile.target {
        ProfileTarget::LocalShell { shell, .. } => shell
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Default local shell")
            .to_string(),
        ProfileTarget::Ssh {
            host,
            port,
            username,
            ..
        } => format!("{username}@{host}:{port}"),
        ProfileTarget::Serial {
            port, baud_rate, ..
        } => format!("{port} @ {baud_rate}"),
        ProfileTarget::Telnet { host, port, .. } => format!("telnet://{host}:{port}"),
        ProfileTarget::Docker {
            runtime, container, ..
        } => {
            let prefix = match runtime {
                termewnal_core::DockerRuntime::Docker => "docker",
                termewnal_core::DockerRuntime::Podman => "podman",
            };
            format!("{prefix}:{container}")
        }
    }
}

/// Renders the Left Sidebar displaying Saved `Profile`s, System SSH hosts, and Active `Tunnel`s.
pub fn render_sidebar<'a>(
    state: &'a SidebarState,
    profiles: &'a [Profile],
    palette: &ThemePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    render_sidebar_responsive(state, profiles, palette, 1280.0)
}

/// Renders the Left Sidebar with width and controls adapted to `window_width`.
pub fn render_sidebar_responsive<'a>(
    state: &'a SidebarState,
    profiles: &'a [Profile],
    palette: &ThemePalette,
    window_width: f32,
) -> Element<'a, Message, Theme, iced::Renderer> {
    render_sidebar_responsive_with_dnd(
        state,
        profiles,
        &[],
        None,
        true,
        palette,
        window_width,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarGlyph {
    Folder,
    Terminal,
}

struct SidebarIcon {
    glyph: SidebarGlyph,
    color: Color,
}

impl SidebarIcon {
    fn new<'a>(glyph: SidebarGlyph, color: Color) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(Self { glyph, color })
    }
}

impl Widget<Message, Theme, iced::Renderer> for SidebarIcon {
    fn children(&self) -> Vec<Tree> {
        Vec::new()
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(13.0), Length::Fixed(12.0))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(Length::Fixed(13.0), Length::Fixed(12.0), Size::ZERO);
        layout::Node::new(size)
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        if !bounds.intersects(viewport) {
            return;
        }
        let x0 = bounds.x.round() + 1.0;
        let y0 = bounds.y.round() + 1.0;
        let c = self.color;

        match self.glyph {
            SidebarGlyph::Folder => {
                sb_fill(renderer, Rectangle::new([x0, y0 + 1.0].into(), [5.0, 2.0].into()), c);
                sb_stroke(renderer, Rectangle::new([x0, y0 + 2.5].into(), [11.0, 7.5].into()), c);
            }
            SidebarGlyph::Terminal => {
                sb_stroke(renderer, Rectangle::new([x0, y0 + 1.0].into(), [11.0, 8.5].into()), c);
                sb_fill(renderer, Rectangle::new([x0 + 2.5, y0 + 3.5].into(), [2.0, 1.2].into()), c);
                sb_fill(renderer, Rectangle::new([x0 + 3.5, y0 + 4.5].into(), [1.5, 1.2].into()), c);
                sb_fill(renderer, Rectangle::new([x0 + 2.5, y0 + 5.5].into(), [2.0, 1.2].into()), c);
                sb_fill(renderer, Rectangle::new([x0 + 5.5, y0 + 6.0].into(), [3.0, 1.2].into()), c);
            }
        }
    }
}

fn sb_fill(renderer: &mut iced::Renderer, bounds: Rectangle, color: Color) {
    use iced::advanced::renderer::Renderer as _;
    renderer.fill_quad(
        Quad {
            bounds,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(color),
    );
}

fn sb_stroke(renderer: &mut iced::Renderer, bounds: Rectangle, color: Color) {
    use iced::advanced::renderer::Renderer as _;
    renderer.fill_quad(
        Quad {
            bounds,
            border: Border {
                color,
                width: 1.0,
                radius: 1.5.into(),
            },
            shadow: Shadow::default(),
            snap: true,
        },
        Background::Color(Color::TRANSPARENT),
    );
}

/// Renders the Left Sidebar with explicit custom groups, right-click targets, and Drag & Drop.
pub fn render_sidebar_responsive_with_dnd<'a>(
    state: &'a SidebarState,
    profiles: &'a [Profile],
    saved_groups: &'a [String],
    drag_drop: Option<&'a DragDropState>,
    drag_enabled: bool,
    palette: &ThemePalette,
    window_width: f32,
) -> Element<'a, Message, Theme, iced::Renderer> {
    render_sidebar_full(
        state,
        profiles,
        saved_groups,
        &[],
        drag_drop,
        drag_enabled,
        palette,
        window_width,
    )
}

/// Renders the full Tabby-inspired Left Sidebar (Active Sessions, `[Active][All][+]`,
/// clean folder tree, SSH config, Tunnels, and compact footer).
#[allow(clippy::too_many_arguments)]
pub fn render_sidebar_full<'a>(
    state: &'a SidebarState,
    profiles: &'a [Profile],
    saved_groups: &'a [String],
    sessions: &[SidebarSessionEntry],
    drag_drop: Option<&'a DragDropState>,
    drag_enabled: bool,
    palette: &ThemePalette,
    window_width: f32,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let chrome = ChromePalette::from_palette(palette);
    let sidebar_w = sidebar_width_for_window(window_width);

    let mut content = column![]
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill);

    if !sessions.is_empty() {
        content = content.push(render_active_sessions_section(state, sessions, chrome));
        content = content.push(hairline_h(chrome));
    }

    let scope = state.filter_scope();
    let filter_scope_row = row![
        button(
            text("Active")
                .size(FONT_META)
                .font(if scope == SidebarFilterScope::Active {
                    ui_font_medium()
                } else {
                    ui_font()
                }),
        )
        .padding([3, 9])
        .style(settings_nav_pill_style(
            chrome,
            scope == SidebarFilterScope::Active,
        ))
        .on_press(Message::SetSidebarFilterScope(SidebarFilterScope::Active)),
        button(
            text("All")
                .size(FONT_META)
                .font(if scope == SidebarFilterScope::All {
                    ui_font_medium()
                } else {
                    ui_font()
                }),
        )
        .padding([3, 10])
        .style(settings_nav_pill_style(
            chrome,
            scope == SidebarFilterScope::All,
        ))
        .on_press(Message::SetSidebarFilterScope(SidebarFilterScope::All)),
        button(text("+").size(13.0).font(ui_font_semibold()))
            .padding([2, 8])
            .style(tab_bar_button_style(chrome, false, false))
            .on_press(Message::OpenNewProfileModal),
        space::horizontal(),
        button(text("+ Group").size(FONT_META).color(chrome.text_muted))
            .padding([3, 6])
            .style(tab_bar_button_style(chrome, false, false))
            .on_press(Message::BeginCreateGroup {
                assign_profile_id: None,
            }),
    ]
    .spacing(4)
    .padding(Padding {
        top: 6.0,
        right: 8.0,
        bottom: 4.0,
        left: 8.0,
    })
    .align_y(Alignment::Center);

    let filter_input = container(
        text_input("Filter profiles...", state.filter_query())
            .id(sidebar_filter_input_id())
            .on_input(Message::SidebarFilterChanged)
            .size(FONT_BODY)
            .padding([5, 8])
            .width(Length::Fill)
            .style(styled_input_style(chrome)),
    )
    .padding([2, 8]);

    content = content.push(filter_scope_row).push(filter_input);

    let tags = state.available_tags(profiles);
    if !tags.is_empty() {
        let mut tag_row = row![].spacing(4).align_y(Alignment::Center);
        for tag in tags {
            let active = state.selected_tag() == Some(tag.as_str());
            tag_row = tag_row.push(
                button(
                    text(format!("#{tag}"))
                        .size(FONT_META)
                        .font(if active { ui_font_medium() } else { ui_font() }),
                )
                .padding([2, 7])
                .style(tab_bar_button_style(chrome, active, false))
                .on_press(Message::SidebarToggleTagFilter(tag)),
            );
        }
        let tags_scroll = container(
            scrollable(tag_row).direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(2).scroller_width(2),
            )),
        )
        .padding([4, 8]);
        content = content.push(tags_scroll);
    }

    let mut sections = column![].spacing(4).width(Length::Fill);
    sections = sections.push(render_saved_profiles_section(
        state,
        profiles,
        saved_groups,
        sessions,
        drag_drop,
        drag_enabled,
        chrome,
    ));
    sections = sections.push(hairline_h(chrome));
    sections = sections.push(render_system_ssh_section(
        state,
        drag_drop,
        drag_enabled,
        chrome,
    ));
    sections = sections.push(hairline_h(chrome));
    sections = sections.push(render_tunnels_section(state, chrome));

    content = content.push(
        scrollable(sections.padding(Padding {
            top: 4.0,
            right: 2.0,
            bottom: 8.0,
            left: 2.0,
        }))
        .style(tabby_scrollable_style(chrome))
        .width(Length::Fill)
        .height(Length::Fill),
    );

    let footer = container(
        row![
            button(text("SSH").size(10.5).color(chrome.text_muted))
                .padding([2, 5])
                .style(tab_bar_button_style(chrome, false, false))
                .on_press(Message::OpenQuickConnectPalette),
            button(text("Import").size(10.5).color(chrome.text_muted))
                .padding([2, 5])
                .style(tab_bar_button_style(chrome, false, false))
                .on_press(Message::OpenTabbyImportModal),
            button(text("Cmds").size(10.5).color(chrome.text_muted))
                .padding([2, 5])
                .style(tab_bar_button_style(chrome, false, false))
                .on_press(Message::OpenQuickCommandsModal),
            button(text("Spaces").size(10.5).color(chrome.text_muted))
                .padding([2, 5])
                .style(tab_bar_button_style(chrome, false, false))
                .on_press(Message::OpenWorkspacesModal),
            space::horizontal(),
            button(text("⚙").size(12.5).color(chrome.text_secondary))
                .padding([2, 6])
                .style(tab_bar_button_style(chrome, false, false))
                .on_press(Message::OpenSettingsModal),
        ]
        .spacing(2)
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding([4, 6])
    .style(move |_| container::Style {
        background: Some(Background::Color(chrome.bg_more)),
        ..Default::default()
    });

    content = content.push(hairline_h(chrome)).push(footer);

    let body = container(content)
        .width(Length::Fixed(sidebar_w))
        .height(Length::Fill)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(chrome.chrome_bg)),
            text_color: Some(chrome.text_primary),
            ..Default::default()
        });

    let body_with_menu = SidebarRightClickCapture::new(None, None, body.into());
    row![body_with_menu, hairline_v(chrome)]
        .height(Length::Fill)
        .into()
}

fn render_active_sessions_section<'a>(
    state: &'a SidebarState,
    sessions: &[SidebarSessionEntry],
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let collapsed = state.is_section_collapsed(SidebarSection::ActiveSessions);
    let prefix = if collapsed { "▸" } else { "▾" };
    let header_btn = button(
        row![
            text(format!("{prefix} ACTIVE SESSIONS"))
                .size(10.5)
                .font(ui_font_semibold())
                .color(chrome.text_muted),
            space::horizontal(),
            container(
                text(format!("{}", sessions.len()))
                    .size(10.0)
                    .font(ui_font_semibold())
                    .color(chrome.text_secondary),
            )
            .padding([1, 6])
            .style(move |_| container::Style {
                background: Some(Background::Color(chrome.secondary)),
                border: Border {
                    radius: 999.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([5, 10])
    .width(Length::Fill)
    .style(list_row_button_style(chrome, false))
    .on_press(Message::SidebarToggleSection(SidebarSection::ActiveSessions));

    let mut col = column![header_btn].spacing(2).width(Length::Fill);
    if collapsed {
        return col.into();
    }

    for entry in sessions {
        let dot_color = if entry.auto_reconnecting || entry.connecting {
            chrome.warning
        } else if entry.connected {
            chrome.success
        } else {
            chrome.text_muted
        };
        let is_active = entry.active_tab;
        let accent_bar = container(space::vertical().width(2).height(16))
            .width(2)
            .height(16)
            .style(move |_| container::Style {
                background: is_active.then_some(Background::Color(chrome.accent)),
                ..Default::default()
            });
        let mut row_items = row![
            accent_bar,
            SidebarIcon::new(
                SidebarGlyph::Terminal,
                if entry.active_tab {
                    chrome.text_primary
                } else {
                    chrome.text_secondary
                },
            ),
            status_dot(dot_color),
            container(
                text(entry.title.clone())
                    .size(FONT_BODY)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .color(if entry.active_tab {
                        chrome.text_primary
                    } else {
                        chrome.text_secondary
                    }),
            )
            .width(Length::Fill)
            .clip(true),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        if entry.auto_reconnecting {
            row_items = row_items.push(badge_pill(
                "Auto-reconnecting",
                chrome.warning,
                chrome.warning_soft,
                Color::TRANSPARENT,
            ));
        } else if entry.reconnected {
            row_items = row_items.push(badge_pill(
                "Reconnected successfully",
                chrome.success,
                chrome.success_soft,
                Color::TRANSPARENT,
            ));
        }

        let row_btn = button(row_items)
            .padding([4, 8])
            .width(Length::Fill)
            .style(list_row_button_style(chrome, entry.active_tab))
            .on_press(Message::SelectTab(entry.tab_id));

        col = col.push(row_btn);
    }

    col.padding(Padding {
        top: 2.0,
        right: 0.0,
        bottom: 6.0,
        left: 0.0,
    })
    .into()
}

fn render_saved_profiles_section<'a>(
    state: &'a SidebarState,
    profiles: &'a [Profile],
    saved_groups: &'a [String],
    sessions: &[SidebarSessionEntry],
    drag_drop: Option<&'a DragDropState>,
    drag_enabled: bool,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let mut col = column![].spacing(2).width(Length::Fill);

    let active_profile_ids: BTreeSet<&str> = sessions
        .iter()
        .filter_map(|s| s.profile_id.as_deref())
        .collect();

    let mut groups = state.grouped_saved_profiles_with_groups(profiles, saved_groups);
    if state.filter_scope() == SidebarFilterScope::Active {
        for group in &mut groups {
            group
                .profiles
                .retain(|p| active_profile_ids.contains(p.id.as_str()));
        }
        groups.retain(|g| !g.profiles.is_empty());
    }

    let has_ungrouped = groups.iter().any(|g| g.name == UNGROUPED_LABEL);
    let dragging_to_group = drag_drop.is_some_and(|dd| {
        matches!(
            dd.active_payload(),
            Some(DragPayload::SavedProfile { .. } | DragPayload::SystemSsh { .. })
        )
    });

    if groups.is_empty() && !dragging_to_group {
        let msg = if state.filter_scope() == SidebarFilterScope::Active {
            "No active saved profiles"
        } else {
            "No saved profiles · Click + or right-click"
        };
        col = col.push(
            container(text(msg).size(FONT_META).color(chrome.text_muted)).padding([8, 12]),
        );
        return col.into();
    }

    let active_dragged_profile_id = drag_drop.and_then(|dd| match dd.active_payload() {
        Some(DragPayload::SavedProfile { profile_id, .. }) => Some(profile_id.as_str()),
        _ => None,
    });
    let hovered_drop_group = drag_drop.and_then(|dd| match dd.hovered_target() {
        Some(DropTarget::ProfileGroup(g)) => Some(g.as_deref()),
        _ => None,
    });

    for group in groups {
        let group_prefix = if group.collapsed { "▸" } else { "▾" };
        let group_target: Option<String> =
            (!group.name.eq_ignore_ascii_case(UNGROUPED_LABEL)).then(|| group.name.clone());
        let is_drop_hovered = hovered_drop_group
            .is_some_and(|target| target == group_target.as_deref());
        let folder_color = if is_drop_hovered {
            chrome.accent
        } else {
            chrome.text_secondary
        };

        let mut group_col = column![
            button(
                row![
                    text(group_prefix)
                        .size(FONT_META)
                        .font(ui_font())
                        .color(chrome.text_muted),
                    SidebarIcon::new(SidebarGlyph::Folder, folder_color),
                    text(group.name.clone())
                        .size(FONT_META)
                        .font(ui_font_medium())
                        .color(folder_color),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding([4, 10])
            .width(Length::Fill)
            .style(list_row_button_style(chrome, false))
            .on_press(Message::SidebarToggleGroup(group.name.clone())),
        ]
        .spacing(1)
        .width(Length::Fill);

        if !group.collapsed {
            let mut rows_col = column![].spacing(1).width(Length::Fill);
            let group_len = group.profiles.len();
            if group_len == 0 {
                rows_col = rows_col.push(
                    container(
                        text("Drop profiles here or right-click")
                            .size(FONT_META)
                            .color(chrome.text_muted),
                    )
                    .padding([4, 18]),
                );
            } else {
                for profile in group.profiles.into_iter() {
                    let matching_session = sessions
                        .iter()
                        .find(|s| s.profile_id.as_deref() == Some(profile.id.as_str()));
                    let is_live = matching_session.is_some();
                    let dot_color = match matching_session {
                        Some(s) if s.connected => chrome.success,
                        Some(s) if s.connecting => chrome.warning,
                        _ => match &profile.target {
                            ProfileTarget::Ssh { .. } | ProfileTarget::Docker { .. } => {
                                chrome.text_muted
                            }
                            ProfileTarget::LocalShell { .. } => chrome.success,
                            ProfileTarget::Serial { .. } | ProfileTarget::Telnet { .. } => {
                                chrome.warning
                            }
                        },
                    };

                    // Single-line compact Tabby better-sidebar row with terminal icon + status dot.
                    let item_row = row![
                        SidebarIcon::new(
                            SidebarGlyph::Terminal,
                            if is_live {
                                chrome.text_primary
                            } else {
                                chrome.text_secondary
                            },
                        ),
                        status_dot(dot_color),
                        container(
                            text(profile.name.as_str())
                                .size(FONT_BODY)
                                .wrapping(iced::widget::text::Wrapping::None)
                                .color(chrome.text_primary),
                        )
                        .width(Length::Fill)
                        .clip(true),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center);

                    let launch_btn = button(item_row)
                        .padding([4, 6])
                        .width(Length::Fill)
                        .style(list_row_button_style(chrome, false))
                        .on_press(Message::LaunchProfile {
                            profile_id: profile.id.clone(),
                            disposition: LaunchDisposition::NewTab,
                        });
                    let draggable_launch = DragSource::new(
                        DragPayload::SavedProfile {
                            profile_id: profile.id.clone(),
                            label: profile.name.clone(),
                        },
                        drag_enabled,
                        active_dragged_profile_id == Some(profile.id.as_str()),
                        launch_btn.into(),
                    );

                    let edit_btn = button(text("⋯").size(FONT_BODY))
                        .padding([2, 6])
                        .style(tab_bar_button_style(chrome, false, false))
                        .on_press(Message::OpenEditProfileModal(profile.id.clone()));

                    let row_el: Element<'a, Message, Theme, iced::Renderer> =
                        row![draggable_launch, edit_btn]
                            .spacing(2)
                            .padding(Padding {
                                top: 0.0,
                                right: 4.0,
                                bottom: 0.0,
                                left: 10.0,
                            })
                            .align_y(Alignment::Center)
                            .into();
                    rows_col = rows_col.push(SidebarRightClickCapture::new(
                        profile.group.clone(),
                        Some(profile.id.clone()),
                        row_el,
                    ));
                }
            }

            group_col = group_col.push(
                container(rows_col)
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: is_drop_hovered.then_some(Background::Color(chrome.accent_soft)),
                        border: iced::Border {
                            color: if is_drop_hovered {
                                chrome.accent
                            } else {
                                Color::TRANSPARENT
                            },
                            width: if is_drop_hovered { 1.0 } else { 0.0 },
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    }),
            );
        }

        let group_drop_wrapped = DropTargetZone::new(
            DropZoneKind::ProfileGroup {
                group: group_target,
                accepting_drag: dragging_to_group,
                is_hovered: is_drop_hovered,
            },
            group_col.into(),
        );
        col = col.push(SidebarRightClickCapture::new(
            Some(group.name),
            None,
            group_drop_wrapped,
        ));
    }

    if dragging_to_group && !has_ungrouped {
        let is_drop_hovered = hovered_drop_group == Some(None);
        let ungrouped_drop = DropTargetZone::new(
            DropZoneKind::ProfileGroup {
                group: None,
                accepting_drag: true,
                is_hovered: is_drop_hovered,
            },
            container(
                text(format!("Drop here for {UNGROUPED_LABEL}"))
                    .size(FONT_META)
                    .color(if is_drop_hovered {
                        chrome.accent
                    } else {
                        chrome.text_muted
                    }),
            )
            .padding([6, 12])
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: is_drop_hovered.then_some(Background::Color(chrome.accent_soft)),
                border: iced::Border {
                    color: if is_drop_hovered {
                        chrome.accent
                    } else {
                        chrome.border_subtle
                    },
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            })
            .into(),
        );
        col = col.push(ungrouped_drop);
    }

    col.into()
}

fn render_system_ssh_section<'a>(
    state: &'a SidebarState,
    drag_drop: Option<&'a DragDropState>,
    drag_enabled: bool,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let collapsed = state.is_section_collapsed(SidebarSection::SystemSshProfiles);
    let prefix = if collapsed { "▸" } else { "▾" };
    let count = state.system_ssh_profiles().len();
    let label = if count > 0 {
        format!("Imported from ~/.ssh/config ({count})")
    } else {
        "Imported from ~/.ssh/config".to_string()
    };
    let header = button(
        row![
            text(prefix)
                .size(FONT_META)
                .font(ui_font())
                .color(chrome.text_muted),
            SidebarIcon::new(SidebarGlyph::Folder, chrome.text_secondary),
            container(
                text(label)
                    .size(FONT_META)
                    .font(ui_font_medium())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .color(chrome.text_secondary),
            )
            .width(Length::Fill)
            .clip(true),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([4, 10])
    .width(Length::Fill)
    .style(list_row_button_style(chrome, false))
    .on_press(Message::SidebarToggleSection(
        SidebarSection::SystemSshProfiles,
    ));

    let mut col = column![header].spacing(2).width(Length::Fill);
    if collapsed {
        return col.into();
    }

    let needle = state.filter_query().trim().to_ascii_lowercase();
    let matching: Vec<&SystemSshProfile> = state
        .system_ssh_profiles()
        .iter()
        .filter(|host| {
            needle.is_empty()
                || host.alias.to_ascii_lowercase().contains(&needle)
                || host
                    .host_name
                    .as_deref()
                    .is_some_and(|h| h.to_ascii_lowercase().contains(&needle))
                || host
                    .user
                    .as_deref()
                    .is_some_and(|u| u.to_ascii_lowercase().contains(&needle))
                || host
                    .proxy_jump
                    .iter()
                    .any(|j| j.to_ascii_lowercase().contains(&needle))
        })
        .collect();

    if matching.is_empty() {
        let empty_msg = if state.system_ssh_profiles().is_empty() {
            "~/.ssh/config hosts appear here"
        } else {
            "No matching SSH config hosts"
        };
        col = col.push(
            container(text(empty_msg).size(FONT_META).color(chrome.text_muted)).padding([4, 12]),
        );
        return col.into();
    }

    let active_dragged_alias = drag_drop.and_then(|dd| match dd.active_payload() {
        Some(DragPayload::SystemSsh { alias, .. }) => Some(alias.as_str()),
        _ => None,
    });

    let mut rows_col = column![].spacing(1).width(Length::Fill);
    for host in matching.into_iter() {
        let subtitle = if host.proxy_jump.is_empty() {
            host.display_target()
        } else {
            format!(
                "{} · via {}",
                host.display_target(),
                host.proxy_jump.join(" → ")
            )
        };
        let item = container(
            column![
                row![
                    status_dot(chrome.text_muted),
                    text(host.alias.as_str())
                        .size(FONT_BODY)
                        .wrapping(iced::widget::text::Wrapping::None)
                        .color(chrome.text_primary),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
                text(subtitle)
                    .size(FONT_META)
                    .font(mono_font())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .color(chrome.text_muted),
            ]
            .spacing(1),
        )
        .width(Length::Fill)
        .clip(true);
        let launch_btn = button(item)
            .padding([4, 8])
            .width(Length::Fill)
            .style(list_row_button_style(chrome, false))
            .on_press(Message::LaunchSystemSsh {
                alias: host.alias.clone(),
                disposition: LaunchDisposition::NewTab,
            });
        let draggable_launch = DragSource::new(
            DragPayload::SystemSsh {
                alias: host.alias.clone(),
                label: host.alias.clone(),
            },
            drag_enabled,
            active_dragged_alias == Some(host.alias.as_str()),
            launch_btn.into(),
        );
        let clone_btn = button(text("Clone").size(FONT_META))
            .padding([2, 6])
            .style(tab_bar_button_style(chrome, false, false))
            .on_press(Message::CloneSystemSshToProfile(host.alias.clone()));
        rows_col = rows_col.push(
            row![draggable_launch, clone_btn]
                .spacing(2)
                .padding(Padding {
                    top: 0.0,
                    right: 4.0,
                    bottom: 0.0,
                    left: 12.0,
                })
                .align_y(Alignment::Center),
        );
    }

    col = col.push(rows_col);
    col.into()
}

fn render_tunnels_section(
    state: &SidebarState,
    chrome: ChromePalette,
) -> Element<'_, Message, Theme, iced::Renderer> {
    let collapsed = state.is_section_collapsed(SidebarSection::ActiveTunnels);
    let prefix = if collapsed { "▸" } else { "▾" };
    let count = state.active_tunnels().len();
    let header_btn = button(
        text(format!("{prefix} Tunnels ({count})"))
            .size(FONT_META)
            .font(ui_font_medium())
            .color(chrome.text_secondary),
    )
    .padding([4, 8])
    .width(Length::Fill)
    .style(list_row_button_style(chrome, false))
    .on_press(Message::SidebarToggleSection(SidebarSection::ActiveTunnels));
    let new_btn = button(text("+ Tunnel").size(FONT_META).color(chrome.text_muted))
        .padding([3, 6])
        .style(tab_bar_button_style(chrome, false, false))
        .on_press(Message::OpenTunnelCreateModal);
    let header = row![header_btn, new_btn]
        .spacing(2)
        .padding([0, 2])
        .width(Length::Fill)
        .align_y(Alignment::Center);

    let mut col = column![header].spacing(2).width(Length::Fill);
    if collapsed {
        return col.into();
    }

    if state.active_tunnels().is_empty() {
        col = col.push(
            container(
                text("No active tunnels")
                    .size(FONT_META)
                    .color(chrome.text_muted),
            )
            .padding([4, 12]),
        );
        return col.into();
    }

    let mut rows_col = column![].spacing(2).width(Length::Fill);
    for tunnel in state.active_tunnels().iter() {
        let dir = match tunnel.direction {
            TunnelDirection::Local => "L",
            TunnelDirection::Remote => "R",
            TunnelDirection::Dynamic => "D",
        };
        let scope = if tunnel.standalone {
            "Standalone"
        } else {
            "Session"
        };
        let is_running = matches!(
            tunnel.state,
            TunnelRuntimeState::Starting
                | TunnelRuntimeState::Active
                | TunnelRuntimeState::Holding
                | TunnelRuntimeState::Reconnecting
                | TunnelRuntimeState::Retrying { .. }
        );
        let (state_fg, state_bg) = match &tunnel.state {
            TunnelRuntimeState::Active => (chrome.success, chrome.success_soft),
            TunnelRuntimeState::Holding
            | TunnelRuntimeState::Reconnecting
            | TunnelRuntimeState::Retrying { .. }
            | TunnelRuntimeState::Starting => (chrome.warning, chrome.warning_soft),
            TunnelRuntimeState::Error(_) => (chrome.danger, chrome.danger_soft),
            TunnelRuntimeState::Stopped => (chrome.text_muted, chrome.card_hover),
        };
        let toggle_btn = button(text(if is_running { "Stop" } else { "Start" }).size(FONT_META))
            .padding([2, 7])
            .style(tab_bar_button_style(chrome, !is_running, false))
            .on_press(Message::ToggleTunnel(tunnel.id.clone()));

        let edit_btn = button(text("⋯").size(FONT_BODY))
            .padding([2, 6])
            .style(tab_bar_button_style(chrome, false, false))
            .on_press(Message::OpenEditTunnelModal(tunnel.id.clone()));

        let badge = match (&tunnel.state, tunnel.reconnect_attempt, tunnel.next_retry_secs) {
            (TunnelRuntimeState::Reconnecting, Some(attempt), Some(secs)) => {
                format!("Auto-reconnecting #{attempt} ({secs}s)")
            }
            (TunnelRuntimeState::Reconnecting, Some(attempt), None) => {
                format!("Auto-reconnecting #{attempt}")
            }
            _ => tunnel.state.status_badge(),
        };

        let title_row = row![
            status_dot(state_fg),
            text(format!("[{dir}]"))
                .size(FONT_META)
                .font(mono_font())
                .color(chrome.accent),
            container(
                text(tunnel.name.as_str())
                    .size(FONT_BODY)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .color(chrome.text_primary),
            )
            .width(Length::Fill)
            .clip(true),
            badge_pill(badge, state_fg, state_bg, Color::TRANSPARENT),
            edit_btn,
        ]
        .spacing(5)
        .align_y(Alignment::Center);

        let mut controls_row = row![
            container(
                text(format!("{} -> {} ({scope})", tunnel.bind_addr, tunnel.target_addr))
                    .size(FONT_META)
                    .font(mono_font())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .color(chrome.text_muted),
            )
            .width(Length::Fill)
            .clip(true),
            toggle_btn,
        ]
        .spacing(4)
        .align_y(Alignment::Center);

        if matches!(
            tunnel.state,
            TunnelRuntimeState::Active
                | TunnelRuntimeState::Holding
                | TunnelRuntimeState::Reconnecting
                | TunnelRuntimeState::Retrying { .. }
                | TunnelRuntimeState::Error(_)
        ) {
            controls_row = controls_row.push(
                button(text("Retry").size(FONT_META))
                    .padding([2, 6])
                    .style(tab_bar_button_style(chrome, false, false))
                    .on_press(Message::ReconnectTunnel(tunnel.id.clone())),
            );
        }

        let mut item = column![title_row, controls_row].spacing(3);

        if let Some(notice) = &tunnel.reconnected_notice {
            item = item.push(badge_pill(
                notice.clone(),
                chrome.success,
                chrome.success_soft,
                Color::TRANSPARENT,
            ));
        }

        match &tunnel.state {
            TunnelRuntimeState::Retrying { reason, .. } => {
                item = item.push(
                    text(format!("Auto-reconnecting: {reason}"))
                        .size(FONT_META)
                        .color(chrome.warning),
                );
            }
            TunnelRuntimeState::Reconnecting | TunnelRuntimeState::Holding => {
                let msg = tunnel
                    .last_error
                    .as_deref()
                    .map(|err| format!("Auto-reconnecting: {err}"))
                    .unwrap_or_else(|| "Auto-reconnecting tunnel...".to_string());
                item = item.push(text(msg).size(FONT_META).color(chrome.warning));
            }
            TunnelRuntimeState::Error(err) => {
                item = item.push(text(err.as_str()).size(FONT_META).color(chrome.danger));
            }
            _ => {}
        }

        let row_card = container(item).padding([4, 10]).width(Length::Fill).into();
        rows_col = rows_col.push(TunnelRightClickCapture::new(tunnel.id.clone(), row_card));
    }

    col = col.push(rows_col);
    col.into()
}
