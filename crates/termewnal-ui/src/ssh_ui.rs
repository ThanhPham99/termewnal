use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use iced::futures::SinkExt;
use iced::widget::{button, column, container, row, space, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Task, Theme};
use termewnal_core::{Profile, VaultSession};
use termewnal_pty::TerminalBuffer;
use termewnal_ssh::{
    AgentConnector, BoxFuture, HostKeyDecision, HostKeyPrompt, InteractivePromptItem,
    KeyboardInteractiveChallenge, OsAgentConnector, SshConnectConfig, SshError,
    SshInteractionHandler, SshSession, SshTransport,
};
use tokio::sync::{mpsc, oneshot};
use zeroize::{Zeroize, Zeroizing};

use crate::app::{Message, SessionId};

/// Shared oneshot responder resolving a TOFU unknown-host prompt.
pub type HostKeyResponder = Arc<Mutex<Option<oneshot::Sender<HostKeyDecision>>>>;

/// Shared oneshot responder resolving interactive passphrase, password, or 2FA/OTP prompts.
pub type AuthPromptResponder = Arc<Mutex<Option<oneshot::Sender<Option<Vec<Zeroizing<String>>>>>>>;

/// Target metadata and connection configuration for opening or restoring an SSH `Session`.
#[derive(Clone)]
pub struct SshTargetSpec {
    pub profile_id: Option<String>,
    pub title: String,
    pub connect_config: SshConnectConfig,
    pub agent_connector: Arc<dyn AgentConnector>,
    pub auto_connect_on_startup: bool,
}

impl fmt::Debug for SshTargetSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshTargetSpec")
            .field("profile_id", &self.profile_id)
            .field("title", &self.title)
            .field("host", &self.connect_config.host)
            .field("port", &self.connect_config.port)
            .field("username", &self.connect_config.username)
            .field("auto_connect_on_startup", &self.auto_connect_on_startup)
            .finish()
    }
}

impl SshTargetSpec {
    /// Builds an [`SshTargetSpec`] from a saved SSH [`Profile`].
    pub fn from_profile(
        profile: &Profile,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
    ) -> Result<Self, SshError> {
        let connect_config =
            SshConnectConfig::from_profile(profile, cols, rows, scrollback_lines)?;
        Ok(Self {
            profile_id: Some(profile.id.clone()),
            title: profile.name.clone(),
            connect_config,
            agent_connector: Arc::new(OsAgentConnector::default()),
            auto_connect_on_startup: profile.auto_connect_on_startup(),
        })
    }

    /// Creates a Quick Connect [`SshTargetSpec`] from explicit [`SshConnectConfig`].
    pub fn quick_connect(title: impl Into<String>, connect_config: SshConnectConfig) -> Self {
        Self {
            profile_id: None,
            title: title.into(),
            connect_config,
            agent_connector: Arc::new(OsAgentConnector::default()),
            auto_connect_on_startup: false,
        }
    }

    /// Overrides the `known_hosts` file path (primarily for isolated tests).
    pub fn with_known_hosts_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.connect_config.known_hosts_path = Some(path.into());
        self
    }

    /// Overrides the [`AgentConnector`] implementation (primarily for isolated tests).
    pub fn with_agent_connector(mut self, connector: Arc<dyn AgentConnector>) -> Self {
        self.agent_connector = connector;
        self
    }
}

/// Interactive SSH authentication prompt displayed to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshAuthPrompt {
    Passphrase { key_path: PathBuf },
    Password { username: String, host: String },
    KeyboardInteractive(KeyboardInteractiveChallenge),
}

impl SshAuthPrompt {
    fn prompt_items(&self) -> Vec<InteractivePromptItem> {
        match self {
            Self::Passphrase { key_path } => vec![InteractivePromptItem {
                prompt: format!("Passphrase for {}: ", key_path.display()),
                echo: false,
            }],
            Self::Password { username, host } => vec![InteractivePromptItem {
                prompt: format!("Password for {username}@{host}: "),
                echo: false,
            }],
            Self::KeyboardInteractive(challenge) => challenge.prompts.clone(),
        }
    }
}

