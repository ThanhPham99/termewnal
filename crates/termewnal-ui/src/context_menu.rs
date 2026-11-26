//! Right-click context menu overlays for terminal panes, workspace tabs, and sidebar items.

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::widget::{Id, button, column, container, mouse_area, opaque, row, space, stack, text};
use iced::{
    Alignment, Background, Border, Color, Element, Event, Length, Padding, Point, Rectangle, Size,
    Theme, Vector, mouse,
};
use termewnal_core::ThemePalette;

use crate::app::{LaunchDisposition, Message, SessionId};
use crate::sidebar::UNGROUPED_LABEL;
use crate::ui_theme::{
    ChromePalette, FONT_BODY, FONT_META, PillTone, hairline_h, list_row_button_style,
    modal_card_style, modal_width, mono_font, pill_button_style, styled_input_style, ui_font,
    ui_font_semibold,
};
use crate::workspace::{SplitAxis, TabId};

/// Static widget [`Id`] for context prompt text inputs (tab rename, group create/rename).
pub const fn context_prompt_input_id() -> Id {
    Id::new("context-prompt-input")
}

/// Curated accent swatches shown in the Tab Context Menu color picker (from Tabby `TAB_COLORS`).
pub const TAB_COLOR_PRESETS: &[(&str, &str)] = &[
    ("Default", ""),
    ("Blue", "#0275d8"),
    ("Green", "#5cb85c"),
    ("Orange", "#f0ad4e"),
    ("Purple", "#613d7c"),
    ("Red", "#d9534f"),
    ("Yellow", "#ffd500"),
];

/// Active floating context menu target.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextMenuKind {
    Terminal {
        session_id: SessionId,
        has_selection: bool,
        logging_active: bool,
        is_ssh: bool,
    },
    Tab {
        tab_id: TabId,
        pinned: bool,
        can_close_others: bool,
        can_close_right: bool,
        current_color: Option<String>,
    },
    Sidebar {
        group_name: Option<String>,
        profile_id: Option<String>,
        profile_name: Option<String>,
        available_groups: Vec<String>,
    },
    Tunnel {
        tunnel_id: String,
        tunnel_name: String,
        running: bool,
        can_reconnect: bool,
    },
}

/// Floating context menu state + optional inline tab/group prompt modals.
#[derive(Debug, Clone, Default)]
pub struct ContextMenuState {
    pub active: Option<(ContextMenuKind, Point)>,
    pub renaming_tab: Option<(TabId, String)>,
    pub creating_group: Option<(String, Option<String>)>,
    pub renaming_group: Option<(String, String)>,
}

impl ContextMenuState {
    pub fn open_terminal(
        &mut self,
        session_id: SessionId,
        position: Point,
        has_selection: bool,
        logging_active: bool,
        is_ssh: bool,
    ) {
        self.clear_prompts();
        self.active = Some((
            ContextMenuKind::Terminal {
                session_id,
                has_selection,
                logging_active,
                is_ssh,
            },
            position,
        ));
    }

    pub fn open_tab(
        &mut self,
        tab_id: TabId,
        position: Point,
        pinned: bool,
        can_close_others: bool,
        can_close_right: bool,
        current_color: Option<String>,
    ) {
        self.clear_prompts();
        self.active = Some((
            ContextMenuKind::Tab {
                tab_id,
                pinned,
                can_close_others,
                can_close_right,
                current_color,
            },
            position,
        ));
    }

    pub fn open_sidebar(
        &mut self,
        position: Point,
        group_name: Option<String>,
        profile_id: Option<String>,
        profile_name: Option<String>,
        available_groups: Vec<String>,
    ) {
        self.clear_prompts();
        self.active = Some((
            ContextMenuKind::Sidebar {
                group_name,
                profile_id,
                profile_name,
                available_groups,
            },
            position,
        ));
    }

