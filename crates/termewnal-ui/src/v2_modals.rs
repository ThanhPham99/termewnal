//! UI modals & drawers for Quick Commands, Named Workspaces, Container/Serial/Telnet launcher,
//! and Zmodem file send picker.

use iced::widget::{button, column, container, row, scrollable, space, text, text_input};
use iced::{Alignment, Element, Length, Padding, Theme};
use termewnal_core::{
    DockerRuntime, QuickCommand, QuickCommandsConfig, ThemePalette, WorkspacesConfig,
};
use termewnal_protocols::{ContainerInfo, SerialPortInfo};

use crate::app::{Message, SessionId};
use crate::ui_theme::{
    ChromePalette, FONT_BODY, FONT_META, FONT_SUBHEAD, PillTone, apple_checkbox, badge_pill,
    hairline_h, inset_well_style, modal_body_height, modal_card_style, modal_overlay, modal_width,
    mono_font, pill_button_style, segmented_button_style, segmented_track_style, sheet_header,
    status_dot, styled_input_style, ui_font, ui_font_medium, ui_font_semibold,
};

/// Modal state for managing and executing reusable [`QuickCommand`] snippets.
#[derive(Debug, Clone, Default)]
pub struct QuickCommandsUiState {
    pub open: bool,
    pub filter: String,
    pub editing_id: Option<String>,
    pub draft_name: String,
    pub draft_group: String,
    pub draft_command: String,
    pub draft_append_newline: bool,
    pub draft_broadcast: bool,
    pub draft_confirm: bool,
    pub confirm_pending_id: Option<String>,
    pub error: Option<String>,
}

