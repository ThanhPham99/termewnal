use iced::widget::{
    Id, button, column, container, mouse_area, opaque, row, rule, scrollable, space, text,
    text_input,
};
use iced::{Alignment, Background, Border, Color, Element, Length, Shadow, Theme, Vector};
use termewnal_core::{Profile, ProfileTarget};

use crate::app::{LaunchDisposition, Message, SessionId, default_os_username};
use crate::sidebar::{ActiveTunnelEntry, SystemSshProfile, profile_target_summary};
use crate::workspace::SplitAxis;

const PALETTE_MAX_WIDTH: f32 = 620.0;
const PALETTE_LIST_MAX_HEIGHT: f32 = 340.0;

/// Returns the static widget [`Id`] for the Command Palette search input.
pub const fn command_palette_input_id() -> Id {
    Id::new("command-palette-search-input")
}

/// Returns the static widget [`Id`] for the Command Palette Quick Connect input.
pub const fn quick_connect_input_id() -> Id {
    Id::new("command-palette-quick-connect-input")
}

/// Active mode inside the Command Palette modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommandPaletteMode {
    #[default]
    Search,
    QuickConnect,
}

/// Visual grouping category for an entry in the Command Palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PaletteCategory {
    QuickConnect,
    Command,
    Session,
    Profile,
    SystemSsh,
    Tunnel,
}

impl PaletteCategory {
    /// Returns a concise uppercase badge label for this category.
    pub fn badge(self) -> &'static str {
        match self {
            Self::QuickConnect => "CONNECT",
            Self::Command => "CMD",
            Self::Session => "SESSION",
            Self::Profile => "PROFILE",
            Self::SystemSsh => "SSH CFG",
            Self::Tunnel => "TUNNEL",
        }
    }
}

/// Parsed SSH destination for Quick Connect (`user@host[:port]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickConnectTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
}

impl QuickConnectTarget {
    /// Parses `[ssh ] [user@]host[:port]` (supporting bracketed IPv6 `[::1]:port`).
    pub fn parse(raw: &str) -> Option<Self> {
        let mut input = raw.trim();
        if input.len() >= 4 && input[..4].eq_ignore_ascii_case("ssh ") {
            input = input[4..].trim_start();
        }
        if input.is_empty() || input.chars().any(char::is_whitespace) {
            return None;
        }

        let (username, host_port) = if let Some((user, rest)) = input.split_once('@') {
            if user.is_empty() || rest.is_empty() || rest.contains('@') {
                return None;
            }
            (user.to_string(), rest)
        } else {
            (default_os_username(), input)
        };

        let (host, port) = if let Some(after_open) = host_port.strip_prefix('[') {
            let (addr, rest) = after_open.split_once(']')?;
            if addr.is_empty() {
                return None;
            }
            let port = if rest.is_empty() {
                22
            } else if let Some(port_str) = rest.strip_prefix(':') {
                parse_valid_port(port_str)?
            } else {
                return None;
            };
            (addr.to_string(), port)
        } else if let Some((h, p)) = host_port.rsplit_once(':') {
            if h.is_empty() || h.contains(':') {
                return None;
            }
            (h.to_string(), parse_valid_port(p)?)
        } else {
            if host_port.is_empty() {
                return None;
            }
            (host_port.to_string(), 22)
        };

        Some(Self {
            host,
            port,
            username,
        })
    }

    /// Formats this target as `username@host:port`.
    pub fn display_addr(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }
}

fn parse_valid_port(raw: &str) -> Option<u16> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port: u16 = raw.parse().ok()?;
    (port > 0).then_some(port)
}

/// Action executed when selecting a [`PaletteItem`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    NewTab,
    SplitPane(SplitAxis),
    TogglePaneZoom,
    ToggleBroadcastInput,
    ToggleSidebar,
    ToggleSftpDrawer,
    CloseFocusedPane,
    OpenQuickConnectPrompt,
    OpenTabbyImporter,
    ReloadSystemSsh,
    QuickConnect(QuickConnectTarget),
    SwitchSession(SessionId),
    LaunchProfile {
        profile_id: String,
        disposition: LaunchDisposition,
    },
    LaunchSystemSsh {
        alias: String,
        disposition: LaunchDisposition,
    },
    ToggleTunnel(String),
}

/// Lightweight active `Session` summary supplied to the Command Palette switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteSessionEntry {
    pub session_id: SessionId,
    pub title: String,
    pub target_label: String,
    pub is_active: bool,
}