    pub fn open_tunnel(
        &mut self,
        tunnel_id: String,
        tunnel_name: String,
        running: bool,
        can_reconnect: bool,
        position: Point,
    ) {
        self.clear_prompts();
        self.active = Some((
            ContextMenuKind::Tunnel {
                tunnel_id,
                tunnel_name,
                running,
                can_reconnect,
            },
            position,
        ));
    }

    pub fn start_rename(&mut self, tab_id: TabId, initial_title: String) {
        self.active = None;
        self.clear_prompts();
        self.renaming_tab = Some((tab_id, initial_title));
    }

    pub fn start_create_group(&mut self, assign_profile_id: Option<String>) {
        self.active = None;
        self.clear_prompts();
        self.creating_group = Some((String::new(), assign_profile_id));
    }

    pub fn start_rename_group(&mut self, old_name: String) {
        self.active = None;
        self.clear_prompts();
        let initial = old_name.clone();
        self.renaming_group = Some((old_name, initial));
    }

    fn clear_prompts(&mut self) {
        self.renaming_tab = None;
        self.creating_group = None;
        self.renaming_group = None;
    }

    pub fn close(&mut self) {
        self.active = None;
        self.clear_prompts();
    }

    pub fn is_open(&self) -> bool {
        self.active.is_some()
            || self.renaming_tab.is_some()
            || self.creating_group.is_some()
            || self.renaming_group.is_some()
    }
}

fn render_text_prompt_modal<'a>(
    title: &'a str,
    placeholder: &'a str,
    draft: &'a str,
    submit_label: &'a str,
    on_input: fn(String) -> Message,
    on_submit: Message,
    window_width: f32,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let card = container(
        column![
            text(title)
                .size(14)
                .font(ui_font_semibold())
                .color(chrome.text_primary),
            iced::widget::text_input(placeholder, draft)
                .id(context_prompt_input_id())
                .on_input(on_input)
                .on_submit(on_submit.clone())
                .size(13)
                .font(ui_font())
                .padding([7, 10])
                .style(styled_input_style(chrome)),
            row![
                space::horizontal(),
                button(text("Cancel").size(12).font(ui_font()))
                    .padding([5, 11])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::DismissContextMenu),
                button(text(submit_label).size(12).font(ui_font_semibold()))
                    .padding([5, 13])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(on_submit),
            ]
            .spacing(8),
        ]
        .spacing(10),
    )
    .width(Length::Fixed(modal_width(360.0, window_width)))
    .padding(16)
    .style(move |_| modal_card_style(chrome, true));

    crate::ui_theme::modal_overlay(card.into(), Some(Message::DismissContextMenu), 32.0)
}