/// Explicit MITM hard-block alert surfaced when a server's host key conflicts with `known_hosts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MitmAlert {
    pub session_id: SessionId,
    pub host: String,
    pub port: u16,
    pub line: usize,
    pub key_type: String,
    pub expected_fingerprint: String,
    pub received_fingerprint: String,
}

/// Asynchronous SSH connection and interaction events delivered to [`AppState::update`].
#[derive(Clone)]
pub enum SshUiEvent {
    Connected {
        generation: u64,
        session: Arc<SshSession>,
    },
    HostKeyPrompt {
        generation: u64,
        prompt: HostKeyPrompt,
        responder: HostKeyResponder,
    },
    AuthPrompt {
        generation: u64,
        prompt: SshAuthPrompt,
        responder: AuthPromptResponder,
    },
    MitmBlocked {
        generation: u64,
        alert: MitmAlert,
    },
    Disconnected {
        generation: u64,
        error: Option<String>,
    },
}

impl fmt::Debug for SshUiEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected { generation, .. } => f
                .debug_struct("Connected")
                .field("generation", generation)
                .finish(),
            Self::HostKeyPrompt {
                generation, prompt, ..
            } => f
                .debug_struct("HostKeyPrompt")
                .field("generation", generation)
                .field("prompt", prompt)
                .finish(),
            Self::AuthPrompt {
                generation, prompt, ..
            } => f
                .debug_struct("AuthPrompt")
                .field("generation", generation)
                .field("prompt", prompt)
                .finish(),
            Self::MitmBlocked { generation, alert } => f
                .debug_struct("MitmBlocked")
                .field("generation", generation)
                .field("alert", alert)
                .finish(),
            Self::Disconnected { generation, error } => f
                .debug_struct("Disconnected")
                .field("generation", generation)
                .field("error", error)
                .finish(),
        }
    }
}

struct PendingHostKeyState {
    session_id: SessionId,
    prompt: HostKeyPrompt,
    responder: HostKeyResponder,
}

struct PendingAuthState {
    session_id: SessionId,
    prompt: SshAuthPrompt,
    items: Vec<InteractivePromptItem>,
    drafts: Vec<Zeroizing<String>>,
    responder: AuthPromptResponder,
}

impl Drop for PendingAuthState {
    fn drop(&mut self) {
        for draft in &mut self.drafts {
            draft.zeroize();
        }
    }
}

/// Manages active TOFU modals, MITM hard-block alert banners, and 2FA/passphrase prompts.
#[derive(Default)]
pub struct SshUiState {
    host_key_queue: VecDeque<PendingHostKeyState>,
    auth_queue: VecDeque<PendingAuthState>,
    mitm_alerts: BTreeMap<SessionId, MitmAlert>,
}

impl SshUiState {
    /// Returns the currently visible unknown-host TOFU prompt, if any.
    pub fn active_host_key_prompt(&self) -> Option<&HostKeyPrompt> {
        self.host_key_queue.front().map(|entry| &entry.prompt)
    }

    /// Returns the `SessionId` owning the currently visible TOFU prompt.
    pub fn active_host_key_session(&self) -> Option<SessionId> {
        self.host_key_queue.front().map(|entry| entry.session_id)
    }

    /// Returns the currently visible interactive auth prompt, if any.
    pub fn active_auth_prompt(&self) -> Option<&SshAuthPrompt> {
        self.auth_queue.front().map(|entry| &entry.prompt)
    }

    /// Returns the `SessionId` owning the currently visible interactive auth prompt.
    pub fn active_auth_session(&self) -> Option<SessionId> {
        self.auth_queue.front().map(|entry| entry.session_id)
    }

    /// Returns the active MITM hard-block alert for `session_id`, if present.
    pub fn mitm_alert(&self, session_id: SessionId) -> Option<&MitmAlert> {
        self.mitm_alerts.get(&session_id)
    }

    /// Returns all active MITM hard-block alerts keyed by `SessionId`.
    pub fn mitm_alerts(&self) -> &BTreeMap<SessionId, MitmAlert> {
        &self.mitm_alerts
    }