/// A single searchable entry shown in the Command Palette list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    pub id: String,
    pub category: PaletteCategory,
    pub title: String,
    pub subtitle: String,
    pub keywords: Vec<String>,
    pub shortcut: Option<String>,
    pub action: PaletteAction,
}

/// Computes a case-insensitive fuzzy match score for `query` against `candidate`.
///
/// Whitespace-separated tokens must all match as ordered character subsequences; higher scores
/// indicate stronger prefix, contiguous, and word-boundary matches.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return Some(0);
    }

    let cand_lower: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    let cand_lower_str: String = cand_lower.iter().collect();
    let mut total = 0_i32;

    for token in tokens {
        let token_chars: Vec<char> = token.chars().flat_map(char::to_lowercase).collect();
        let token_str: String = token_chars.iter().collect();
        let score = score_single_token(&token_chars, &token_str, &cand_lower, &cand_lower_str)?;
        total += score;
    }

    Some(total)
}

fn score_single_token(
    token_chars: &[char],
    token_str: &str,
    cand_chars: &[char],
    cand_str: &str,
) -> Option<i32> {
    if token_chars.is_empty() {
        return Some(0);
    }

    let mut score = 0_i32;
    if cand_str.starts_with(token_str) {
        score += 120;
    } else if cand_str.contains(token_str) {
        score += 60;
    }

    let mut search_from = 0_usize;
    let mut prev_match: Option<usize> = None;

    for &tc in token_chars {
        let mut found_idx = None;
        for (idx, &cc) in cand_chars.iter().enumerate().skip(search_from) {
            if cc == tc {
                found_idx = Some(idx);
                break;
            }
        }
        let idx = found_idx?;

        score += 8;
        if idx == 0 || is_word_boundary(cand_chars[idx - 1]) {
            score += 20;
        }
        if let Some(prev) = prev_match {
            if idx == prev + 1 {
                score += 14;
            } else {
                let gap = (idx - prev - 1) as i32;
                score -= gap.min(10);
            }
        } else {
            score -= (idx as i32).min(12);
        }

        prev_match = Some(idx);
        search_from = idx + 1;
    }

    score -= (cand_chars.len() as i32 / 8).min(15);
    Some(score)
}

fn is_word_boundary(ch: char) -> bool {
    matches!(ch, ' ' | '-' | '_' | '/' | '.' | ':' | '@' | '#' | '[' | ']')
}

/// State backing the searchable Command Palette (`Ctrl+K` / `Ctrl+Shift+P`).
#[derive(Debug, Clone, Default)]
pub struct CommandPaletteState {
    open: bool,
    mode: CommandPaletteMode,
    query: String,
    quick_connect_input: String,
    selected_index: usize,
}

impl CommandPaletteState {
    /// Creates a closed [`CommandPaletteState`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` while the Command Palette modal is visible.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Returns the active [`CommandPaletteMode`] (`Search` or `QuickConnect`).
    pub fn mode(&self) -> CommandPaletteMode {
        self.mode
    }

    /// Returns the current search/filter query string.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Returns the current Quick Connect input string (`user@host[:port]`).
    pub fn quick_connect_input(&self) -> &str {
        &self.quick_connect_input
    }

    /// Returns the highlighted result row index.
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Opens the Command Palette in `Search` mode and clears any stale query.
    pub fn open(&mut self) {
        self.open = true;
        self.mode = CommandPaletteMode::Search;
        self.query.clear();
        self.selected_index = 0;
    }

    /// Opens the Command Palette directly in `QuickConnect` mode.
    pub fn open_quick_connect(&mut self) {
        self.open = true;
        self.mode = CommandPaletteMode::QuickConnect;
        self.quick_connect_input.clear();
        self.selected_index = 0;
    }

    /// Closes the Command Palette modal.
    pub fn close(&mut self) {
        self.open = false;
        self.mode = CommandPaletteMode::Search;
    }

    /// Toggles Command Palette visibility and returns the new open state.
    pub fn toggle(&mut self) -> bool {
        if self.open {
            self.close();
        } else {
            self.open();
        }
        self.open
    }