/// Renders either the active floating context menu or prompt modal over a click-dismiss scrim.
pub fn render_context_menu_overlay<'a>(
    state: &'a ContextMenuState,
    window_width: f32,
    window_height: f32,
    palette: &ThemePalette,
) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
    let chrome = ChromePalette::from_palette(palette);

    if let Some((_tab_id, draft)) = &state.renaming_tab {
        return Some(render_text_prompt_modal(
            "Rename Tab",
            "Tab title (empty to restore automatic title)",
            draft,
            "Save",
            Message::TabRenameDraftChanged,
            Message::SubmitTabRename,
            window_width,
            chrome,
        ));
    }

    if let Some((draft, assign_profile_id)) = &state.creating_group {
        let title = if assign_profile_id.is_some() {
            "New Group & Move Profile"
        } else {
            "New Profile Group"
        };
        return Some(render_text_prompt_modal(
            title,
            "Group name (e.g. Production)",
            draft,
            "Create",
            Message::CreateGroupDraftChanged,
            Message::SubmitCreateGroup,
            window_width,
            chrome,
        ));
    }

    if let Some((_old_name, draft)) = &state.renaming_group {
        return Some(render_text_prompt_modal(
            "Rename Group",
            "Group name (e.g. Production)",
            draft,
            "Rename",
            Message::RenameGroupDraftChanged,
            Message::SubmitRenameGroup,
            window_width,
            chrome,
        ));
    }

    let (kind, pos) = state.active.as_ref()?;
    let menu_w = 248.0_f32;
    let menu_h = match kind {
        ContextMenuKind::Terminal { .. } => 340.0_f32,
        ContextMenuKind::Tab { .. } => 280.0_f32,
        ContextMenuKind::Sidebar {
            profile_id,
            available_groups,
            ..
        } => {
            if profile_id.is_some() {
                (230.0 + (available_groups.len() as f32 * 30.0)).min(440.0)
            } else {
                200.0
            }
        }
        ContextMenuKind::Tunnel { .. } => 170.0_f32,
    };
    let left = pos.x.clamp(8.0, (window_width - menu_w - 12.0).max(8.0));
    let top = pos.y.clamp(8.0, (window_height - menu_h - 12.0).max(8.0));

    let items: Element<'a, Message, Theme, iced::Renderer> = match kind {
        ContextMenuKind::Terminal {
            session_id,
            has_selection,
            logging_active,
            is_ssh,
        } => {
            let sid = *session_id;
            let mut col = column![].spacing(2);
            if *has_selection {
                col = col.push(menu_row("Copy Selection", Some("Ctrl+Shift+C"), chrome, Message::CopySelection { session_id: sid }));
            }
            col = col.push(menu_row("Paste", Some("Ctrl+Shift+V"), chrome, Message::PasteFromClipboard { session_id: sid }));
            col = col.push(menu_row("Clear Buffer", Some("Ctrl+Shift+L"), chrome, Message::ClearTerminalBuffer(sid)));
            col = col.push(hairline_h(chrome));
            col = col.push(menu_row("Find in Scrollback...", Some("Ctrl+F"), chrome, Message::OpenSearch));
            col = col.push(menu_row("Save Scrollback to File", None, chrome, Message::SaveScrollback(sid)));
            col = col.push(menu_row(
                if *logging_active { "Stop Session Logging" } else { "Start Session Logging" },
                None,
                chrome,
                Message::ToggleSessionLogging(sid),
            ));
            col = col.push(hairline_h(chrome));
            col = col.push(menu_row("Split Right (Vertical)", Some("Ctrl+Shift+D"), chrome, Message::SplitFocusedPane(crate::workspace::SplitAxis::Vertical)));
            col = col.push(menu_row("Split Down (Horizontal)", Some("Ctrl+Shift+O"), chrome, Message::SplitFocusedPane(crate::workspace::SplitAxis::Horizontal)));
            col = col.push(menu_row("Quick Commands...", Some("Ctrl+Shift+Q"), chrome, Message::OpenQuickCommandsModal));
            col = col.push(menu_row("Zmodem Send File...", None, chrome, Message::TriggerZmodemSend(sid)));
            if *is_ssh {
                col = col.push(menu_row("Toggle SFTP+ Panel", Some("Ctrl+Shift+E"), chrome, Message::ToggleSftpDrawer));
            }
            col = col.push(hairline_h(chrome));
            col = col.push(menu_row("Close Pane", Some("Ctrl+Shift+W"), chrome, Message::CloseFocusedPane));
            col.into()
        }
        ContextMenuKind::Tab {
            tab_id,
            pinned,
            can_close_others,
            can_close_right,
            current_color,
        } => {
            let tid = *tab_id;
            let mut swatches = row![].spacing(5).align_y(Alignment::Center);
            for (label, hex) in TAB_COLOR_PRESETS {
                let selected = current_color.as_deref().unwrap_or("") == *hex;
                let color_arg = (!hex.is_empty()).then(|| hex.to_string());
                let swatch_color = if hex.is_empty() {
                    chrome.text_muted
                } else {
                    parse_hex_color(hex).unwrap_or(chrome.accent)
                };
                let swatch_btn = button(
                    container(space::horizontal().width(14).height(14)).style(move |_| container::Style {
                        background: Some(Background::Color(swatch_color)),
                        border: Border {
                            color: if selected { chrome.text_primary } else { chrome.border },
                            width: if selected { 2.0 } else { 1.0 },
                            radius: 999.0.into(),
                        },
                        ..Default::default()
                    }),
                )
                .padding(2)
                .style(pill_button_style(chrome, if selected { PillTone::ActiveAccent } else { PillTone::Ghost }))
                .on_press(Message::SetTabColor {
                    tab_id: tid,
                    color: color_arg,
                });
                let _ = label;
                swatches = swatches.push(swatch_btn);
            }

            let mut col = column![
                menu_row("Rename Tab...", Some("Ctrl+Shift+R"), chrome, Message::BeginTabRename(tid)),
                menu_row(
                    if *pinned { "Unpin Tab" } else { "Pin Tab" },
                    None,
                    chrome,
                    Message::TogglePinTab(tid),
                ),
                menu_row("Duplicate Tab", Some("Ctrl+Shift+N"), chrome, Message::DuplicateTab(tid)),
                container(column![
                    text("Tab Accent").size(FONT_META).color(chrome.text_muted),
                    swatches,
                ].spacing(4)).padding([4, 8]),
                hairline_h(chrome),
            ]
            .spacing(2);

            if *can_close_others {
                col = col.push(menu_row("Close Other Tabs", None, chrome, Message::CloseOtherTabs(tid)));
            }
            if *can_close_right {
                col = col.push(menu_row("Close Tabs to Right", None, chrome, Message::CloseTabsToRight(tid)));
            }
            if !*pinned {
                col = col.push(menu_row("Close Tab", None, chrome, Message::CloseTab(tid)));
            }
            col.into()
        }
        ContextMenuKind::Sidebar {
            group_name,
            profile_id,
            profile_name,
            available_groups,
        } => {
            let mut col = column![].spacing(2);
            if let Some(pid) = profile_id {
                if let Some(name) = profile_name {
                    col = col.push(
                        container(
                            text(name.as_str())
                                .size(FONT_META)
                                .font(ui_font_semibold())
                                .color(chrome.text_muted),
                        )
                        .padding([4, 10]),
                    );
                }
                col = col.push(menu_row(
                    "Connect in New Tab",
                    None,
                    chrome,
                    Message::LaunchProfile {
                        profile_id: pid.clone(),
                        disposition: LaunchDisposition::NewTab,
                    },
                ));
                col = col.push(menu_row(
                    "Split Right",
                    None,
                    chrome,
                    Message::LaunchProfile {
                        profile_id: pid.clone(),
                        disposition: LaunchDisposition::Split(SplitAxis::Vertical),
                    },
                ));
                col = col.push(menu_row(
                    "Split Down",
                    None,
                    chrome,
                    Message::LaunchProfile {
                        profile_id: pid.clone(),
                        disposition: LaunchDisposition::Split(SplitAxis::Horizontal),
                    },
                ));
                col = col.push(hairline_h(chrome));
                col = col.push(menu_row(
                    "Edit Profile...",
                    None,
                    chrome,
                    Message::OpenEditProfileModal(pid.clone()),
                ));
                col = col.push(menu_row(
                    "Move to New Group...",
                    None,
                    chrome,
                    Message::BeginCreateGroup {
                        assign_profile_id: Some(pid.clone()),
                    },
                ));
                for target_group in available_groups {
                    if group_name
                        .as_deref()
                        .is_some_and(|cur| cur.eq_ignore_ascii_case(target_group))
                    {
                        continue;
                    }
                    col = col.push(menu_row(
                        format!("Move to \"{target_group}\""),
                        None,
                        chrome,
                        Message::MoveProfileToGroup {
                            profile_id: pid.clone(),
                            group: Some(target_group.clone()),
                        },
                    ));
                }
                if group_name
                    .as_deref()
                    .is_some_and(|g| !g.trim().is_empty() && !g.eq_ignore_ascii_case(UNGROUPED_LABEL))
                {
                    col = col.push(menu_row(
                        "Move to Ungrouped",
                        None,
                        chrome,
                        Message::MoveProfileToGroup {
                            profile_id: pid.clone(),
                            group: None,
                        },
                    ));
                }
                col = col.push(hairline_h(chrome));
                col = col.push(menu_row(
                    "Delete Profile",
                    None,
                    chrome,
                    Message::DeleteSavedProfileById(pid.clone()),
                ));
            } else if let Some(gname) = group_name {
                if !gname.eq_ignore_ascii_case(UNGROUPED_LABEL) {
                    col = col.push(menu_row(
                        format!("New Profile in \"{gname}\"..."),
                        None,
                        chrome,
                        Message::OpenNewProfileInGroup(Some(gname.clone())),
                    ));
                    col = col.push(menu_row(
                        "New Group...",
                        None,
                        chrome,
                        Message::BeginCreateGroup {
                            assign_profile_id: None,
                        },
                    ));
                    col = col.push(menu_row(
                        "Rename Group...",
                        None,
                        chrome,
                        Message::BeginRenameGroup(gname.clone()),
                    ));
                    col = col.push(hairline_h(chrome));
                    col = col.push(menu_row(
                        "Delete Group",
                        None,
                        chrome,
                        Message::DeleteProfileGroup(gname.clone()),
                    ));
                } else {
                    col = col.push(menu_row(
                        "New Profile...",
                        None,
                        chrome,
                        Message::OpenNewProfileModal,
                    ));
                    col = col.push(menu_row(
                        "New Group...",
                        None,
                        chrome,
                        Message::BeginCreateGroup {
                            assign_profile_id: None,
                        },
                    ));
                }
            } else {
                col = col.push(menu_row(
                    "New Group...",
                    None,
                    chrome,
                    Message::BeginCreateGroup {
                        assign_profile_id: None,
                    },
                ));
                col = col.push(menu_row(
                    "New Profile...",
                    None,
                    chrome,
                    Message::OpenNewProfileModal,
                ));
                col = col.push(menu_row(
                    "Quick SSH...",
                    None,
                    chrome,
                    Message::OpenQuickConnectPalette,
                ));
                col = col.push(hairline_h(chrome));
                col = col.push(menu_row(
                    "Import from Tabby...",
                    None,
                    chrome,
                    Message::OpenTabbyImportModal,
                ));
            }
            col.into()
        }
        ContextMenuKind::Tunnel {
            tunnel_id,
            tunnel_name,
            running,
            can_reconnect,
        } => {
            let mut col = column![
                container(
                    text(tunnel_name.as_str())
                        .size(FONT_META)
                        .font(ui_font_semibold())
                        .color(chrome.text_muted),
                )
                .padding([4, 10]),
                menu_row(
                    if *running {
                        "Stop Tunnel"
                    } else {
                        "Start Tunnel"
                    },
                    None,
                    chrome,
                    Message::ToggleTunnel(tunnel_id.clone()),
                ),
            ]
            .spacing(2);
            if *can_reconnect {
                col = col.push(menu_row(
                    "Reconnect Tunnel",
                    None,
                    chrome,
                    Message::ReconnectTunnel(tunnel_id.clone()),
                ));
            }
            col = col.push(hairline_h(chrome));
            col = col.push(menu_row(
                "Edit Tunnel...",
                None,
                chrome,
                Message::OpenEditTunnelModal(tunnel_id.clone()),
            ));
            col = col.push(menu_row(
                "Delete Tunnel",
                None,
                chrome,
                Message::DeleteTunnel(tunnel_id.clone()),
            ));
            col.into()
        }
    };

    let card = container(items)
        .width(Length::Fixed(menu_w))
        .padding(6)
        .style(move |_| modal_card_style(chrome, false));

    let scrim = mouse_area(
        container(space::horizontal().width(Length::Fill).height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .on_press(Message::DismissContextMenu)
    .on_right_press(Message::DismissContextMenu);

    let positioned = container(opaque(card))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding {
            top,
            left,
            right: 8.0,
            bottom: 8.0,
        });

    Some(opaque(stack![scrim, positioned]).into())
}