    /// Queues an unknown-host TOFU verification prompt for `session_id`.
    pub fn push_host_key_prompt(
        &mut self,
        session_id: SessionId,
        prompt: HostKeyPrompt,
        responder: HostKeyResponder,
    ) {
        self.host_key_queue.push_back(PendingHostKeyState {
            session_id,
            prompt,
            responder,
        });
    }

    /// Resolves the active TOFU prompt for `session_id` with `accept` (`true` => Trust & Save).
    pub fn respond_host_key(&mut self, session_id: SessionId, accept: bool) {
        if let Some(pos) = self
            .host_key_queue
            .iter()
            .position(|entry| entry.session_id == session_id)
            && let Some(entry) = self.host_key_queue.remove(pos)
        {
            let decision = if accept {
                HostKeyDecision::AcceptAndSave
            } else {
                HostKeyDecision::Reject
            };
            if let Ok(mut guard) = entry.responder.lock()
                && let Some(tx) = guard.take()
            {
                let _ = tx.send(decision);
            }
        }
    }

    /// Queues an interactive auth prompt (2FA/OTP, passphrase, or password) for `session_id`.
    pub fn push_auth_prompt(
        &mut self,
        session_id: SessionId,
        prompt: SshAuthPrompt,
        responder: AuthPromptResponder,
    ) {
        let items = prompt.prompt_items();
        let drafts = vec![Zeroizing::new(String::new()); items.len()];
        self.auth_queue.push_back(PendingAuthState {
            session_id,
            prompt,
            items,
            drafts,
            responder,
        });
    }

    /// Updates input draft `index` on the active auth prompt for `session_id`.
    pub fn set_auth_draft(&mut self, session_id: SessionId, index: usize, value: String) {
        if let Some(entry) = self
            .auth_queue
            .iter_mut()
            .find(|entry| entry.session_id == session_id)
            && let Some(slot) = entry.drafts.get_mut(index)
        {
            slot.zeroize();
            **slot = value;
        }
    }

    /// Submits the entered auth answers for `session_id` and zeroizes the UI draft buffers.
    pub fn submit_auth_prompt(&mut self, session_id: SessionId) {
        if let Some(pos) = self
            .auth_queue
            .iter()
            .position(|entry| entry.session_id == session_id)
            && let Some(mut entry) = self.auth_queue.remove(pos)
        {
            let answers = std::mem::take(&mut entry.drafts);
            if let Ok(mut guard) = entry.responder.lock()
                && let Some(tx) = guard.take()
            {
                let _ = tx.send(Some(answers));
            }
        }
    }