    /// Updates the search query and resets selection to the top match.
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.query = query.into();
        self.selected_index = 0;
    }

    /// Switches into `QuickConnect` input mode seeded with `initial_input`.
    pub fn enter_quick_connect(&mut self, initial_input: impl Into<String>) {
        self.mode = CommandPaletteMode::QuickConnect;
        self.quick_connect_input = initial_input.into();
    }

    /// Returns from `QuickConnect` input mode back to `Search` mode.
    pub fn exit_quick_connect(&mut self) {
        self.mode = CommandPaletteMode::Search;
    }

    /// Updates the Quick Connect input text.
    pub fn set_quick_connect_input(&mut self, input: impl Into<String>) {
        self.quick_connect_input = input.into();
    }

    /// Moves selection forward with wrap-around across `item_count` results.
    pub fn select_next(&mut self, item_count: usize) {
        if item_count == 0 {
            self.selected_index = 0;
        } else {
            let current = self.selected_index.min(item_count - 1);
            self.selected_index = (current + 1) % item_count;
        }
    }

    /// Moves selection backward with wrap-around across `item_count` results.
    pub fn select_previous(&mut self, item_count: usize) {
        if item_count == 0 {
            self.selected_index = 0;
        } else {
            let current = self.selected_index.min(item_count - 1);
            self.selected_index = (current + item_count - 1) % item_count;
        }
    }

    /// Builds and fuzzy-filters Command Palette items across actions, `Session`s, `Profile`s, and Quick Connect.
    pub fn filtered_items(
        &self,
        profiles: &[Profile],
        sessions: &[PaletteSessionEntry],
        system_ssh: &[SystemSshProfile],
        tunnels: &[ActiveTunnelEntry],
    ) -> Vec<PaletteItem> {
        let mut base_items = workspace_command_items();

        for s in sessions {
            let active_marker = if s.is_active { " (Active)" } else { "" };
            base_items.push(PaletteItem {
                id: format!("session:{}", s.session_id.0),
                category: PaletteCategory::Session,
                title: format!("Switch to Session #{}: {}", s.session_id.0, s.title),
                subtitle: format!("{}{active_marker}", s.target_label),
                keywords: vec![
                    "session".to_string(),
                    "switch".to_string(),
                    format!("#{}", s.session_id.0),
                    s.title.clone(),
                ],
                shortcut: None,
                action: PaletteAction::SwitchSession(s.session_id),
            });
        }

        for profile in profiles {
            let kind = match profile.target {
                ProfileTarget::LocalShell { .. } => "Local",
                ProfileTarget::Ssh { .. } => "SSH",
            };
            let group_label = profile.group.as_deref().unwrap_or("Ungrouped");
            let mut keywords = vec![
                "profile".to_string(),
                "launch".to_string(),
                kind.to_ascii_lowercase(),
                group_label.to_string(),
            ];
            keywords.extend(profile.tags.iter().cloned());

            base_items.push(PaletteItem {
                id: format!("profile:{}", profile.id),
                category: PaletteCategory::Profile,
                title: format!("Launch Profile: {}", profile.name),
                subtitle: format!("{kind} · {} · {group_label}", profile_target_summary(profile)),
                keywords,
                shortcut: None,
                action: PaletteAction::LaunchProfile {
                    profile_id: profile.id.clone(),
                    disposition: LaunchDisposition::NewTab,
                },
            });
        }

        for host in system_ssh {
            base_items.push(PaletteItem {
                id: format!("system-ssh:{}", host.alias),
                category: PaletteCategory::SystemSsh,
                title: format!("Launch System SSH: {}", host.alias),
                subtitle: host.display_target(),
                keywords: vec![
                    "ssh".to_string(),
                    "config".to_string(),
                    host.alias.clone(),
                    host.display_target(),
                ],
                shortcut: None,
                action: PaletteAction::LaunchSystemSsh {
                    alias: host.alias.clone(),
                    disposition: LaunchDisposition::NewTab,
                },
            });
        }

        for tunnel in tunnels {
            base_items.push(PaletteItem {
                id: format!("tunnel:{}", tunnel.id),
                category: PaletteCategory::Tunnel,
                title: format!("Toggle Tunnel: {}", tunnel.name),
                subtitle: format!(
                    "{} -> {} ({})",
                    tunnel.bind_addr,
                    tunnel.target_addr,
                    tunnel.state.label()
                ),
                keywords: vec!["tunnel".to_string(), tunnel.name.clone()],
                shortcut: None,
                action: PaletteAction::ToggleTunnel(tunnel.id.clone()),
            });
        }

        let trimmed = self.query.trim();
        if trimmed.is_empty() {
            return base_items;
        }

        let mut scored: Vec<(i32, usize, PaletteItem)> = base_items
            .into_iter()
            .enumerate()
            .filter_map(|(order, item)| {
                let score = score_palette_item(trimmed, &item)?;
                Some((score, order, item))
            })
            .collect();

        if let Some(target) = QuickConnectTarget::parse(trimmed) {
            let explicit_host_syntax = trimmed.contains('@')
                || trimmed.contains(':')
                || trimmed.contains('.')
                || trimmed.to_ascii_lowercase().starts_with("ssh ");
            let qc_score = if explicit_host_syntax { 10_000 } else { -100 };
            let addr = target.display_addr();
            scored.push((
                qc_score,
                usize::MAX,
                PaletteItem {
                    id: format!("quick-connect:{addr}"),
                    category: PaletteCategory::QuickConnect,
                    title: format!("Quick Connect to {addr}"),
                    subtitle: "Open ad-hoc SSH Session slot".to_string(),
                    keywords: vec!["ssh".to_string(), "quick connect".to_string()],
                    shortcut: None,
                    action: PaletteAction::QuickConnect(target),
                },
            ));
        }

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, _, item)| item).collect()
    }
}