fn menu_row<'a>(
    label: impl Into<std::borrow::Cow<'a, str>>,
    shortcut: Option<&'a str>,
    chrome: ChromePalette,
    msg: Message,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let mut r = row![
        text(label.into())
            .size(FONT_BODY)
            .font(ui_font())
            .color(chrome.text_primary)
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if let Some(sc) = shortcut {
        r = r
            .push(space::horizontal())
            .push(text(sc).size(FONT_META).font(mono_font()).color(chrome.text_muted));
    }
    button(r)
        .padding([6, 10])
        .width(Length::Fill)
        .style(list_row_button_style(chrome, false))
        .on_press(msg)
        .into()
}

/// Parses `"#RRGGBB"` into an iced `Color`.
pub fn parse_hex_color(raw: &str) -> Option<Color> {
    let rgb = termewnal_core::RgbColor::from_hex(raw)?;
    Some(Color::from_rgb8(rgb.r, rgb.g, rgb.b))
}

#[derive(Debug, Clone)]
enum RightClickActionTarget {
    Tab(TabId),
    Sidebar {
        group_name: Option<String>,
        profile_id: Option<String>,
    },
    Tunnel(String),
}

/// Shared right-click interceptor that publishes a context menu message when not already captured.
struct RightClickCaptureWidget<'a> {
    target: RightClickActionTarget,
    capture_before_children: bool,
    content: Element<'a, Message, Theme, iced::Renderer>,
}

