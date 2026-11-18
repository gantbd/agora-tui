//! Conversation-native, exact-confirmed room creation.

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

use agora_client::{
    room_command::{
        CreateRoomErrorV1, CreateRoomRequestV1, create_room, required_create_confirmation,
    },
    v2::{ClientErrorV2, ClientV2, ServicePortV2},
};
use agora_protocol::v2::AdmissionPolicyV2;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{ControlHandle, ControlResult, Lifecycle};

pub const MAX_ROOM_ID_BYTES: usize = 128;
pub const MAX_ROOM_NAME_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateRoomInput {
    Character(char),
    Backspace,
    NextField,
    PreviousField,
    ToggleAdmission,
    Arm,
    Confirm,
    RetryExact,
    Close,
}

pub fn translate_create_room_key(key: KeyEvent) -> Option<CreateRoomInput> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Esc) => Some(CreateRoomInput::Close),
        (KeyModifiers::NONE, KeyCode::Backspace) => Some(CreateRoomInput::Backspace),
        (KeyModifiers::NONE, KeyCode::Tab) => Some(CreateRoomInput::NextField),
        (KeyModifiers::SHIFT, KeyCode::BackTab) => Some(CreateRoomInput::PreviousField),
        (KeyModifiers::NONE, KeyCode::Left | KeyCode::Right) => {
            Some(CreateRoomInput::ToggleAdmission)
        }
        (KeyModifiers::NONE, KeyCode::Enter) => Some(CreateRoomInput::Confirm),
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(character)) => {
            Some(CreateRoomInput::Character(character))
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateRoomDisposition {
    Editing,
    Armed,
    Pending,
    Applied,
    Refused,
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedRoomReceipt {
    pub room_id: String,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Field {
    RoomId,
    Name,
    Admission,
    Review,
}

impl Field {
    const fn next(self) -> Self {
        match self {
            Self::RoomId => Self::Name,
            Self::Name => Self::Admission,
            Self::Admission => Self::Review,
            Self::Review => Self::RoomId,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::RoomId => Self::Review,
            Self::Name => Self::RoomId,
            Self::Admission => Self::Name,
            Self::Review => Self::Admission,
        }
    }
}

enum WorkerRequest {
    Submit(CreateRoomRequestV1),
}

struct WorkerReply {
    disposition: CreateRoomDisposition,
    notice: String,
    receipt: Option<AppliedRoomReceipt>,
}

pub struct CreateRoomComponent {
    requests: SyncSender<WorkerRequest>,
    replies: Receiver<WorkerReply>,
    expected_revision: u64,
    room_id: String,
    name: String,
    admission: AdmissionPolicyV2,
    field: Field,
    disposition: CreateRoomDisposition,
    retained: Option<CreateRoomRequestV1>,
    applied_receipt: Option<AppliedRoomReceipt>,
    notice: String,
}

impl CreateRoomComponent {
    pub fn new_with_handle(handle: ControlHandle, expected_revision: u64) -> Self {
        Self::spawn(expected_revision, move |request_rx, reply_tx| {
            while let Ok(WorkerRequest::Submit(request)) = request_rx.recv() {
                let result = handle.create_room(request);
                let reply = match result {
                    ControlResult::Applied(receipt) => WorkerReply {
                        disposition: CreateRoomDisposition::Applied,
                        notice: format!(
                            "Agora room {} durably created at revision {}",
                            receipt.room_id, receipt.revision
                        ),
                        receipt: Some(AppliedRoomReceipt {
                            room_id: receipt.room_id,
                            revision: receipt.revision,
                        }),
                    },
                    ControlResult::OutcomeUnknown => WorkerReply {
                        disposition: CreateRoomDisposition::OutcomeUnknown,
                        notice: "Agora room outcome unknown; r replays the exact retained request"
                            .into(),
                        receipt: None,
                    },
                    ControlResult::Refused => WorkerReply {
                        disposition: CreateRoomDisposition::Refused,
                        notice: "Agora refused the exact room proposal; refresh room state".into(),
                        receipt: None,
                    },
                    ControlResult::Failed => WorkerReply {
                        disposition: CreateRoomDisposition::Failed,
                        notice: "Agora room creation failed without a durable receipt".into(),
                        receipt: None,
                    },
                };
                if reply_tx.send(reply).is_err() {
                    break;
                }
            }
        })
    }

    pub fn new<P>(mut client: ClientV2<P>, expected_revision: u64) -> Self
    where
        P: ServicePortV2 + Send + 'static,
        P::Error: Send + 'static,
    {
        Self::spawn(expected_revision, move |request_rx, reply_tx| {
            while let Ok(WorkerRequest::Submit(request)) = request_rx.recv() {
                let confirmation = required_create_confirmation(&request);
                let result = create_room(&mut client, request, &confirmation);
                if reply_tx.send(classify(result)).is_err() {
                    break;
                }
            }
        })
    }

    /// Builds the form immediately and performs transport connection on its
    /// worker only after exact confirmation, keeping host render/update paths
    /// free of socket waits.
    pub fn new_with_connector<P, F, E>(connector: F, expected_revision: u64) -> Self
    where
        P: ServicePortV2 + Send + 'static,
        P::Error: Send + 'static,
        F: FnOnce() -> Result<ClientV2<P>, E> + Send + 'static,
        E: std::fmt::Debug + Send + 'static,
    {
        Self::spawn(expected_revision, move |request_rx, reply_tx| {
            let Ok(WorkerRequest::Submit(first_request)) = request_rx.recv() else {
                return;
            };
            let mut client = match connector() {
                Ok(client) => client,
                Err(error) => {
                    let _ = reply_tx.send(WorkerReply {
                        disposition: CreateRoomDisposition::Failed,
                        notice: format!(
                            "Agora room control connection refused before submission: {error:?}"
                        ),
                        receipt: None,
                    });
                    return;
                }
            };
            let mut request = first_request;
            loop {
                let confirmation = required_create_confirmation(&request);
                let result = create_room(&mut client, request, &confirmation);
                if reply_tx.send(classify(result)).is_err() {
                    break;
                }
                let Ok(WorkerRequest::Submit(next)) = request_rx.recv() else {
                    break;
                };
                request = next;
            }
        })
    }

    fn spawn<F>(expected_revision: u64, worker: F) -> Self
    where
        F: FnOnce(Receiver<WorkerRequest>, SyncSender<WorkerReply>) + Send + 'static,
    {
        let (request_tx, request_rx) = sync_channel(1);
        let (reply_tx, reply_rx) = sync_channel(1);
        std::thread::spawn(move || worker(request_rx, reply_tx));
        Self {
            requests: request_tx,
            replies: reply_rx,
            expected_revision,
            room_id: String::new(),
            name: String::new(),
            admission: AdmissionPolicyV2::InviteOnly,
            field: Field::RoomId,
            disposition: CreateRoomDisposition::Editing,
            retained: None,
            applied_receipt: None,
            notice: "Name the room; Tab moves through fields".into(),
        }
    }

    pub fn poll(&mut self) {
        match self.replies.try_recv() {
            Ok(reply) => {
                self.disposition = reply.disposition;
                self.notice = reply.notice;
                self.applied_receipt = reply.receipt;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected)
                if self.disposition == CreateRoomDisposition::Pending =>
            {
                self.disposition = CreateRoomDisposition::OutcomeUnknown;
                self.notice =
                    "Agora room worker disconnected after submission; exact replay required".into();
            }
            Err(TryRecvError::Disconnected) => {}
        }
    }

    pub fn update(&mut self, input: CreateRoomInput, focused: bool) -> Lifecycle {
        if !focused {
            return Lifecycle::Running;
        }
        if input == CreateRoomInput::Close {
            if matches!(
                self.disposition,
                CreateRoomDisposition::Pending | CreateRoomDisposition::OutcomeUnknown
            ) {
                self.notice =
                    "Room creation is unresolved; wait or replay the exact retained request".into();
                return Lifecycle::Running;
            }
            return Lifecycle::CloseRequested;
        }
        match self.disposition {
            CreateRoomDisposition::Editing => self.update_editing(input),
            CreateRoomDisposition::Armed => match input {
                CreateRoomInput::Confirm => self.submit_retained(),
                CreateRoomInput::Close => unreachable!("handled above"),
                _ => {
                    self.disposition = CreateRoomDisposition::Editing;
                    self.notice = "Room creation disarmed".into();
                }
            },
            CreateRoomDisposition::OutcomeUnknown
                if matches!(
                    input,
                    CreateRoomInput::RetryExact | CreateRoomInput::Character('r')
                ) =>
            {
                self.submit_retained();
            }
            CreateRoomDisposition::Pending
            | CreateRoomDisposition::Applied
            | CreateRoomDisposition::Refused
            | CreateRoomDisposition::Failed
            | CreateRoomDisposition::OutcomeUnknown => {}
        }
        Lifecycle::Running
    }

    fn update_editing(&mut self, input: CreateRoomInput) {
        match input {
            CreateRoomInput::Character(character) => match self.field {
                Field::RoomId => push_bounded(&mut self.room_id, character, MAX_ROOM_ID_BYTES),
                Field::Name => push_bounded(&mut self.name, character, MAX_ROOM_NAME_BYTES),
                Field::Admission | Field::Review => {
                    if self.field == Field::Review && character == 'c' {
                        self.arm();
                    }
                }
            },
            CreateRoomInput::Backspace => match self.field {
                Field::RoomId => {
                    self.room_id.pop();
                }
                Field::Name => {
                    self.name.pop();
                }
                Field::Admission | Field::Review => {}
            },
            CreateRoomInput::NextField => self.field = self.field.next(),
            CreateRoomInput::PreviousField => self.field = self.field.previous(),
            CreateRoomInput::ToggleAdmission if self.field == Field::Admission => {
                self.admission = match self.admission {
                    AdmissionPolicyV2::Open => AdmissionPolicyV2::InviteOnly,
                    AdmissionPolicyV2::InviteOnly => AdmissionPolicyV2::Open,
                };
            }
            CreateRoomInput::Arm if self.field == Field::Review => self.arm(),
            CreateRoomInput::Confirm => {
                if self.field == Field::Review {
                    self.arm();
                } else {
                    self.field = self.field.next();
                }
            }
            CreateRoomInput::RetryExact
            | CreateRoomInput::ToggleAdmission
            | CreateRoomInput::Arm
            | CreateRoomInput::Close => {}
        }
    }

    /// Prefills one exact owner proposal while retaining the ordinary
    /// review/arm/confirm path. This grants no create authority and submits
    /// nothing; composed journeys still require an explicit human gate.
    pub fn prefill(
        &mut self,
        room_id: &str,
        name: &str,
        admission: AdmissionPolicyV2,
    ) -> Result<(), &'static str> {
        if self.disposition != CreateRoomDisposition::Editing
            || self.retained.is_some()
            || self.applied_receipt.is_some()
        {
            return Err("Agora room form is no longer editable");
        }
        if room_id.is_empty()
            || room_id.len() > MAX_ROOM_ID_BYTES
            || room_id.chars().any(char::is_control)
        {
            return Err("Agora room id is empty, over limit, or contains control text");
        }
        if name.is_empty() || name.len() > MAX_ROOM_NAME_BYTES || name.chars().any(char::is_control)
        {
            return Err("Agora room name is empty, over limit, or contains control text");
        }
        self.room_id = room_id.to_owned();
        self.name = name.to_owned();
        self.admission = admission;
        self.field = Field::Review;
        self.notice =
            "Room proposal prefilled; review exact identity, admission, and revision, then press c to arm"
                .into();
        Ok(())
    }

    fn arm(&mut self) {
        let Some(request) = self.proposal() else {
            self.notice = "Room id and name are required before review".into();
            return;
        };
        self.retained = Some(request);
        self.disposition = CreateRoomDisposition::Armed;
        self.notice =
            "Room creation armed; Enter submits exactly once, any other key disarms".into();
    }

    fn submit_retained(&mut self) {
        let Some(request) = self.retained.clone() else {
            self.disposition = CreateRoomDisposition::Failed;
            self.notice = "No exact room proposal retained".into();
            return;
        };
        if self
            .requests
            .try_send(WorkerRequest::Submit(request))
            .is_ok()
        {
            self.disposition = CreateRoomDisposition::Pending;
            self.notice =
                "Agora room creation pending; do not close or submit another effect".into();
        } else {
            self.disposition = CreateRoomDisposition::Failed;
            self.notice = "Agora room worker unavailable before submission".into();
        }
    }

    fn proposal(&self) -> Option<CreateRoomRequestV1> {
        if self.room_id.is_empty() || self.name.is_empty() {
            return None;
        }
        Some(CreateRoomRequestV1 {
            room_id: self.room_id.clone(),
            name: self.name.clone(),
            admission: self.admission,
            expected_revision: self.expected_revision,
        })
    }

    pub fn draw_in(&self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let title = if focused {
            " Create Agora room • focused "
        } else {
            " Create Agora room "
        };
        let field = |candidate, label: &'static str, value: String| {
            let selected = self.field == candidate;
            Line::from(vec![
                Span::styled(
                    if selected { "▸ " } else { "  " },
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("{label:<12}"),
                    if selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw(value),
            ])
        };
        let admission = match self.admission {
            AdmissionPolicyV2::Open => "open",
            AdmissionPolicyV2::InviteOnly => "invite only",
        };
        let mut lines = vec![
            field(Field::RoomId, "room id", self.room_id.clone()),
            field(Field::Name, "name", self.name.clone()),
            field(Field::Admission, "admission", admission.into()),
            Line::from(""),
            field(Field::Review, "review", format!("{:?}", self.disposition)),
        ];
        let proposal = self.proposal();
        if let Some(request) = self.retained.as_ref().or(proposal.as_ref()) {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Exact revision-bound proposal",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            lines.push(Line::from(required_create_confirmation(request)));
        }
        lines.extend([
            Line::from(""),
            Line::styled(
                "Tab/Shift-Tab fields · ←/→ admission · c arm on review · Enter confirm",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        if self.disposition == CreateRoomDisposition::OutcomeUnknown {
            lines.push(Line::styled(
                "Outcome unknown — r replays only the exact retained request",
                Style::default().fg(Color::Yellow),
            ));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(title)),
            body,
        );
        frame.render_widget(Paragraph::new(self.notice.as_str()), footer);
    }

    pub const fn disposition(&self) -> CreateRoomDisposition {
        self.disposition
    }

    pub fn notice(&self) -> &str {
        &self.notice
    }

    pub fn request_pending(&self) -> bool {
        self.disposition == CreateRoomDisposition::Pending
    }

    pub fn recovery_required(&self) -> bool {
        self.disposition == CreateRoomDisposition::OutcomeUnknown
    }

    pub fn applied_receipt(&self) -> Option<&AppliedRoomReceipt> {
        self.applied_receipt.as_ref()
    }
}

fn push_bounded(target: &mut String, character: char, max_bytes: usize) {
    if !character.is_control() && target.len() + character.len_utf8() <= max_bytes {
        target.push(character);
    }
}

fn classify<E>(
    result: Result<agora_client::room_command::CreateRoomReceiptV1, CreateRoomErrorV1<E>>,
) -> WorkerReply {
    match result {
        Ok(receipt) => WorkerReply {
            disposition: CreateRoomDisposition::Applied,
            notice: format!(
                "Agora room {} durably created at revision {}",
                receipt.room_id, receipt.revision
            ),
            receipt: Some(AppliedRoomReceipt {
                room_id: receipt.room_id,
                revision: receipt.revision,
            }),
        },
        Err(CreateRoomErrorV1::Client(ClientErrorV2::OutcomeUnknown(_)))
        | Err(CreateRoomErrorV1::Client(ClientErrorV2::UnresolvedOutcome)) => WorkerReply {
            disposition: CreateRoomDisposition::OutcomeUnknown,
            notice: "Agora room outcome unknown; r replays the exact retained request".into(),
            receipt: None,
        },
        Err(CreateRoomErrorV1::Refused(_)) | Err(CreateRoomErrorV1::ConfirmationRefused { .. }) => {
            WorkerReply {
                disposition: CreateRoomDisposition::Refused,
                notice: "Agora refused the exact room proposal; refresh room state".into(),
                receipt: None,
            }
        }
        Err(CreateRoomErrorV1::Client(ClientErrorV2::NotSent(_)))
        | Err(CreateRoomErrorV1::Client(ClientErrorV2::RequestIdExhausted))
        | Err(CreateRoomErrorV1::UnexpectedReply) => WorkerReply {
            disposition: CreateRoomDisposition::Failed,
            notice: "Agora room creation failed without a durable receipt".into(),
            receipt: None,
        },
    }
}