fn score_palette_item(query: &str, item: &PaletteItem) -> Option<i32> {
    let haystack = format!(
        "{} {} {}",
        item.title,
        item.subtitle,
        item.keywords.join(" ")
    );
    let combined = fuzzy_score(query, &haystack)?;
    let title_bonus = fuzzy_score(query, &item.title)
        .map(|s| 200 + s * 2)
        .unwrap_or(0);
    Some(combined + title_bonus)
}

fn workspace_command_items() -> Vec<PaletteItem> {
    vec![
        PaletteItem {
            id: "cmd:quick-connect".to_string(),
            category: PaletteCategory::Command,
            title: "Quick Connect (SSH)...".to_string(),
            subtitle: "Connect to user@host[:port] without saving a Profile".to_string(),
            keywords: vec![
                "ssh".to_string(),
                "connect".to_string(),
                "host".to_string(),
                "quick".to_string(),
            ],
            shortcut: None,
            action: PaletteAction::OpenQuickConnectPrompt,
        },
        PaletteItem {
            id: "cmd:import-tabby".to_string(),
            category: PaletteCategory::Command,
            title: "Import Tabby config.yaml...".to_string(),
            subtitle: "Migrate Tabby Profiles, groups/tags, and Tunnel rules into profiles.toml"
                .to_string(),
            keywords: vec![
                "tabby".to_string(),
                "import".to_string(),
                "yaml".to_string(),
                "migrate".to_string(),
                "profiles".to_string(),
            ],
            shortcut: None,
            action: PaletteAction::OpenTabbyImporter,
        },
        PaletteItem {
            id: "cmd:reload-openssh".to_string(),
            category: PaletteCategory::Command,
            title: "Reload ~/.ssh/config".to_string(),
            subtitle: "Re-scan OpenSSH hosts in read-only mode".to_string(),
            keywords: vec![
                "openssh".to_string(),
                "ssh".to_string(),
                "config".to_string(),
                "reload".to_string(),
            ],
            shortcut: None,
            action: PaletteAction::ReloadSystemSsh,
        },
        PaletteItem {
            id: "cmd:new-tab".to_string(),
            category: PaletteCategory::Command,
            title: "New Tab".to_string(),
            subtitle: "Open a new local shell Session tab".to_string(),
            keywords: vec!["tab".to_string(), "shell".to_string(), "nt".to_string()],
            shortcut: Some("Ctrl+Shift+T".to_string()),
            action: PaletteAction::NewTab,
        },
        PaletteItem {
            id: "cmd:split-vertical".to_string(),
            category: PaletteCategory::Command,
            title: "Split Pane Vertically".to_string(),
            subtitle: "Split focused pane left/right".to_string(),
            keywords: vec!["split".to_string(), "vertical".to_string(), "pane".to_string()],
            shortcut: Some("Ctrl+Shift+D".to_string()),
            action: PaletteAction::SplitPane(SplitAxis::Vertical),
        },
        PaletteItem {
            id: "cmd:split-horizontal".to_string(),
            category: PaletteCategory::Command,
            title: "Split Pane Horizontally".to_string(),
            subtitle: "Split focused pane top/bottom".to_string(),
            keywords: vec![
                "split".to_string(),
                "horizontal".to_string(),
                "pane".to_string(),
            ],
            shortcut: Some("Ctrl+Shift+O".to_string()),
            action: PaletteAction::SplitPane(SplitAxis::Horizontal),
        },
        PaletteItem {
            id: "cmd:toggle-zoom".to_string(),
            category: PaletteCategory::Command,
            title: "Toggle Pane Zoom".to_string(),
            subtitle: "Maximize or restore the focused split pane".to_string(),
            keywords: vec!["zoom".to_string(), "maximize".to_string(), "pane".to_string()],
            shortcut: Some("Ctrl+Shift+Z".to_string()),
            action: PaletteAction::TogglePaneZoom,
        },
        PaletteItem {
            id: "cmd:toggle-broadcast".to_string(),
            category: PaletteCategory::Command,
            title: "Toggle Broadcast Input".to_string(),
            subtitle: "Mirror keystrokes across all panes in the active tab".to_string(),
            keywords: vec!["broadcast".to_string(), "input".to_string(), "mirror".to_string()],
            shortcut: Some("Ctrl+Shift+B".to_string()),
            action: PaletteAction::ToggleBroadcastInput,
        },
        PaletteItem {
            id: "cmd:toggle-sidebar".to_string(),
            category: PaletteCategory::Command,
            title: "Toggle Left Sidebar".to_string(),
            subtitle: "Show or collapse Profiles, System SSH, and Tunnels sidebar".to_string(),
            keywords: vec![
                "sidebar".to_string(),
                "profiles".to_string(),
                "tunnels".to_string(),
            ],
            shortcut: Some("Ctrl+B".to_string()),
            action: PaletteAction::ToggleSidebar,
        },
        PaletteItem {
            id: "cmd:toggle-sftp-drawer".to_string(),
            category: PaletteCategory::Command,
            title: "Toggle SFTP Browser & Transfers".to_string(),
            subtitle: "Show or hide remote SFTP File Browser and Transfer Queue drawer".to_string(),
            keywords: vec![
                "sftp".to_string(),
                "transfer".to_string(),
                "upload".to_string(),
                "download".to_string(),
                "files".to_string(),
            ],
            shortcut: Some("Ctrl+Shift+E".to_string()),
            action: PaletteAction::ToggleSftpDrawer,
        },
        PaletteItem {
            id: "cmd:close-pane".to_string(),
            category: PaletteCategory::Command,
            title: "Close Focused Pane".to_string(),
            subtitle: "Close the active pane or its tab if it is the last pane".to_string(),
            keywords: vec!["close".to_string(), "pane".to_string(), "kill".to_string()],
            shortcut: Some("Ctrl+Shift+W".to_string()),
            action: PaletteAction::CloseFocusedPane,
        },
    ]
}

