use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use iced::widget::{Id, button, column, container, row, rule, scrollable, space, text, text_input};
use iced::{Alignment, Background, Color, Element, Length, Theme};
use termewnal_core::{OpenSshHost, Profile, ProfileTarget, ThemePalette, TunnelDirection};

use crate::app::{LaunchDisposition, Message, SessionId};
use crate::ui_theme::{
    ChromePalette, PillTone, badge_pill, card_container_style, pill_button_style, status_dot,
    styled_input_style,
};
use crate::workspace::SplitAxis;

/// Group heading used when a saved `Profile` has no explicit group assigned.
pub const UNGROUPED_LABEL: &str = "Ungrouped";

const SIDEBAR_WIDTH: f32 = 280.0;

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
            forward_agent: None,
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
            Self::Reconnecting => "Reconnecting",
            Self::Retrying { .. } => "Retrying",
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
            } => format!("Retrying #{attempt} ({next_retry_secs}s)"),
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
    palette: &ThemePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let chrome = ChromePalette::from_palette(palette);

    let header = row![
        text("Profiles & Tunnels")
            .size(13)
            .color(chrome.text_primary),
        space::horizontal(),
        button(text("Import").size(11))
            .padding([3, 8])
            .style(pill_button_style(chrome, PillTone::Ghost))
            .on_press(Message::OpenTabbyImportModal),
        button(text("×").size(13))
            .padding([2, 7])
            .style(pill_button_style(chrome, PillTone::Ghost))
            .on_press(Message::SetSidebarOpen(false)),
    ]
    .spacing(4)
    .align_y(Alignment::Center);

    let quick_row = row![
        button(text("+ New Profile").size(11))
            .padding([5, 8])
            .width(Length::Fill)
            .style(pill_button_style(chrome, PillTone::Primary))
            .on_press(Message::OpenNewProfileModal),
        button(text("+ Quick SSH").size(11))
            .padding([5, 8])
            .width(Length::Fill)
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::OpenQuickConnectPalette),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let filter_input = text_input("Filter profiles...", state.filter_query())
        .id(sidebar_filter_input_id())
        .on_input(Message::SidebarFilterChanged)
        .size(12)
        .padding([6, 9])
        .style(styled_input_style(chrome));

    let mut content = column![header, quick_row, filter_input].spacing(8);

    let tags = state.available_tags(profiles);
    if !tags.is_empty() {
        let mut tag_row = row![].spacing(4).align_y(Alignment::Center);
        for tag in tags {
            let active = state.selected_tag() == Some(tag.as_str());
            tag_row = tag_row.push(
                button(text(format!("#{tag}")).size(11))
                    .padding([2, 7])
                    .style(pill_button_style(
                        chrome,
                        if active {
                            PillTone::ActiveAccent
                        } else {
                            PillTone::Subtle
                        },
                    ))
                    .on_press(Message::SidebarToggleTagFilter(tag)),
            );
        }
        let tags_scroll = scrollable(tag_row).direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::new().width(2).scroller_width(2),
        ));
        content = content.push(tags_scroll);
    }

    let mut sections = column![].spacing(10);
    sections = sections.push(render_saved_profiles_section(state, profiles, chrome));
    sections = sections.push(rule::horizontal(1));
    sections = sections.push(render_system_ssh_section(state, chrome));
    sections = sections.push(rule::horizontal(1));
    sections = sections.push(render_tunnels_section(state, chrome));

    content = content.push(
        scrollable(sections)
            .width(Length::Fill)
            .height(Length::Fill),
    );

    let body = container(content.padding(10))
        .width(Length::Fixed(SIDEBAR_WIDTH))
        .height(Length::Fill)
        .style(move |_theme: &Theme| container::Style {
            background: Some(Background::Color(chrome.chrome_bg)),
            text_color: Some(chrome.text_primary),
            ..Default::default()
        });

    let right_hairline = container(space::vertical().width(1))
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(chrome.border_subtle)),
            ..Default::default()
        });

    row![body, right_hairline]
        .height(Length::Fill)
        .into()
}