impl QuickCommandsUiState {
    pub fn open_fresh(&mut self) {
        self.open = true;
        self.error = None;
        self.confirm_pending_id = None;
        if !self.editing_id.is_some() {
            self.draft_append_newline = true;
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.confirm_pending_id = None;
        self.error = None;
    }

    pub fn load_for_edit(&mut self, cmd: &QuickCommand) {
        self.editing_id = Some(cmd.id.clone());
        self.draft_name = cmd.name.clone();
        self.draft_group = cmd.group.clone().unwrap_or_default();
        self.draft_command = cmd.command.clone();
        self.draft_append_newline = cmd.append_newline;
        self.draft_broadcast = cmd.broadcast;
        self.draft_confirm = cmd.confirm_before_run;
        self.error = None;
    }

    pub fn clear_draft(&mut self) {
        self.editing_id = None;
        self.draft_name.clear();
        self.draft_group.clear();
        self.draft_command.clear();
        self.draft_append_newline = true;
        self.draft_broadcast = false;
        self.draft_confirm = false;
        self.error = None;
    }

    pub fn build_command(&self) -> Result<QuickCommand, String> {
        let name = self.draft_name.trim().to_string();
        if name.is_empty() {
            return Err("Quick command name is required".to_string());
        }
        let command = self.draft_command.trim_end_matches(&['\r', '\n'][..]).to_string();
        if command.trim().is_empty() {
            return Err("Quick command snippet text is required".to_string());
        }
        let group = {
            let trimmed = self.draft_group.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        let mut qc = QuickCommand::new(name, command);
        if let Some(id) = &self.editing_id {
            qc.id = id.clone();
        }
        qc.group = group;
        qc.append_newline = self.draft_append_newline;
        qc.broadcast = self.draft_broadcast;
        qc.confirm_before_run = self.draft_confirm;
        Ok(qc)
    }

    pub fn view<'a>(
        &'a self,
        config: &'a QuickCommandsConfig,
        palette: &ThemePalette,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        self.view_responsive(config, palette, 1280.0, 800.0)
    }

    pub fn view_responsive<'a>(
        &'a self,
        config: &'a QuickCommandsConfig,
        palette: &ThemePalette,
        window_width: f32,
        window_height: f32,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        if !self.open {
            return None;
        }
        let chrome = ChromePalette::from_palette(palette);
        let card_w = modal_width(680.0, window_width);
        let list_h = modal_body_height(240.0, window_height, 360.0);
        let compact_qc = card_w < 540.0;
        let filter_lower = self.filter.trim().to_ascii_lowercase();

        let header = sheet_header(
            "Quick Commands",
            "Execute saved command snippets in the active pane or broadcast across tab panes",
            Some("Ctrl+Shift+Q"),
            Message::CloseQuickCommandsModal,
            chrome,
        );

        let name_group_row: Element<'a, Message, Theme, iced::Renderer> = if compact_qc {
            column![
                text_input("Snippet name", &self.draft_name)
                    .on_input(Message::QuickCommandNameChanged)
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill),
                text_input("Group (optional)", &self.draft_group)
                    .on_input(Message::QuickCommandGroupChanged)
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::Fill),
            ]
            .spacing(8)
            .into()
        } else {
            row![
                text_input("Snippet name", &self.draft_name)
                    .on_input(Message::QuickCommandNameChanged)
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::FillPortion(2)),
                text_input("Group (optional)", &self.draft_group)
                    .on_input(Message::QuickCommandGroupChanged)
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::FillPortion(1)),
            ]
            .spacing(8)
            .into()
        };

        let form = container(
            column![
                name_group_row,
                text_input("Command to run (e.g. git status -sb)", &self.draft_command)
                    .on_input(Message::QuickCommandBodyChanged)
                    .on_submit(Message::QuickCommandSave)
                    .font(mono_font())
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome)),
                row![
                    apple_checkbox(
                        self.draft_append_newline,
                        "Send Enter",
                        Message::QuickCommandAppendNewlineToggled,
                        chrome,
                    ),
                    apple_checkbox(
                        self.draft_broadcast,
                        "Broadcast by default",
                        Message::QuickCommandBroadcastToggled,
                        chrome,
                    ),
                    apple_checkbox(
                        self.draft_confirm,
                        "Confirm first",
                        Message::QuickCommandConfirmToggled,
                        chrome,
                    ),
                    if self.editing_id.is_some() {
                        Element::from(
                            button(text("Cancel Edit").size(FONT_META))
                                .padding([6, 11])
                                .style(pill_button_style(chrome, PillTone::Subtle))
                                .on_press(Message::QuickCommandCancelEdit),
                        )
                    } else {
                        space::horizontal().width(0).into()
                    },
                    button(
                        text(if self.editing_id.is_some() {
                            "Update Snippet"
                        } else {
                            "Add Snippet"
                        })
                        .size(FONT_BODY)
                        .font(ui_font_medium()),
                    )
                    .padding([6, 13])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::QuickCommandSave),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .wrap()
                .vertical_spacing(6),
            ]
            .spacing(9),
        )
        .padding(12)
        .style(move |_| inset_well_style(chrome));

        let filter_bar = text_input("Filter quick commands...", &self.filter)
            .on_input(Message::QuickCommandFilterChanged)
            .size(FONT_BODY)
            .padding([7, 10])
            .style(styled_input_style(chrome));

        let mut list = column![].spacing(4);
        let mut matched_count = 0usize;
        for cmd in &config.commands {
            let matches = filter_lower.is_empty()
                || cmd.name.to_ascii_lowercase().contains(&filter_lower)
                || cmd.command.to_ascii_lowercase().contains(&filter_lower)
                || cmd
                    .group
                    .as_deref()
                    .is_some_and(|g| g.to_ascii_lowercase().contains(&filter_lower));
            if !matches {
                continue;
            }
            if matched_count > 0 {
                list = list.push(hairline_h(chrome));
            }
            matched_count += 1;
            let cid_run = cmd.id.clone();
            let cid_bcast = cmd.id.clone();
            let cid_edit = cmd.id.clone();
            let cid_del = cmd.id.clone();

            let cmd_info = column![
                row![
                    text(cmd.name.as_str())
                        .size(FONT_BODY)
                        .font(ui_font_medium())
                        .color(chrome.text_primary),
                    if let Some(grp) = &cmd.group {
                        Element::from(badge_pill(
                            grp.as_str(),
                            chrome.accent,
                            chrome.accent_soft,
                            chrome.border_subtle,
                        ))
                    } else {
                        space::horizontal().width(0).into()
                    },
                    if cmd.broadcast {
                        Element::from(badge_pill(
                            "Broadcast",
                            chrome.warning,
                            chrome.warning_soft,
                            chrome.border_subtle,
                        ))
                    } else {
                        space::horizontal().width(0).into()
                    },
                ]
                .spacing(6)
                .align_y(Alignment::Center)
                .wrap()
                .vertical_spacing(4),
                text(cmd.command.as_str())
                    .size(FONT_META)
                    .font(mono_font())
                    .color(chrome.text_muted),
            ]
            .spacing(2);

            let actions_row = row![
                button(text("Run").size(FONT_META).font(ui_font_medium()))
                    .padding([5, 10])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::ExecuteQuickCommand {
                        id: cid_run,
                        broadcast_override: None,
                    }),
                button(text("Broadcast").size(FONT_META))
                    .padding([5, 10])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::ExecuteQuickCommand {
                        id: cid_bcast,
                        broadcast_override: Some(true),
                    }),
                button(text("Edit").size(FONT_META))
                    .padding([5, 9])
                    .style(pill_button_style(chrome, PillTone::Ghost))
                    .on_press(Message::QuickCommandEdit(cid_edit)),
                button(text("×").size(FONT_BODY))
                    .padding([5, 9])
                    .style(pill_button_style(chrome, PillTone::Danger))
                    .on_press(Message::QuickCommandDelete(cid_del)),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .wrap()
            .vertical_spacing(4);

            let row_content: Element<'a, Message, Theme, iced::Renderer> =
                if self.confirm_pending_id.as_deref() == Some(cmd.id.as_str()) {
                    let cid_confirm = cmd.id.clone();
                    row![
                        text(format!("Confirm running '{}'?", cmd.name))
                            .size(FONT_BODY)
                            .color(chrome.warning)
                            .width(Length::Fill),
                        button(text("Confirm Run").size(FONT_META).font(ui_font_medium()))
                            .padding([5, 10])
                            .style(pill_button_style(chrome, PillTone::Danger))
                            .on_press(Message::ConfirmExecuteQuickCommand(cid_confirm)),
                        button(text("Cancel").size(FONT_META))
                            .padding([5, 10])
                            .style(pill_button_style(chrome, PillTone::Subtle))
                            .on_press(Message::CancelQuickCommandConfirm),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .wrap()
                    .vertical_spacing(6)
                    .into()
                } else if compact_qc {
                    column![cmd_info, actions_row]
                        .spacing(8)
                        .width(Length::Fill)
                        .into()
                } else {
                    row![cmd_info.width(Length::Fill), actions_row]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                };

            list = list.push(container(row_content).padding([6, 8]).width(Length::Fill));
        }

        if config.commands.is_empty() {
            list = list.push(
                container(
                    text("No Quick Commands saved yet. Add a snippet above.")
                        .size(FONT_BODY)
                        .color(chrome.text_muted),
                )
                .padding([8, 8]),
            );
        } else if matched_count == 0 {
            list = list.push(
                container(
                    text("No Quick Commands match your filter.")
                        .size(FONT_BODY)
                        .color(chrome.text_muted),
                )
                .padding([8, 8]),
            );
        }

        let mut body = column![
            header,
            hairline_h(chrome),
            form,
            filter_bar,
            container(
                scrollable(
                    container(list)
                        .padding(Padding {
                            top: 0.0,
                            right: 8.0,
                            bottom: 0.0,
                            left: 0.0,
                        })
                        .width(Length::Fill),
                )
                .height(Length::Fixed(list_h)),
            )
            .padding(4)
            .style(move |_| inset_well_style(chrome)),
        ]
        .spacing(10);

        if let Some(err) = &self.error {
            body = body.push(text(err.as_str()).size(FONT_BODY).color(chrome.danger));
        }

        let card = container(body.padding(18))
            .width(Length::Fixed(card_w))
            .style(move |_| modal_card_style(chrome, true));
        Some(modal_overlay(card.into(), Some(Message::CloseQuickCommandsModal), 24.0))
    }
}