/// Renders the centered Command Palette overlay modal on top of the workspace.
pub fn render_command_palette<'a>(
    palette: &'a CommandPaletteState,
    items: Vec<PaletteItem>,
    last_error: Option<&'a str>,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let card_content: Element<'a, Message, Theme, iced::Renderer> = match palette.mode() {
        CommandPaletteMode::Search => render_search_mode(palette, items),
        CommandPaletteMode::QuickConnect => render_quick_connect_mode(palette, last_error),
    };

    let card = opaque(
        container(card_content)
            .width(Length::Fixed(PALETTE_MAX_WIDTH))
            .padding(12)
            .style(|theme: &Theme| {
                let ext = theme.extended_palette();
                container::Style {
                    background: Some(Background::Color(ext.background.base.color)),
                    border: Border {
                        color: ext.primary.strong.color,
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    shadow: Shadow {
                        color: Color::from_rgba(0.0, 0.0, 0.0, 0.45),
                        offset: Vector::new(0.0, 8.0),
                        blur_radius: 20.0,
                    },
                    ..Default::default()
                }
            }),
    );

    let backdrop = container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .padding([56, 24])
        .style(|_theme: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.55))),
            ..Default::default()
        });

    mouse_area(backdrop)
        .on_press(Message::CloseCommandPalette)
        .into()
}