fn render_saved_profiles_section<'a>(
    state: &'a SidebarState,
    profiles: &'a [Profile],
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let collapsed = state.is_section_collapsed(SidebarSection::SavedProfiles);
    let prefix = if collapsed { "▸" } else { "▾" };
    let header = button(
        text(format!("{prefix} SAVED PROFILES ({})", profiles.len()))
            .size(11)
            .color(chrome.text_muted),
    )
    .padding([3, 4])
    .width(Length::Fill)
    .style(pill_button_style(chrome, PillTone::Ghost))
    .on_press(Message::SidebarToggleSection(SidebarSection::SavedProfiles));

    let mut col = column![header].spacing(6);
    if collapsed {
        return col.into();
    }

    let groups = state.grouped_saved_profiles(profiles);
    if groups.is_empty() {
        col = col.push(
            text("No matching saved profiles")
                .size(11)
                .color(chrome.text_muted),
        );
        return col.into();
    }

    for group in groups {
        let group_prefix = if group.collapsed { "▸" } else { "▾" };
        let group_title = format!("{group_prefix} {} ({})", group.name, group.profiles.len());
        col = col.push(
            button(text(group_title).size(11).color(chrome.text_secondary))
                .padding([4, 6])
                .width(Length::Fill)
                .style(pill_button_style(chrome, PillTone::Subtle))
                .on_press(Message::SidebarToggleGroup(group.name.clone())),
        );

        if group.collapsed {
            continue;
        }

        for profile in group.profiles {
            let is_ssh = matches!(profile.target, ProfileTarget::Ssh { .. });
            let (kind_badge, dot_color, badge_fg, badge_bg) = if is_ssh {
                ("SSH", chrome.accent, chrome.accent, chrome.accent_soft)
            } else {
                ("LOCAL", chrome.success, chrome.success, chrome.success_soft)
            };
            let subtitle = profile_target_summary(profile);
            let tags_suffix = if profile.tags.is_empty() {
                String::new()
            } else {
                format!(
                    " · {}",
                    profile
                        .tags
                        .iter()
                        .map(|t| format!("#{t}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            };

            let item_label = column![
                row![
                    status_dot(dot_color),
                    badge_pill(kind_badge, badge_fg, badge_bg, Color::TRANSPARENT),
                    text(profile.name.as_str())
                        .size(12)
                        .color(chrome.text_primary),
                ]
                .spacing(5)
                .align_y(Alignment::Center),
                text(format!("{subtitle}{tags_suffix}"))
                    .size(11)
                    .color(chrome.text_muted),
            ]
            .spacing(3);

            let launch_btn = button(item_label)
                .padding([5, 6])
                .width(Length::Fill)
                .style(pill_button_style(chrome, PillTone::Ghost))
                .on_press(Message::LaunchProfile {
                    profile_id: profile.id.clone(),
                    disposition: LaunchDisposition::NewTab,
                });

            let action_col = column![
                row![
                    button(text("|").size(10))
                        .padding([2, 6])
                        .style(pill_button_style(chrome, PillTone::Ghost))
                        .on_press(Message::LaunchProfile {
                            profile_id: profile.id.clone(),
                            disposition: LaunchDisposition::Split(SplitAxis::Vertical),
                        }),
                    button(text("-").size(10))
                        .padding([2, 6])
                        .style(pill_button_style(chrome, PillTone::Ghost))
                        .on_press(Message::LaunchProfile {
                            profile_id: profile.id.clone(),
                            disposition: LaunchDisposition::Split(SplitAxis::Horizontal),
                        }),
                ]
                .spacing(2),
                button(text("Edit").size(10))
                    .padding([2, 6])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::OpenEditProfileModal(profile.id.clone())),
            ]
            .spacing(3)
            .align_x(Alignment::End);

            col = col.push(
                container(
                    row![launch_btn, action_col]
                        .spacing(4)
                        .align_y(Alignment::Center),
                )
                .padding([3, 6])
                .width(Length::Fill)
                .style(move |_| card_container_style(chrome, false)),
            );
        }
    }

    col.into()
}

fn render_system_ssh_section(
    state: &SidebarState,
    chrome: ChromePalette,
) -> Element<'_, Message, Theme, iced::Renderer> {
    let collapsed = state.is_section_collapsed(SidebarSection::SystemSshProfiles);
    let prefix = if collapsed { "▸" } else { "▾" };
    let count = state.system_ssh_profiles().len();
    let header = button(
        text(format!("{prefix} SYSTEM SSH ({count})"))
            .size(11)
            .color(chrome.text_muted),
    )
    .padding([3, 4])
    .width(Length::Fill)
    .style(pill_button_style(chrome, PillTone::Ghost))
    .on_press(Message::SidebarToggleSection(
        SidebarSection::SystemSshProfiles,
    ));

    let mut col = column![header].spacing(6);
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
            "Read-only ~/.ssh/config hosts appear here"
        } else {
            "No matching System SSH hosts"
        };
        col = col.push(text(empty_msg).size(11).color(chrome.text_muted));
        return col.into();
    }

    for host in matching {
        let subtitle = if host.proxy_jump.is_empty() {
            host.display_target()
        } else {
            format!("{} · via {}", host.display_target(), host.proxy_jump.join(","))
        };
        let item = column![
            row![
                badge_pill("SSH CFG", chrome.text_secondary, chrome.card_hover, Color::TRANSPARENT),
                text(host.alias.as_str())
                    .size(12)
                    .color(chrome.text_primary),
            ]
            .spacing(5)
            .align_y(Alignment::Center),
            text(subtitle).size(11).color(chrome.text_muted),
        ]
        .spacing(3);
        let launch_btn = button(item)
            .padding([5, 6])
            .width(Length::Fill)
            .style(pill_button_style(chrome, PillTone::Ghost))
            .on_press(Message::LaunchSystemSsh {
                alias: host.alias.clone(),
                disposition: LaunchDisposition::NewTab,
            });
        let clone_btn = button(text("Clone").size(10))
            .padding([3, 7])
            .style(pill_button_style(chrome, PillTone::Subtle))
            .on_press(Message::CloneSystemSshToProfile(host.alias.clone()));
        col = col.push(
            container(
                row![launch_btn, clone_btn]
                    .spacing(4)
                    .align_y(Alignment::Center),
            )
            .padding([3, 6])
            .width(Length::Fill)
            .style(move |_| card_container_style(chrome, false)),
        );
    }

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
        text(format!("{prefix} ACTIVE TUNNELS ({count})"))
            .size(11)
            .color(chrome.text_muted),
    )
    .padding([3, 4])
    .width(Length::Fill)
    .style(pill_button_style(chrome, PillTone::Ghost))
    .on_press(Message::SidebarToggleSection(SidebarSection::ActiveTunnels));
    let new_btn = button(text("+ Tunnel").size(10))
        .padding([3, 8])
        .style(pill_button_style(chrome, PillTone::Subtle))
        .on_press(Message::OpenTunnelCreateModal);
    let header = row![header_btn, new_btn]
        .spacing(4)
        .align_y(Alignment::Center);

    let mut col = column![header].spacing(6);
    if collapsed {
        return col.into();
    }

    if state.active_tunnels().is_empty() {
        col = col.push(text("No active tunnels").size(11).color(chrome.text_muted));
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
        let toggle_btn = button(text(if is_running { "Stop" } else { "Start" }).size(10))
            .padding([3, 7])
            .style(pill_button_style(
                chrome,
                if is_running {
                    PillTone::Subtle
                } else {
                    PillTone::Primary
                },
            ))
            .on_press(Message::ToggleTunnel(tunnel.id.clone()));

        let mut action_row = row![
            status_dot(state_fg),
            badge_pill(dir, chrome.accent, chrome.accent_soft, Color::TRANSPARENT),
            text(tunnel.name.as_str())
                .size(12)
                .color(chrome.text_primary),
            space::horizontal(),
            badge_pill(tunnel.state.status_badge(), state_fg, state_bg, Color::TRANSPARENT),
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
            action_row = action_row.push(
                button(text("Reconnect").size(10))
                    .padding([3, 7])
                    .style(pill_button_style(chrome, PillTone::Ghost))
                    .on_press(Message::ReconnectTunnel(tunnel.id.clone())),
            );
        }

        let mut item = column![
            action_row,
            text(format!("{} -> {} ({scope})", tunnel.bind_addr, tunnel.target_addr))
                .size(11)
                .color(chrome.text_muted),
        ]
        .spacing(3);

        match &tunnel.state {
            TunnelRuntimeState::Retrying { reason, .. } => {
                item = item.push(text(reason.as_str()).size(10).color(chrome.warning));
            }
            TunnelRuntimeState::Error(err) => {
                item = item.push(text(err.as_str()).size(10).color(chrome.danger));
            }
            _ => {}
        }

        col = col.push(
            container(item)
                .padding([6, 8])
                .width(Length::Fill)
                .style(move |_| card_container_style(chrome, false)),
        );
    }

    col.into()
}