    /// Cancels the interactive auth prompt for `session_id`, aborting authentication cleanly.
    pub fn cancel_auth_prompt(&mut self, session_id: SessionId) {
        if let Some(pos) = self
            .auth_queue
            .iter()
            .position(|entry| entry.session_id == session_id)
            && let Some(entry) = self.auth_queue.remove(pos)
            && let Ok(mut guard) = entry.responder.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(None);
        }
    }

    /// Records a MITM host-key mismatch alert for `alert.session_id`.
    pub fn record_mitm_alert(&mut self, alert: MitmAlert) {
        self.clear_session_prompts(alert.session_id);
        self.mitm_alerts.insert(alert.session_id, alert);
    }

    /// Dismisses the MITM alert banner for `session_id`.
    pub fn dismiss_mitm_alert(&mut self, session_id: SessionId) {
        self.mitm_alerts.remove(&session_id);
    }

    /// Clears and cancels any pending TOFU/auth prompts and MITM alerts owned by `session_id`.
    pub fn clear_session_prompts(&mut self, session_id: SessionId) {
        self.mitm_alerts.remove(&session_id);
        let mut idx = 0;
        while idx < self.host_key_queue.len() {
            if self.host_key_queue[idx].session_id == session_id {
                self.respond_host_key(session_id, false);
            } else {
                idx += 1;
            }
        }
        let mut auth_idx = 0;
        while auth_idx < self.auth_queue.len() {
            if self.auth_queue[auth_idx].session_id == session_id {
                self.cancel_auth_prompt(session_id);
            } else {
                auth_idx += 1;
            }
        }
    }

    /// Renders persistent red MITM hard-block alert banners.
    pub fn view_mitm_alerts(&self) -> Option<Element<'_, Message, Theme, iced::Renderer>> {
        if self.mitm_alerts.is_empty() {
            return None;
        }

        let mut list = column![].spacing(6).width(Length::Fill);
        for alert in self.mitm_alerts.values() {
            let summary = format!(
                "HOST KEY VERIFICATION FAILED (POSSIBLE MITM ATTACK) for {}:{} (known_hosts line {}, {}) — Expected {}, Received {}",
                alert.host,
                alert.port,
                alert.line,
                alert.key_type,
                alert.expected_fingerprint,
                alert.received_fingerprint
            );
            let banner = container(
                row![
                    text(summary).size(13).color(Color::WHITE),
                    space::horizontal(),
                    button(text("Dismiss").size(12))
                        .padding([4, 10])
                        .style(button::secondary)
                        .on_press(Message::DismissMitmAlert(alert.session_id)),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding([8, 12])
            .style(mitm_banner_style);
            list = list.push(banner);
        }

        Some(list.into())
    }

    /// Renders the interactive Trust On First Use (TOFU) host key confirmation panel.
    pub fn view_tofu_modal(&self) -> Option<Element<'_, Message, Theme, iced::Renderer>> {
        let entry = self.host_key_queue.front()?;
        let prompt = &entry.prompt;
        let sid = entry.session_id;

        let card = container(
            column![
                text(format!(
                    "Unknown SSH Host Key: {}:{} ({})",
                    prompt.host, prompt.port, prompt.key_type
                ))
                .size(14),
                text(format!("SHA256 Fingerprint: {}", prompt.fingerprint_sha256)).size(13),
                row![
                    button(text("Trust & Save to known_hosts").size(13))
                        .padding([6, 12])
                        .style(button::primary)
                        .on_press(Message::RespondHostKeyPrompt {
                            session_id: sid,
                            accept: true,
                        }),
                    button(text("Reject").size(13))
                        .padding([6, 12])
                        .style(button::danger)
                        .on_press(Message::RespondHostKeyPrompt {
                            session_id: sid,
                            accept: false,
                        }),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            ]
            .spacing(8),
        )
        .width(Length::Fill)
        .padding([10, 14])
        .style(prompt_panel_style);

        Some(card.into())
    }

    /// Renders the active interactive passphrase, password, or 2FA/OTP prompt bar.
    pub fn view_auth_prompt(&self) -> Option<Element<'_, Message, Theme, iced::Renderer>> {
        let entry = self.auth_queue.front()?;
        let sid = entry.session_id;

        let header = match &entry.prompt {
            SshAuthPrompt::Passphrase { key_path } => {
                format!("Private Key Passphrase Required ({})", key_path.display())
            }
            SshAuthPrompt::Password { username, host } => {
                format!("SSH Password Required ({username}@{host})")
            }
            SshAuthPrompt::KeyboardInteractive(challenge) => {
                if challenge.instructions.is_empty() {
                    if challenge.name.is_empty() {
                        "SSH Interactive Verification".to_string()
                    } else {
                        challenge.name.clone()
                    }
                } else if challenge.name.is_empty() {
                    challenge.instructions.clone()
                } else {
                    format!("{} — {}", challenge.name, challenge.instructions)
                }
            }
        };

        let mut col = column![text(header).size(14)].spacing(6);
        for (idx, item) in entry.items.iter().enumerate() {
            let val = entry
                .drafts
                .get(idx)
                .map(|z| z.as_str())
                .unwrap_or_default();
            let input = text_input(item.prompt.trim(), val)
                .secure(!item.echo)
                .width(Length::Fixed(280.0))
                .on_input(move |v| Message::SshAuthInputChanged {
                    session_id: sid,
                    index: idx,
                    value: v,
                })
                .on_submit(Message::SubmitSshAuthPrompt(sid));
            col = col.push(
                row![text(item.prompt.as_str()).size(13), input]
                    .spacing(8)
                    .align_y(Alignment::Center),
            );
        }

        col = col.push(
            row![
                button(text("Submit").size(13))
                    .padding([5, 12])
                    .style(button::primary)
                    .on_press(Message::SubmitSshAuthPrompt(sid)),
                button(text("Cancel").size(13))
                    .padding([5, 12])
                    .style(button::secondary)
                    .on_press(Message::CancelSshAuthPrompt(sid)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );

        Some(
            container(col)
                .width(Length::Fill)
                .padding([10, 14])
                .style(prompt_panel_style)
                .into(),
        )
    }

    /// Renders a compact top action bar inside a disconnected SSH pane allowing click or Enter to connect.
    pub fn view_disconnected_ssh_bar<'a>(
        session_id: SessionId,
        host: &str,
        port: u16,
        blocked_by_mitm: bool,
        last_error: Option<&str>,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        let copy = if blocked_by_mitm {
            format!("SSH blocked for {host}:{port} — host key mismatch detected (see MITM alert)")
        } else if let Some(err) = last_error {
            format!("SSH Disconnected ({host}:{port}) — {err} — Press Enter or click Connect")
        } else {
            format!("SSH Disconnected ({host}:{port}) — Press Enter or click Connect")
        };

        let mut bar = row![text(copy).size(12), space::horizontal()]
            .spacing(8)
            .align_y(Alignment::Center);

        if !blocked_by_mitm {
            bar = bar.push(
                button(text("Connect").size(12))
                    .padding([3, 10])
                    .style(button::primary)
                    .on_press(Message::ConnectSshSession(session_id)),
            );
        }

        container(bar)
            .width(Length::Fill)
            .padding([5, 10])
            .style(disconnected_bar_style)
            .into()
    }
}