fn render_search_mode<'a>(
    palette: &'a CommandPaletteState,
    items: Vec<PaletteItem>,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let header = row![
        text("Command Palette").size(13),
        space::horizontal(),
        text("Ctrl+K / Esc")
            .size(11)
            .color(Color::from_rgb8(148, 163, 184)),
    ]
    .align_y(Alignment::Center);

    let input = text_input(
        "Type command, profile, session, or user@host[:port]...",
        palette.query(),
    )
    .id(command_palette_input_id())
    .on_input(Message::CommandPaletteQueryChanged)
    .on_submit(Message::CommandPaletteConfirm)
    .size(13)
    .padding([8, 10]);

    let selected = if items.is_empty() {
        0
    } else {
        palette.selected_index().min(items.len() - 1)
    };

    let mut list_col = column![].spacing(4);
    if items.is_empty() {
        list_col = list_col.push(
            container(
                text("No matching commands, profiles, or sessions")
                    .size(12)
                    .color(Color::from_rgb8(148, 163, 184)),
            )
            .padding([12, 8]),
        );
    } else {
        for (idx, item) in items.into_iter().enumerate() {
            let is_selected = idx == selected;
            let badge = item.category.badge();
            let shortcut_text = item.shortcut.clone().unwrap_or_default();
            let split_profile_id = match &item.action {
                PaletteAction::LaunchProfile { profile_id, .. } => Some(profile_id.clone()),
                _ => None,
            };

            let text_col = column![
                text(format!("[{badge}] {}", item.title)).size(13),
                text(item.subtitle)
                    .size(11)
                    .color(Color::from_rgb8(148, 163, 184)),
            ]
            .spacing(2)
            .width(Length::Fill);

            let mut row_items = row![text_col].spacing(8).align_y(Alignment::Center);
            if !shortcut_text.is_empty() {
                row_items = row_items.push(
                    text(shortcut_text)
                        .size(11)
                        .color(Color::from_rgb8(148, 163, 184)),
                );
            }

            let main_btn = button(row_items)
                .padding([6, 10])
                .width(Length::Fill)
                .style(if is_selected {
                    button::primary
                } else {
                    button::subtle
                })
                .on_press(Message::CommandPaletteExecuteItem(item.action));

            if let Some(pid) = split_profile_id {
                let split_btn = button(text("Split V").size(11))
                    .padding([6, 8])
                    .style(button::secondary)
                    .on_press(Message::LaunchProfile {
                        profile_id: pid,
                        disposition: LaunchDisposition::Split(SplitAxis::Vertical),
                    });
                list_col = list_col.push(
                    row![main_btn, split_btn]
                        .spacing(4)
                        .align_y(Alignment::Center),
                );
            } else {
                list_col = list_col.push(main_btn);
            }
        }
    }

    column![
        header,
        input,
        rule::horizontal(1),
        scrollable(list_col)
            .width(Length::Fill)
            .height(Length::Fixed(PALETTE_LIST_MAX_HEIGHT)),
    ]
    .spacing(8)
    .into()
}

fn render_quick_connect_mode<'a>(
    palette: &'a CommandPaletteState,
    last_error: Option<&'a str>,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let parsed = QuickConnectTarget::parse(palette.quick_connect_input());
    let preview = match &parsed {
        Some(target) => format!("Ready: ssh {}", target.display_addr()),
        None => "Enter destination as user@host[:port] or host".to_string(),
    };

    let header = row![
        text("Quick Connect (SSH)").size(14),
        space::horizontal(),
        button(text("Back").size(11))
            .padding([3, 8])
            .style(button::subtle)
            .on_press(Message::CommandPaletteExitQuickConnect),
    ]
    .align_y(Alignment::Center);

    let input = text_input(
        "alice@10.0.0.5:2222",
        palette.quick_connect_input(),
    )
    .id(quick_connect_input_id())
    .on_input(Message::CommandPaletteQuickConnectInputChanged)
    .on_submit(Message::CommandPaletteConfirm)
    .size(13)
    .padding([8, 10]);

    let mut connect_btn = button(text("Connect").size(12))
        .padding([6, 14])
        .style(button::primary);
    if let Some(target) = parsed {
        connect_btn = connect_btn.on_press(Message::QuickConnect(target));
    }

    let mut col = column![
        header,
        input,
        row![
            text(preview)
                .size(12)
                .color(Color::from_rgb8(148, 163, 184)),
            space::horizontal(),
            connect_btn,
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(10);

    if let Some(err) = last_error {
        col = col.push(text(err).size(11).color(Color::from_rgb8(248, 113, 113)));
    }

    col.into()
}
