use std::collections::{BTreeMap, BTreeSet};

use iced::widget::{Id, button, column, container, row, rule, scrollable, space, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Theme};
use termewnal_core::{Profile, ProfileTarget, TunnelDirection};

use crate::app::{LaunchDisposition, Message, SessionId};
use crate::workspace::SplitAxis;

/// Group heading used when a saved `Profile` has no explicit group assigned.
pub const UNGROUPED_LABEL: &str = "Ungrouped";

const SIDEBAR_WIDTH: f32 = 270.0;

/// Returns the static widget [`Id`] for the Left Sidebar profile filter input.
pub const fn sidebar_filter_input_id() -> Id {
    Id::new("sidebar-filter-input")
}

/// Top-level collapsible sections inside the Left Sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SidebarSection {
    SavedProfiles,
    SystemSshProfiles,
    ActiveTunnels,
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
}

/// Lifecycle state of an active `Tunnel` shown in the Left Sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelRuntimeState {
    Stopped,
    Starting,
    Active,
    Holding,
    Reconnecting,
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
            Self::Reconnecting => "Reconnecting",
            Self::Error(_) => "Error",
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
}

/// State backing the collapsible Left Sidebar (`Ctrl+B`).
#[derive(Debug, Clone, Default)]
pub struct SidebarState {
    open: bool,
    filter_query: String,
    selected_tag: Option<String>,
    collapsed_groups: BTreeSet<String>,
    collapsed_sections: BTreeSet<SidebarSection>,
    system_ssh_profiles: Vec<SystemSshProfile>,
    active_tunnels: Vec<ActiveTunnelEntry>,
}

impl SidebarState {
    /// Creates a new collapsed [`SidebarState`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when the Left Sidebar is expanded.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Explicitly opens or collapses the Left Sidebar.
    pub fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    /// Toggles Left Sidebar visibility and returns the updated state.
    pub fn toggle(&mut self) -> bool {
        self.open = !self.open;
        self.open
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
        let needle = self.filter_query.trim().to_ascii_lowercase();
        let active_tag = self.selected_tag.as_deref().map(str::to_ascii_lowercase);

        let mut grouped: BTreeMap<(bool, String, String), Vec<&'a Profile>> = BTreeMap::new();
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
    }
}

/// Renders the Left Sidebar displaying Saved `Profile`s, System SSH hosts, and Active `Tunnel`s.
pub fn render_sidebar<'a>(
    state: &'a SidebarState,
    profiles: &'a [Profile],
) -> Element<'a, Message, Theme, iced::Renderer> {
    let header = row![
        text("Profiles & Tunnels").size(14),
        space::horizontal(),
        button(text("+ SSH").size(11))
            .padding([2, 6])
            .style(button::secondary)
            .on_press(Message::OpenQuickConnectPalette),
        button(text("x").size(12))
            .padding([2, 7])
            .style(button::subtle)
            .on_press(Message::SetSidebarOpen(false)),
    ]
    .spacing(4)
    .align_y(Alignment::Center);

    let filter_input = text_input("Filter profiles...", state.filter_query())
        .id(sidebar_filter_input_id())
        .on_input(Message::SidebarFilterChanged)
        .size(12)
        .padding([5, 8]);

    let mut content = column![header, filter_input].spacing(8);

    let tags = state.available_tags(profiles);
    if !tags.is_empty() {
        let mut tag_row = row![].spacing(4).align_y(Alignment::Center);
        for tag in tags {
            let active = state.selected_tag() == Some(tag.as_str());
            tag_row = tag_row.push(
                button(text(format!("#{tag}")).size(11))
                    .padding([2, 6])
                    .style(if active {
                        button::primary
                    } else {
                        button::subtle
                    })
                    .on_press(Message::SidebarToggleTagFilter(tag)),
            );
        }
        let tags_scroll = scrollable(tag_row).direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::new().width(2).scroller_width(2),
        ));
        content = content.push(tags_scroll);
    }

    let mut sections = column![].spacing(10);
    sections = sections.push(render_saved_profiles_section(state, profiles));
    sections = sections.push(rule::horizontal(1));
    sections = sections.push(render_system_ssh_section(state));
    sections = sections.push(rule::horizontal(1));
    sections = sections.push(render_tunnels_section(state));

    content = content.push(
        scrollable(sections)
            .width(Length::Fill)
            .height(Length::Fill),
    );

    container(content.padding(10))
        .width(Length::Fixed(SIDEBAR_WIDTH))
        .height(Length::Fill)
        .style(|theme: &Theme| {
            let ext = theme.extended_palette();
            container::Style {
                background: Some(Background::Color(ext.background.weak.color)),
                border: Border {
                    color: ext.background.strong.color,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            }
        })
        .into()
}