struct UiSshInteractionHandler {
    generation: u64,
    events_tx: mpsc::UnboundedSender<SshUiEvent>,
}

impl SshInteractionHandler for UiSshInteractionHandler {
    fn verify_unknown_host(&self, prompt: HostKeyPrompt) -> BoxFuture<'_, HostKeyDecision> {
        let generation = self.generation;
        let tx = self.events_tx.clone();
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            let responder = Arc::new(Mutex::new(Some(reply_tx)));
            if tx
                .send(SshUiEvent::HostKeyPrompt {
                    generation,
                    prompt,
                    responder,
                })
                .is_err()
            {
                return HostKeyDecision::Reject;
            }
            reply_rx.await.unwrap_or(HostKeyDecision::Reject)
        })
    }

    fn prompt_key_passphrase(&self, key_path: &Path) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        let generation = self.generation;
        let tx = self.events_tx.clone();
        let path = key_path.to_path_buf();
        Box::pin(async move {
            let mut answers = request_auth_inputs(
                &tx,
                generation,
                SshAuthPrompt::Passphrase { key_path: path },
            )
            .await?;
            answers.drain(..).next()
        })
    }

    fn prompt_password(
        &self,
        username: &str,
        host: &str,
    ) -> BoxFuture<'_, Option<Zeroizing<String>>> {
        let generation = self.generation;
        let tx = self.events_tx.clone();
        let user = username.to_string();
        let host = host.to_string();
        Box::pin(async move {
            let mut answers = request_auth_inputs(
                &tx,
                generation,
                SshAuthPrompt::Password {
                    username: user,
                    host,
                },
            )
            .await?;
            answers.drain(..).next()
        })
    }

    fn prompt_keyboard_interactive(
        &self,
        challenge: KeyboardInteractiveChallenge,
    ) -> BoxFuture<'_, Option<Vec<Zeroizing<String>>>> {
        let generation = self.generation;
        let tx = self.events_tx.clone();
        Box::pin(async move {
            request_auth_inputs(&tx, generation, SshAuthPrompt::KeyboardInteractive(challenge))
                .await
        })
    }
}