impl Widget<Message, Theme, iced::Renderer> for RightClickCaptureWidget<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        let publish_right_click = |position: Point, shell: &mut Shell<'_, Message>| {
            let msg = match &self.target {
                RightClickActionTarget::Tab(tab_id) => Message::OpenTabContextMenu {
                    tab_id: *tab_id,
                    position,
                },
                RightClickActionTarget::Sidebar {
                    group_name,
                    profile_id,
                } => Message::OpenSidebarContextMenu {
                    group_name: group_name.clone(),
                    profile_id: profile_id.clone(),
                    position,
                },
                RightClickActionTarget::Tunnel(tunnel_id) => Message::OpenTunnelContextMenu {
                    tunnel_id: tunnel_id.clone(),
                    position,
                },
            };
            shell.publish(msg);
            shell.capture_event();
        };

        if self.capture_before_children
            && let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) = event
            && cursor.is_over(layout.bounds())
            && let Some(position) = cursor.position()
        {
            publish_right_click(position, shell);
            return;
        }

        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        if !self.capture_before_children
            && !shell.is_event_captured()
            && let Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) = event
            && cursor.is_over(layout.bounds())
            && let Some(position) = cursor.position()
        {
            publish_right_click(position, shell);
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
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
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
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
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

/// Wraps a workspace tab chip to publish [`Message::OpenTabContextMenu`] on right-click
/// without swallowing left-click events bound to inner buttons.
pub struct TabRightClickCapture;

impl TabRightClickCapture {
    pub fn new<'a>(
        tab_id: TabId,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(RightClickCaptureWidget {
            target: RightClickActionTarget::Tab(tab_id),
            capture_before_children: true,
            content,
        })
    }
}

/// Wraps a Left Sidebar profile, group, or section area to open [`ContextMenuKind::Sidebar`].
///
/// Children are checked first so profile rows take precedence over enclosing group/sidebar areas.
pub struct SidebarRightClickCapture;

impl SidebarRightClickCapture {
    pub fn new<'a>(
        group_name: Option<String>,
        profile_id: Option<String>,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(RightClickCaptureWidget {
            target: RightClickActionTarget::Sidebar {
                group_name,
                profile_id,
            },
            capture_before_children: false,
            content,
        })
    }
}

/// Wraps a Left Sidebar tunnel row to open [`ContextMenuKind::Tunnel`] on right-click.
pub struct TunnelRightClickCapture;

impl TunnelRightClickCapture {
    pub fn new<'a>(
        tunnel_id: String,
        content: Element<'a, Message, Theme, iced::Renderer>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        Element::new(RightClickCaptureWidget {
            target: RightClickActionTarget::Tunnel(tunnel_id),
            capture_before_children: false,
            content,
        })
    }
}