fn render_saved_profiles_section<'a>(
    state: &'a SidebarState,
    profiles: &'a [Profile],
) -> Element<'a, Message, Theme, iced::Renderer> {
    let collapsed = state.is_section_collapsed(SidebarSection::SavedProfiles);
    let prefix = if collapsed { ">" } else { "v" };
    let header = button(text(format!("{prefix} SAVED PROFILES ({})", profiles.len())).size(11))
        .padding([3, 4])
        .width(Length::Fill)
        .style(button::subtle)
        .on_press(Message::SidebarToggleSection(SidebarSection::SavedProfiles));

    let mut col = column![header].spacing(6);
    if collapsed {
        return col.into();
    }

    let groups = state.grouped_saved_profiles(profiles);
    if groups.is_empty() {
        col = col.push(
            text("No matching saved profiles")
                .size(12)
                .color(Color::from_rgb8(148, 163, 184)),
        );
        return col.into();
    }

    for group in groups {
        let group_prefix = if group.collapsed { ">" } else { "v" };
        let group_title = format!("{group_prefix} {} ({})", group.name, group.profiles.len());
        col = col.push(
            button(text(group_title).size(12))
                .padding([3, 6])
                .width(Length::Fill)
                .style(button::secondary)
                .on_press(Message::SidebarToggleGroup(group.name.clone())),
        );

        if group.collapsed {
            continue;
        }

        for profile in group.profiles {
            let kind_badge = match profile.target {
                ProfileTarget::LocalShell { .. } => "LOCAL",
                ProfileTarget::Ssh { .. } => "SSH",
            };
            let subtitle = profile_target_summary(profile);
            let tags_suffix = if profile.tags.is_empty() {
                String::new()
            } else {
                format!(
                    "  [{}]",
                    profile
                        .tags
                        .iter()
                        .map(|t| format!("#{t}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            };

            let item_label = column![
                text(format!("[{kind_badge}] {}", profile.name)).size(12),
                text(format!("{subtitle}{tags_suffix}"))
                    .size(11)
                    .color(Color::from_rgb8(148, 163, 184)),
            ]
            .spacing(2);

            let launch_btn = button(item_label)
                .padding([4, 8])
                .width(Length::Fill)
                .style(button::subtle)
                .on_press(Message::LaunchProfile {
                    profile_id: profile.id.clone(),
                    disposition: LaunchDisposition::NewTab,
                });

            let split_v_btn = button(text("|").size(11))
                .padding([4, 5])
                .style(button::subtle)
                .on_press(Message::LaunchProfile {
                    profile_id: profile.id.clone(),
                    disposition: LaunchDisposition::Split(SplitAxis::Vertical),
                });

            let split_h_btn = button(text("-").size(11))
                .padding([4, 5])
                .style(button::subtle)
                .on_press(Message::LaunchProfile {
                    profile_id: profile.id.clone(),
                    disposition: LaunchDisposition::Split(SplitAxis::Horizontal),
                });

            col = col.push(
                row![launch_btn, split_v_btn, split_h_btn]
                    .spacing(2)
                    .align_y(Alignment::Center),
            );
        }
    }

    col.into()
}

fn render_system_ssh_section(state: &SidebarState) -> Element<'_, Message, Theme, iced::Renderer> {
    let collapsed = state.is_section_collapsed(SidebarSection::SystemSshProfiles);
    let prefix = if collapsed { ">" } else { "v" };
    let count = state.system_ssh_profiles().len();
    let header = button(text(format!("{prefix} SYSTEM SSH ({count})")).size(11))
        .padding([3, 4])
        .width(Length::Fill)
        .style(button::subtle)
        .on_press(Message::SidebarToggleSection(
            SidebarSection::SystemSshProfiles,
        ));

    let mut col = column![header].spacing(6);
    if collapsed {
        return col.into();
    }

    if state.system_ssh_profiles().is_empty() {
        col = col.push(
            text("Read-only ~/.ssh/config hosts appear here")
                .size(11)
                .color(Color::from_rgb8(148, 163, 184)),
        );
        return col.into();
    }

    for host in state.system_ssh_profiles() {
        let item = column![
            text(format!("[RO] {}", host.alias)).size(12),
            text(host.display_target())
                .size(11)
                .color(Color::from_rgb8(148, 163, 184)),
        ]
        .spacing(2);
        col = col.push(
            button(item)
                .padding([4, 8])
                .width(Length::Fill)
                .style(button::subtle)
                .on_press(Message::LaunchSystemSsh {
                    alias: host.alias.clone(),
                    disposition: LaunchDisposition::NewTab,
                }),
        );
    }

    col.into()
}

fn render_tunnels_section(state: &SidebarState) -> Element<'_, Message, Theme, iced::Renderer> {
    let collapsed = state.is_section_collapsed(SidebarSection::ActiveTunnels);
    let prefix = if collapsed { ">" } else { "v" };
    let count = state.active_tunnels().len();
    let header = button(text(format!("{prefix} ACTIVE TUNNELS ({count})")).size(11))
        .padding([3, 4])
        .width(Length::Fill)
        .style(button::subtle)
        .on_press(Message::SidebarToggleSection(SidebarSection::ActiveTunnels));

    let mut col = column![header].spacing(6);
    if collapsed {
        return col.into();
    }

    if state.active_tunnels().is_empty() {
        col = col.push(
            text("No active tunnels")
                .size(11)
                .color(Color::from_rgb8(148, 163, 184)),
        );
        return col.into();
    }

    for tunnel in state.active_tunnels() {
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
        let item = column![
            text(format!("[{dir}] {} · {}", tunnel.name, tunnel.state.label())).size(12),
            text(format!("{} -> {} ({scope})", tunnel.bind_addr, tunnel.target_addr))
                .size(11)
                .color(Color::from_rgb8(148, 163, 184)),
        ]
        .spacing(2);
        col = col.push(container(item).padding([4, 8]).width(Length::Fill));
    }

    col.into()
}