/// Modal state for saving and launching named [`WorkspaceTemplate`] layouts (`workspaces.toml`).
#[derive(Debug, Clone, Default)]
pub struct WorkspacesUiState {
    pub open: bool,
    pub draft_name: String,
    pub draft_description: String,
    pub error: Option<String>,
}

impl WorkspacesUiState {
    pub fn open_fresh(&mut self) {
        self.open = true;
        self.error = None;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.error = None;
    }

    pub fn view<'a>(
        &'a self,
        config: &'a WorkspacesConfig,
        active_tab_count: usize,
        palette: &ThemePalette,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        self.view_responsive(config, active_tab_count, palette, 1280.0, 800.0)
    }

    pub fn view_responsive<'a>(
        &'a self,
        config: &'a WorkspacesConfig,
        active_tab_count: usize,
        palette: &ThemePalette,
        window_width: f32,
        window_height: f32,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        if !self.open {
            return None;
        }
        let chrome = ChromePalette::from_palette(palette);
        let card_w = modal_width(640.0, window_width);
        let list_h = modal_body_height(240.0, window_height, 300.0);

        let header = sheet_header(
            "Named Workspaces",
            "Save multi-tab split layouts into workspaces.toml and restore anytime",
            Some("Ctrl+Shift+M"),
            Message::CloseWorkspacesModal,
            chrome,
        );

        let save_inputs: Element<'a, Message, Theme, iced::Renderer> = if card_w < 500.0 {
            column![
                text_input("Workspace name (e.g. Prod Incident)", &self.draft_name)
                    .on_input(Message::WorkspaceNameChanged)
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome)),
                text_input("Optional description", &self.draft_description)
                    .on_input(Message::WorkspaceDescChanged)
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome)),
                row![
                    space::horizontal(),
                    button(text("Save Workspace").size(FONT_BODY).font(ui_font_medium()))
                        .padding([6, 14])
                        .style(pill_button_style(chrome, PillTone::Primary))
                        .on_press(Message::SaveCurrentWorkspaceTemplate),
                ],
            ]
            .spacing(8)
            .into()
        } else {
            row![
                text_input("Workspace name (e.g. Prod Incident)", &self.draft_name)
                    .on_input(Message::WorkspaceNameChanged)
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::FillPortion(2)),
                text_input("Optional description", &self.draft_description)
                    .on_input(Message::WorkspaceDescChanged)
                    .size(FONT_BODY)
                    .padding([7, 10])
                    .style(styled_input_style(chrome))
                    .width(Length::FillPortion(3)),
                button(text("Save Workspace").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SaveCurrentWorkspaceTemplate),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into()
        };

        let save_box = container(
            column![
                text(format!("Save Active Workspace ({active_tab_count} tab(s))"))
                    .size(FONT_SUBHEAD)
                    .font(ui_font_semibold())
                    .color(chrome.text_primary),
                save_inputs,
            ]
            .spacing(8),
        )
        .padding(12)
        .style(move |_| inset_well_style(chrome));

        let mut list = column![].spacing(4);
        if config.workspaces.is_empty() {
            list = list.push(
                container(
                    text("No named workspaces saved yet.")
                        .size(FONT_BODY)
                        .color(chrome.text_muted),
                )
                .padding([8, 8]),
            );
        } else {
            let total = config.workspaces.len();
            for (idx, tpl) in config.workspaces.iter().enumerate() {
                let id_open = tpl.id.clone();
                let id_del = tpl.id.clone();
                let meta = format!(
                    "{} tab(s){}",
                    tpl.tabs.len(),
                    tpl.description
                        .as_deref()
                        .map(|d| format!(" — {d}"))
                        .unwrap_or_default()
                );
                let ws_info = column![
                    text(tpl.name.as_str())
                        .size(FONT_BODY)
                        .font(ui_font_medium())
                        .color(chrome.text_primary),
                    text(meta).size(FONT_META).color(chrome.text_muted),
                ]
                .spacing(2);
                let ws_actions = row![
                    button(text("Open").size(FONT_META).font(ui_font_medium()))
                        .padding([5, 11])
                        .style(pill_button_style(chrome, PillTone::Primary))
                        .on_press(Message::LaunchWorkspaceTemplate(id_open)),
                    button(text("Delete").size(FONT_META))
                        .padding([5, 9])
                        .style(pill_button_style(chrome, PillTone::Danger))
                        .on_press(Message::DeleteWorkspaceTemplate(id_del)),
                ]
                .spacing(8)
                .align_y(Alignment::Center);
                let ws_row: Element<'a, Message, Theme, iced::Renderer> = if card_w < 460.0 {
                    column![ws_info, row![space::horizontal(), ws_actions]]
                        .spacing(8)
                        .width(Length::Fill)
                        .into()
                } else {
                    row![ws_info.width(Length::Fill), ws_actions]
                        .spacing(8)
                        .align_y(Alignment::Center)
                        .into()
                };
                list = list.push(
                    container(ws_row)
                        .padding([6, 8])
                        .width(Length::Fill),
                );
                if idx + 1 < total {
                    list = list.push(hairline_h(chrome));
                }
            }
        }

        let mut body = column![
            header,
            hairline_h(chrome),
            save_box,
            container(
                scrollable(
                    container(list)
                        .padding(Padding {
                            top: 0.0,
                            right: 8.0,
                            bottom: 0.0,
                            left: 0.0,
                        })
                        .width(Length::Fill),
                )
                .height(Length::Fixed(list_h)),
            )
            .padding(4)
            .style(move |_| inset_well_style(chrome)),
        ]
        .spacing(10);
        if let Some(err) = &self.error {
            body = body.push(text(err.as_str()).size(FONT_BODY).color(chrome.danger));
        }

        let card = container(body.padding(18))
            .width(Length::Fixed(card_w))
            .style(move |_| modal_card_style(chrome, true));
        Some(modal_overlay(card.into(), Some(Message::CloseWorkspacesModal), 24.0))
    }
}