async fn request_auth_inputs(
    events_tx: &mpsc::UnboundedSender<SshUiEvent>,
    generation: u64,
    prompt: SshAuthPrompt,
) -> Option<Vec<Zeroizing<String>>> {
    let (reply_tx, reply_rx) = oneshot::channel();
    let responder = Arc::new(Mutex::new(Some(reply_tx)));
    events_tx
        .send(SshUiEvent::AuthPrompt {
            generation,
            prompt,
            responder,
        })
        .ok()?;
    reply_rx.await.ok().flatten()
}

/// Spawns an `iced::Task` that connects an SSH `Session`, wiring TOFU/2FA/MITM prompts back to
/// `Message::SshEvent`.
pub fn connect_ssh_task(
    session_id: SessionId,
    generation: u64,
    spec: SshTargetSpec,
    buffer: Arc<TerminalBuffer>,
    vault: Arc<VaultSession>,
) -> Task<Message> {
    Task::stream(iced::stream::channel(32, async move |mut output| {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(drive_ssh_connect(
            session_id,
            generation,
            spec,
            buffer,
            vault,
            tx,
        ));

        while let Some(event) = rx.recv().await {
            if output
                .send(Message::SshEvent { session_id, event })
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = driver.await;
    }))
}

/// Drives an SSH connection attempt against `spec` and emits [`SshUiEvent`]s on `events_tx`.
pub async fn drive_ssh_connect(
    session_id: SessionId,
    generation: u64,
    spec: SshTargetSpec,
    buffer: Arc<TerminalBuffer>,
    vault: Arc<VaultSession>,
    events_tx: mpsc::UnboundedSender<SshUiEvent>,
) {
    let interaction: Arc<dyn SshInteractionHandler> = Arc::new(UiSshInteractionHandler {
        generation,
        events_tx: events_tx.clone(),
    });

    let cfg = &spec.connect_config;
    let transport = match SshTransport::connect(
        cfg,
        Some(vault.as_ref()),
        interaction,
        Arc::clone(&spec.agent_connector),
    )
    .await
    {
        Ok(transport) => transport,
        Err(SshError::HostKeyMismatch {
            host,
            port,
            line,
            key_type,
            expected_fingerprint,
            received_fingerprint,
        }) => {
            let _ = events_tx.send(SshUiEvent::MitmBlocked {
                generation,
                alert: MitmAlert {
                    session_id,
                    host,
                    port,
                    line,
                    key_type,
                    expected_fingerprint,
                    received_fingerprint,
                },
            });
            return;
        }
        Err(err) => {
            let _ = events_tx.send(SshUiEvent::Disconnected {
                generation,
                error: Some(err.to_string()),
            });
            return;
        }
    };

    match SshSession::open_on_buffer(
        transport,
        buffer,
        cfg.cols,
        cfg.rows,
        &cfg.term_name,
    )
    .await
    {
        Ok(session) => {
            let _ = events_tx.send(SshUiEvent::Connected {
                generation,
                session: Arc::new(session),
            });
        }
        Err(err) => {
            let _ = events_tx.send(SshUiEvent::Disconnected {
                generation,
                error: Some(err.to_string()),
            });
        }
    }
}

fn mitm_banner_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(153, 27, 27))),
        border: Border {
            color: Color::from_rgb8(248, 113, 113),
            width: 1.0,
            radius: 0.0.into(),
        },
        text_color: Some(Color::WHITE),
        ..Default::default()
    }
}

fn prompt_panel_style(_theme: &Theme) -> container::Style {
    container::Style::default()
        .background(Color::from_rgb8(0x28, 0x30, 0x3d))
        .color(Color::from_rgb8(0xe5, 0xe9, 0xf0))
        .border(Border {
            color: Color::from_rgb8(0x5e, 0x81, 0xac),
            width: 1.0,
            radius: 4.0.into(),
        })
}

fn disconnected_bar_style(_theme: &Theme) -> container::Style {
    container::Style::default()
        .background(Color::from_rgb8(0x2e, 0x34, 0x40))
        .color(Color::from_rgb8(0xd8, 0xde, 0xe9))
        .border(Border {
            color: Color::from_rgb8(0x4c, 0x56, 0x6a),
            width: 1.0,
            radius: 0.0.into(),
        })
}