/// Active tab inside the Containers / Serial / Telnet quick launcher modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtocolLauncherTab {
    #[default]
    Containers,
    Serial,
    Telnet,
}

/// Modal state for discovering Docker/Podman containers, local serial ports, and launching Telnet/Serial/Docker sessions.
#[derive(Debug, Clone)]
pub struct ProtocolLauncherState {
    pub open: bool,
    pub tab: ProtocolLauncherTab,
    pub containers: Vec<ContainerInfo>,
    pub serial_ports: Vec<SerialPortInfo>,
    pub serial_port_input: String,
    pub serial_baud_input: String,
    pub telnet_host_input: String,
    pub telnet_port_input: String,
    pub telnet_auto_reconnect: bool,
    pub error: Option<String>,
}

impl Default for ProtocolLauncherState {
    fn default() -> Self {
        Self {
            open: false,
            tab: ProtocolLauncherTab::Containers,
            containers: Vec::new(),
            serial_ports: Vec::new(),
            serial_port_input: String::new(),
            serial_baud_input: "115200".to_string(),
            telnet_host_input: String::new(),
            telnet_port_input: "23".to_string(),
            telnet_auto_reconnect: false,
            error: None,
        }
    }
}

impl ProtocolLauncherState {
    pub fn view<'a>(&'a self, palette: &ThemePalette) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        self.view_responsive(palette, 1280.0, 800.0)
    }

    pub fn view_responsive<'a>(
        &'a self,
        palette: &ThemePalette,
        window_width: f32,
        window_height: f32,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        if !self.open {
            return None;
        }
        let chrome = ChromePalette::from_palette(palette);
        let card_w = modal_width(620.0, window_width);
        let list_h = modal_body_height(240.0, window_height, 270.0);
        let compact_proto = card_w < 460.0;

        let header = sheet_header(
            "Protocols & Containers",
            "Attach to running Docker/Podman containers, USB serial ports, or Telnet hosts",
            Some("Esc"),
            Message::CloseProtocolLauncherModal,
            chrome,
        );

        let tabs_row = row![
            container(
                row![
                    tab_pill("Docker / Podman", ProtocolLauncherTab::Containers, self.tab, chrome),
                    tab_pill("Serial Port", ProtocolLauncherTab::Serial, self.tab, chrome),
                    tab_pill("Telnet", ProtocolLauncherTab::Telnet, self.tab, chrome),
                ]
                .spacing(3)
                .wrap()
                .vertical_spacing(4),
            )
            .padding(3)
            .style(move |_| segmented_track_style(chrome)),
            space::horizontal(),
            button(text("Refresh").size(FONT_META).font(ui_font_medium()))
                .padding([6, 12])
                .style(pill_button_style(chrome, PillTone::Subtle))
                .on_press(Message::RefreshProtocolDiscovery),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .wrap()
        .vertical_spacing(6);

        let body_section: Element<'a, Message, Theme, iced::Renderer> = match self.tab {
            ProtocolLauncherTab::Containers => {
                let mut col = column![].spacing(4);
                if self.containers.is_empty() {
                    col = col.push(
                        container(
                            text("No running Docker or Podman containers found. Click Refresh to re-scan.")
                                .size(FONT_BODY)
                                .color(chrome.text_muted),
                        )
                        .padding([8, 8]),
                    );
                } else {
                    let total = self.containers.len();
                    for (idx, c) in self.containers.iter().enumerate() {
                        let runtime_badge = match c.runtime {
                            DockerRuntime::Docker => "Docker",
                            DockerRuntime::Podman => "Podman",
                        };
                        let rt = c.runtime;
                        let cname = c.name.clone();
                        let info = row![
                            status_dot(chrome.success),
                            badge_pill(runtime_badge, chrome.accent, chrome.accent_soft, chrome.border_subtle),
                            column![
                                text(c.name.as_str())
                                    .size(FONT_BODY)
                                    .font(ui_font_medium())
                                    .color(chrome.text_primary),
                                text(format!("{} — {}", c.image, c.status))
                                    .size(FONT_META)
                                    .font(mono_font())
                                    .color(chrome.text_muted),
                            ]
                            .spacing(2)
                            .width(Length::Fill),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center);
                        let exec_btn = button(text("Exec Shell").size(FONT_META).font(ui_font_medium()))
                            .padding([5, 11])
                            .style(pill_button_style(chrome, PillTone::Primary))
                            .on_press(Message::LaunchDiscoveredContainer {
                                runtime: rt,
                                container: cname,
                            });
                        let item_el: Element<'a, Message, Theme, iced::Renderer> = if compact_proto {
                            column![info, row![space::horizontal(), exec_btn]]
                                .spacing(6)
                                .into()
                        } else {
                            row![info.width(Length::Fill), exec_btn]
                                .spacing(8)
                                .align_y(Alignment::Center)
                                .into()
                        };
                        col = col.push(
                            container(item_el)
                                .padding([6, 8])
                                .width(Length::Fill),
                        );
                        if idx + 1 < total {
                            col = col.push(hairline_h(chrome));
                        }
                    }
                }
                container(scrollable(col).height(Length::Fixed(list_h)))
                    .padding(4)
                    .style(move |_| inset_well_style(chrome))
                    .into()
            }
            ProtocolLauncherTab::Serial => {
                let serial_connect_row: Element<'a, Message, Theme, iced::Renderer> = if compact_proto {
                    column![
                        text_input("/dev/ttyUSB0 or COM3", &self.serial_port_input)
                            .on_input(Message::ProtocolSerialPortChanged)
                            .font(mono_font())
                            .size(FONT_BODY)
                            .padding([7, 10])
                            .style(styled_input_style(chrome))
                            .width(Length::Fill),
                        row![
                            text_input("115200", &self.serial_baud_input)
                                .on_input(Message::ProtocolSerialBaudChanged)
                                .font(mono_font())
                                .size(FONT_BODY)
                                .padding([7, 10])
                                .style(styled_input_style(chrome))
                                .width(Length::Fill),
                            button(text("Connect").size(FONT_BODY).font(ui_font_medium()))
                                .padding([7, 13])
                                .style(pill_button_style(chrome, PillTone::Primary))
                                .on_press(Message::LaunchQuickSerial),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    ]
                    .spacing(8)
                    .into()
                } else {
                    row![
                        text_input("/dev/ttyUSB0 or COM3", &self.serial_port_input)
                            .on_input(Message::ProtocolSerialPortChanged)
                            .font(mono_font())
                            .size(FONT_BODY)
                            .padding([7, 10])
                            .style(styled_input_style(chrome))
                            .width(Length::Fill),
                        text_input("115200", &self.serial_baud_input)
                            .on_input(Message::ProtocolSerialBaudChanged)
                            .font(mono_font())
                            .size(FONT_BODY)
                            .padding([7, 10])
                            .style(styled_input_style(chrome))
                            .width(Length::Fixed(96.0)),
                        button(text("Connect").size(FONT_BODY).font(ui_font_medium()))
                            .padding([7, 13])
                            .style(pill_button_style(chrome, PillTone::Primary))
                            .on_press(Message::LaunchQuickSerial),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .into()
                };
                let mut col = column![
                    serial_connect_row,
                    text("Detected Local Ports")
                        .size(FONT_SUBHEAD)
                        .font(ui_font_semibold())
                        .color(chrome.text_secondary),
                ]
                .spacing(8);
                if self.serial_ports.is_empty() {
                    col = col.push(
                        text("No serial devices detected; enter a path manually above.")
                            .size(FONT_BODY)
                            .color(chrome.text_muted),
                    );
                } else {
                    for p in &self.serial_ports {
                        let path = p.path.clone();
                        col = col.push(
                            button(
                                row![
                                    text(p.display_name.as_str()).size(FONT_BODY).color(chrome.text_primary),
                                    space::horizontal(),
                                    text(p.path.as_str()).size(FONT_META).font(mono_font()).color(chrome.text_muted),
                                ]
                                .align_y(Alignment::Center)
                                .wrap()
                                .vertical_spacing(4),
                            )
                            .padding([7, 10])
                            .width(Length::Fill)
                            .style(pill_button_style(chrome, PillTone::Subtle))
                            .on_press(Message::ProtocolSerialPortChanged(path)),
                        );
                    }
                }
                container(scrollable(col).height(Length::Fixed(list_h)))
                    .padding(10)
                    .style(move |_| inset_well_style(chrome))
                    .into()
            }
            ProtocolLauncherTab::Telnet => container(
                column![
                    row![
                        text_input("Host (e.g. 192.168.1.1)", &self.telnet_host_input)
                            .on_input(Message::ProtocolTelnetHostChanged)
                            .font(mono_font())
                            .size(FONT_BODY)
                            .padding([7, 10])
                            .style(styled_input_style(chrome))
                            .width(Length::Fill),
                        text_input("23", &self.telnet_port_input)
                            .on_input(Message::ProtocolTelnetPortChanged)
                            .font(mono_font())
                            .size(FONT_BODY)
                            .padding([7, 10])
                            .style(styled_input_style(chrome))
                            .width(Length::Fixed(76.0)),
                    ]
                    .spacing(8),
                    hairline_h(chrome),
                    row![
                        apple_checkbox(
                            self.telnet_auto_reconnect,
                            "Auto-reconnect on disconnect",
                            Message::ProtocolTelnetAutoReconnectToggled,
                            chrome,
                        ),
                        space::horizontal(),
                        button(text("Connect Telnet").size(FONT_BODY).font(ui_font_medium()))
                            .padding([7, 14])
                            .style(pill_button_style(chrome, PillTone::Primary))
                            .on_press(Message::LaunchQuickTelnet),
                    ]
                    .align_y(Alignment::Center)
                    .wrap()
                    .vertical_spacing(6),
                ]
                .spacing(10),
            )
            .padding(12)
            .width(Length::Fill)
            .style(move |_| inset_well_style(chrome))
            .into(),
        };

        let mut body = column![header, hairline_h(chrome), tabs_row, body_section].spacing(10);
        if let Some(err) = &self.error {
            body = body.push(text(err.as_str()).size(FONT_BODY).color(chrome.danger));
        }

        let card = container(body.padding(18))
            .width(Length::Fixed(card_w))
            .style(move |_| modal_card_style(chrome, true));
        Some(modal_overlay(card.into(), Some(Message::CloseProtocolLauncherModal), 24.0))
    }
}

fn tab_pill<'a>(
    label: &'static str,
    tab: ProtocolLauncherTab,
    current: ProtocolLauncherTab,
    chrome: ChromePalette,
) -> Element<'a, Message, Theme, iced::Renderer> {
    let active = tab == current;
    button(
        text(label)
            .size(12.0)
            .font(if active { ui_font_medium() } else { ui_font() }),
    )
    .padding([5, 12])
    .style(segmented_button_style(chrome, active, true))
    .on_press(Message::ProtocolLauncherSelectTab(tab))
    .into()
}

/// Modal prompting local file path to send via Zmodem (`rz`/`sz`).
#[derive(Debug, Clone, Default)]
pub struct ZmodemSendUiState {
    pub session_id: Option<SessionId>,
    pub file_path_draft: String,
    pub error: Option<String>,
}

impl ZmodemSendUiState {
    pub fn open_for(&mut self, session_id: SessionId) {
        self.session_id = Some(session_id);
        self.error = None;
    }

    pub fn close(&mut self) {
        self.session_id = None;
        self.error = None;
    }

    pub fn view<'a>(&'a self, palette: &ThemePalette) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        self.view_responsive(palette, 1280.0)
    }

    pub fn view_responsive<'a>(
        &'a self,
        palette: &ThemePalette,
        window_width: f32,
    ) -> Option<Element<'a, Message, Theme, iced::Renderer>> {
        let _sid = self.session_id?;
        let chrome = ChromePalette::from_palette(palette);
        let card_w = modal_width(480.0, window_width);
        let header = sheet_header(
            "Zmodem Send File",
            "Enter one or more local file paths (comma-separated) to transmit via Zmodem",
            Some("Esc"),
            Message::CloseZmodemSendModal,
            chrome,
        );
        let mut content = column![
            header,
            hairline_h(chrome),
            text_input("/path/to/archive.tar.gz", &self.file_path_draft)
                .on_input(Message::ZmodemPathInputChanged)
                .on_submit(Message::SubmitZmodemSend)
                .font(mono_font())
                .size(FONT_BODY)
                .padding([7, 10])
                .style(styled_input_style(chrome)),
            row![
                space::horizontal(),
                button(text("Cancel").size(FONT_BODY))
                    .padding([6, 13])
                    .style(pill_button_style(chrome, PillTone::Subtle))
                    .on_press(Message::CloseZmodemSendModal),
                button(text("Send via Zmodem").size(FONT_BODY).font(ui_font_medium()))
                    .padding([6, 14])
                    .style(pill_button_style(chrome, PillTone::Primary))
                    .on_press(Message::SubmitZmodemSend),
            ]
            .spacing(8),
        ]
        .spacing(10);
        if let Some(err) = &self.error {
            content = content.push(text(err.as_str()).size(FONT_BODY).color(chrome.danger));
        }
        let card = container(content.padding(18))
            .width(Length::Fixed(card_w))
            .style(move |_| modal_card_style(chrome, true));
        Some(modal_overlay(card.into(), Some(Message::CloseZmodemSendModal), 24.0))
    }
}
