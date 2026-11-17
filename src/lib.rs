//! Composable Ratatui presentation and guarded controls over Agora's public V2 client.
#![forbid(unsafe_code)]

mod message_control;
mod room_control;
pub use message_control::{
    ComposeMessageComponent, ComposeMessageDisposition, ComposeMessageInput,
    translate_compose_message_key,
};
pub use room_control::{
    CreateRoomComponent, CreateRoomDisposition, CreateRoomInput, translate_create_room_key,
};

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

use agora_client::{
    message_command::{
        SendMessageErrorV1, SendMessageRequestV1, required_send_confirmation, send_message,
    },
    room_command::{
        CreateRoomErrorV1, CreateRoomRequestV1, create_room, required_create_confirmation,
    },
    v2::{ClientErrorV2, ClientV2, ServicePortV2},
};
use agora_protocol::v2::{CommandV2, MessageV2, OutcomeV2, ReplyV2, RoomSummaryV2};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

const PAGE_LIMIT: u32 = 32;

/// Presentation capabilities owned by this peer. These identify supported host
/// composition surfaces; they do not grant Agora authority or claim that a
/// connected service will authorize a particular observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityManifest {
    pub schema: u8,
    pub peer: &'static str,
    pub interactive: &'static [&'static str],
    pub headless: &'static [&'static str],
}

pub const CAPABILITIES: CapabilityManifest = CapabilityManifest {
    schema: 1,
    peer: "agora",
    interactive: &["rooms.read.v2", "messages.read.v2"],
    headless: &["rooms.read.v2"],
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Input {
    Up,
    Down,
    Open,
    Back,
    Refresh,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    Running,
    CloseRequested,
}

pub fn translate_key(key: KeyEvent) -> Option<Input> {
    if key.kind != KeyEventKind::Press || key.modifiers != KeyModifiers::NONE {
        return None;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => Some(Input::Up),
        KeyCode::Down | KeyCode::Char('j') => Some(Input::Down),
        KeyCode::Enter => Some(Input::Open),
        KeyCode::Esc | KeyCode::Backspace => Some(Input::Back),
        KeyCode::Char('r') => Some(Input::Refresh),
        KeyCode::Char('q') => Some(Input::Close),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum View {
    Rooms,
    Messages,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageTargetV1 {
    pub room_id: String,
    pub expected_revision: u64,
    pub logical_time: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Request {
    Rooms,
    Messages(String),
}

impl Request {
    fn command(&self) -> CommandV2 {
        match self {
            Self::Rooms => CommandV2::Rooms {
                cursor: None,
                limit: PAGE_LIMIT,
            },
            Self::Messages(room_id) => CommandV2::Messages {
                room_id: room_id.clone(),
                cursor: None,
                limit: PAGE_LIMIT,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    NotSent,
    OutcomeUnknown,
    UnresolvedOutcome,
    RequestIdExhausted,
}

struct WorkerReply {
    request: Request,
    result: Result<ReplyV2, Failure>,
}

enum ControlResult<T> {
    Applied(T),
    Refused,
    Failed,
    OutcomeUnknown,
}

struct RoomControlReceipt {
    room_id: String,
    revision: u64,
}

struct MessageControlReceipt {
    room_id: String,
    revision: u64,
    sequence: u64,
}

enum WorkerCommand {
    Read(Request),
    CreateRoom {
        request: CreateRoomRequestV1,
        reply: SyncSender<ControlResult<RoomControlReceipt>>,
    },
    SendMessage {
        request: SendMessageRequestV1,
        reply: SyncSender<ControlResult<MessageControlReceipt>>,
    },
}

/// A cloneable route back to the exact `ClientV2` session owned by the
/// observation component. Agora's local owner serves one session at a time,
/// so controls must share this worker rather than opening a competing socket.
#[derive(Clone)]
pub struct ControlHandle {
    requests: SyncSender<WorkerCommand>,
}

impl ControlHandle {
    fn create_room(&self, request: CreateRoomRequestV1) -> ControlResult<RoomControlReceipt> {
        let (reply_tx, reply_rx) = sync_channel(1);
        if self
            .requests
            .send(WorkerCommand::CreateRoom {
                request,
                reply: reply_tx,
            })
            .is_err()
        {
            return ControlResult::Failed;
        }
        reply_rx.recv().unwrap_or(ControlResult::OutcomeUnknown)
    }

    fn send_message(&self, request: SendMessageRequestV1) -> ControlResult<MessageControlReceipt> {
        let (reply_tx, reply_rx) = sync_channel(1);
        if self
            .requests
            .send(WorkerCommand::SendMessage {
                request,
                reply: reply_tx,
            })
            .is_err()
        {
            return ControlResult::Failed;
        }
        reply_rx.recv().unwrap_or(ControlResult::OutcomeUnknown)
    }
}

pub struct Component {
    requests: SyncSender<WorkerCommand>,
    replies: Receiver<WorkerReply>,
    pending: Option<Request>,
    uncertain: Option<Request>,
    rooms: Vec<RoomSummaryV2>,
    messages: Vec<MessageV2>,
    room_selection: usize,
    message_selection: usize,
    active_room: Option<String>,
    room_revision: Option<u64>,
    message_revision: Option<u64>,
    next_message_sequence: Option<u64>,
    rooms_truncated: bool,
    messages_truncated: bool,
    notice: String,
}

impl Component {
    pub fn new<P>(mut client: ClientV2<P>) -> Self
    where
        P: ServicePortV2 + Send + 'static,
        P::Error: Send + 'static,
    {
        let (request_tx, request_rx) = sync_channel::<WorkerCommand>(1);
        let (reply_tx, reply_rx) = sync_channel::<WorkerReply>(1);
        std::thread::spawn(move || {
            while let Ok(command) = request_rx.recv() {
                match command {
                    WorkerCommand::Read(request) => {
                        let result =
                            client
                                .execute(request.command())
                                .map_err(|failure| match failure {
                                    ClientErrorV2::NotSent(_) => Failure::NotSent,
                                    ClientErrorV2::OutcomeUnknown(_) => Failure::OutcomeUnknown,
                                    ClientErrorV2::UnresolvedOutcome => Failure::UnresolvedOutcome,
                                    ClientErrorV2::RequestIdExhausted => {
                                        Failure::RequestIdExhausted
                                    }
                                });
                        if reply_tx.send(WorkerReply { request, result }).is_err() {
                            break;
                        }
                    }
                    WorkerCommand::CreateRoom { request, reply } => {
                        let confirmation = required_create_confirmation(&request);
                        let result = match create_room(&mut client, request, &confirmation) {
                            Ok(receipt) => ControlResult::Applied(RoomControlReceipt {
                                room_id: receipt.room_id,
                                revision: receipt.revision,
                            }),
                            Err(CreateRoomErrorV1::Client(ClientErrorV2::OutcomeUnknown(_)))
                            | Err(CreateRoomErrorV1::Client(ClientErrorV2::UnresolvedOutcome)) => {
                                ControlResult::OutcomeUnknown
                            }
                            Err(CreateRoomErrorV1::Refused(_))
                            | Err(CreateRoomErrorV1::ConfirmationRefused { .. }) => {
                                ControlResult::Refused
                            }
                            Err(CreateRoomErrorV1::Client(ClientErrorV2::NotSent(_)))
                            | Err(CreateRoomErrorV1::Client(ClientErrorV2::RequestIdExhausted))
                            | Err(CreateRoomErrorV1::UnexpectedReply) => ControlResult::Failed,
                        };
                        let _ = reply.send(result);
                    }
                    WorkerCommand::SendMessage { request, reply } => {
                        let confirmation = required_send_confirmation(&request);
                        let result = match send_message(&mut client, request, &confirmation) {
                            Ok(receipt) => ControlResult::Applied(MessageControlReceipt {
                                room_id: receipt.room_id,
                                revision: receipt.revision,
                                sequence: receipt.sequence,
                            }),
                            Err(SendMessageErrorV1::Client(ClientErrorV2::OutcomeUnknown(_)))
                            | Err(SendMessageErrorV1::Client(ClientErrorV2::UnresolvedOutcome)) => {
                                ControlResult::OutcomeUnknown
                            }
                            Err(SendMessageErrorV1::Refused(_))
                            | Err(SendMessageErrorV1::ConfirmationRefused { .. }) => {
                                ControlResult::Refused
                            }
                            Err(SendMessageErrorV1::Client(ClientErrorV2::NotSent(_)))
                            | Err(SendMessageErrorV1::Client(ClientErrorV2::RequestIdExhausted))
                            | Err(SendMessageErrorV1::UnexpectedReply) => ControlResult::Failed,
                        };
                        let _ = reply.send(result);
                    }
                }
            }
        });
        let mut component = Self {
            requests: request_tx,
            replies: reply_rx,
            pending: None,
            uncertain: None,
            rooms: Vec::new(),
            messages: Vec::new(),
            room_selection: 0,
            message_selection: 0,
            active_room: None,
            room_revision: None,
            message_revision: None,
            next_message_sequence: None,
            rooms_truncated: false,
            messages_truncated: false,
            notice: "Agora ready; refresh queued".into(),
        };
        component.offer(Request::Rooms);
        component
    }

    pub fn poll(&mut self) {
        let _ = self.poll_settlement();
    }

    /// Polls worker replies and reports whether the most recent pending public
    /// read produced a valid current projection. Hosts may use this only for
    /// freshness/performance instrumentation; it grants no retry authority.
    pub fn poll_settlement(&mut self) -> Option<Result<(), ()>> {
        let mut settlement = None;
        loop {
            match self.replies.try_recv() {
                Ok(reply) => {
                    settlement = Some(if self.admit(reply) { Ok(()) } else { Err(()) });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.pending.is_some() {
                        settlement = Some(Err(()));
                    }
                    self.pending = None;
                    self.notice = "Agora worker unavailable".into();
                    break;
                }
            }
        }
        settlement
    }

    pub const fn request_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn update(&mut self, input: Input, focused: bool) -> Lifecycle {
        if !focused {
            return Lifecycle::Running;
        }
        match input {
            Input::Close => return Lifecycle::CloseRequested,
            Input::Refresh => {
                let request = self
                    .uncertain
                    .clone()
                    .unwrap_or_else(|| match &self.active_room {
                        Some(room) => Request::Messages(room.clone()),
                        None => Request::Rooms,
                    });
                self.offer(request);
            }
            Input::Back if self.active_room.is_some() => {
                self.active_room = None;
                self.messages.clear();
                self.message_revision = None;
                self.next_message_sequence = None;
                self.message_selection = 0;
                self.notice = "Room list".into();
            }
            Input::Open if self.active_room.is_none() => {
                if let Some(room) = self.rooms.get(self.room_selection) {
                    self.offer(Request::Messages(room.room_id.clone()));
                }
            }
            Input::Up => self.move_selection(false),
            Input::Down => self.move_selection(true),
            Input::Open | Input::Back => {}
        }
        Lifecycle::Running
    }

    pub fn view(&self) -> View {
        if self.active_room.is_some() {
            View::Messages
        } else {
            View::Rooms
        }
    }

    /// Current stable room-list revision used only to open Agora's separate
    /// exact-confirmed create-room workflow.
    pub const fn room_revision(&self) -> Option<u64> {
        self.room_revision
    }

    /// Stable, revision-bound target for Agora's separate message composer.
    /// The owner-provided next sequence is used as a deterministic logical
    /// clock; the revision remains the concurrency guard.
    pub fn message_target(&self) -> Option<MessageTargetV1> {
        Some(MessageTargetV1 {
            room_id: self.active_room.clone()?,
            expected_revision: self.message_revision?,
            logical_time: self.next_message_sequence?,
        })
    }

    pub fn control_handle(&self) -> ControlHandle {
        ControlHandle {
            requests: self.requests.clone(),
        }
    }

    pub fn draw_in(&self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .areas(area);
        let focus = if focused { "focused" } else { "inactive" };
        frame.render_widget(
            Paragraph::new(match &self.active_room {
                Some(room) => match self.message_revision {
                    Some(revision) => {
                        format!("AGORA · {room} · revision {revision} · {focus}")
                    }
                    None => format!("AGORA · {room} · {focus}"),
                },
                None => match self.room_revision {
                    Some(revision) => format!("AGORA · rooms · revision {revision} · {focus}"),
                    None => format!("AGORA · rooms · {focus}"),
                },
            })
            .block(Block::default().borders(Borders::BOTTOM)),
            header,
        );
        match &self.active_room {
            Some(room_id) => {
                let [rooms, conversation] =
                    Layout::horizontal([Constraint::Percentage(34), Constraint::Percentage(66)])
                        .areas(body);
                let room_items = self.rooms.iter().enumerate().map(|(index, room)| {
                    let active = &room.room_id == room_id;
                    let selected = index == self.room_selection;
                    let marker = if active { "●" } else { " " };
                    let style = if active {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else if selected {
                        Style::default().fg(Color::White)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{marker} {}", room.name), style),
                        Span::styled(
                            format!("  {}", room.retained_message_count),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                });
                frame.render_widget(
                    List::new(room_items)
                        .block(Block::default().borders(Borders::RIGHT).title(" Rooms ")),
                    rooms,
                );

                let mut lines = Vec::new();
                for (index, message) in self.messages.iter().enumerate() {
                    let selected = index == self.message_selection;
                    lines.push(Line::from(vec![
                        Span::styled(
                            if selected { "▸ " } else { "  " },
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            message.author.as_str(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  #{} · t{}", message.sequence, message.logical_time),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                    for body_line in message.body.lines() {
                        lines.push(Line::from(format!("    {body_line}")));
                    }
                    if !message.addressed_to.is_empty() {
                        lines.push(Line::styled(
                            format!("    to: {}", message.addressed_to.join(", ")),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    lines.push(Line::from(""));
                }
                if lines.is_empty() {
                    lines.push(Line::styled(
                        "No retained messages in this room.",
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                frame.render_widget(
                    Paragraph::new(lines)
                        .wrap(Wrap { trim: false })
                        .block(Block::default().title(format!(" Conversation · {room_id} "))),
                    conversation,
                );
            }
            None => {
                let items = self.rooms.iter().enumerate().map(|(index, room)| {
                    let marker = if index == self.room_selection {
                        ">"
                    } else {
                        " "
                    };
                    ListItem::new(Line::from(format!(
                        "{marker} {} · {} members · {} messages",
                        room.name, room.member_count, room.retained_message_count
                    )))
                });
                frame.render_widget(
                    List::new(items).block(Block::default().title("Rooms")),
                    body,
                );
            }
        }
        let truncated = match self.view() {
            View::Rooms => self.rooms_truncated,
            View::Messages => self.messages_truncated,
        };
        let controls = match self.view() {
            View::Rooms => "↑/↓ choose room · Enter open · r refresh",
            View::Messages => "↑/↓ inspect message · Esc rooms · r refresh",
        };
        frame.render_widget(
            Paragraph::new(format!(
                "{}{} · {controls}",
                self.notice,
                if truncated { " · more available" } else { "" }
            ))
            .block(Block::default().borders(Borders::TOP)),
            footer,
        );
    }

    fn offer(&mut self, request: Request) {
        if self.pending.is_some() {
            self.notice = "Agora request already pending".into();
            return;
        }
        if let Some(uncertain) = &self.uncertain
            && uncertain != &request
        {
            self.notice = "Replay the exact uncertain Agora read before another request".into();
            return;
        }
        match self.requests.try_send(WorkerCommand::Read(request.clone())) {
            Ok(()) => {
                self.pending = Some(request);
                self.notice = "Agora request pending".into();
            }
            Err(_) => self.notice = "Agora worker busy".into(),
        }
    }

    fn admit(&mut self, reply: WorkerReply) -> bool {
        if self.pending.as_ref() != Some(&reply.request) {
            self.notice = "Discarded stale Agora reply".into();
            return false;
        }
        self.pending = None;
        match reply.result {
            Ok(ReplyV2::Accepted(OutcomeV2::Rooms(page))) if reply.request == Request::Rooms => {
                self.rooms = page.rooms;
                self.room_revision = Some(page.revision);
                self.rooms_truncated = page.next.is_some();
                self.room_selection = self.room_selection.min(self.rooms.len().saturating_sub(1));
                self.uncertain = None;
                self.notice = format!("Observed {} rooms", self.rooms.len());
                true
            }
            Ok(ReplyV2::Accepted(OutcomeV2::Messages(page)))
                if reply.request == Request::Messages(page.room_id.clone()) =>
            {
                self.active_room = Some(page.room_id);
                self.messages = page.messages;
                self.message_revision = Some(page.revision);
                self.next_message_sequence = Some(page.next_message_sequence);
                self.messages_truncated = page.next.is_some();
                self.message_selection = 0;
                self.uncertain = None;
                self.notice = format!("Observed {} messages", self.messages.len());
                true
            }
            Ok(ReplyV2::Refused(refusal)) => {
                self.uncertain = None;
                self.notice = format!("Agora request refused: {refusal:?}");
                false
            }
            Ok(_) => {
                self.uncertain = None;
                self.notice = "Agora returned an unexpected reply".into();
                false
            }
            Err(Failure::OutcomeUnknown) => {
                self.uncertain = Some(reply.request);
                self.notice = "Agora read outcome unknown; press r to replay exactly".into();
                false
            }
            Err(Failure::NotSent) => {
                self.notice = "Agora request was not sent".into();
                false
            }
            Err(Failure::UnresolvedOutcome) => {
                self.notice = "Agora client has an unresolved outcome".into();
                false
            }
            Err(Failure::RequestIdExhausted) => {
                self.notice = "Agora request identity exhausted".into();
                false
            }
        }
    }

    fn move_selection(&mut self, down: bool) {
        let (selection, length) = if self.active_room.is_some() {
            (&mut self.message_selection, self.messages.len())
        } else {
            (&mut self.room_selection, self.rooms.len())
        };
        if length == 0 {
            return;
        }
        *selection = if down {
            (*selection + 1).min(length - 1)
        } else {
            selection.saturating_sub(1)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_client::v2::PortFailureV2;
    use agora_protocol::v2::{
        AdmissionPolicyV2, ClientIdV2, MessagePageV2, PageCursorV2, RequestV2, RoleV2, RoomPageV2,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use std::{collections::VecDeque, thread, time::Duration};

    struct Port {
        replies: VecDeque<Result<ReplyV2, PortFailureV2<()>>>,
    }

    impl ServicePortV2 for Port {
        type Error = ();

        fn execute(
            &mut self,
            _: ClientIdV2,
            _: RequestV2,
        ) -> Result<ReplyV2, PortFailureV2<Self::Error>> {
            self.replies.pop_front().expect("fixture reply")
        }
    }

    fn room_page() -> ReplyV2 {
        ReplyV2::Accepted(OutcomeV2::Rooms(RoomPageV2 {
            revision: 4,
            rooms: vec![RoomSummaryV2 {
                room_id: "atlas".into(),
                name: "Atlas".into(),
                admission: AdmissionPolicyV2::InviteOnly,
                observer_role: RoleV2::Member,
                member_count: 3,
                retained_message_count: 1,
                next_message_sequence: 2,
            }],
            next: Some(PageCursorV2(1)),
        }))
    }

    fn messages_page() -> ReplyV2 {
        ReplyV2::Accepted(OutcomeV2::Messages(MessagePageV2 {
            revision: 4,
            room_id: "atlas".into(),
            messages: vec![MessageV2 {
                sequence: 1,
                author: "Ada".into(),
                logical_time: 1,
                body: "Ship the bounded view".into(),
                addressed_to: Vec::new(),
            }],
            oldest_retained_sequence: 1,
            next_message_sequence: 2,
            next: None,
        }))
    }

    fn messages_page_after_delivery() -> ReplyV2 {
        ReplyV2::Accepted(OutcomeV2::Messages(MessagePageV2 {
            revision: 5,
            room_id: "atlas".into(),
            messages: vec![
                MessageV2 {
                    sequence: 1,
                    author: "Ada".into(),
                    logical_time: 1,
                    body: "Ship the bounded view".into(),
                    addressed_to: Vec::new(),
                },
                MessageV2 {
                    sequence: 2,
                    author: "Ada".into(),
                    logical_time: 2,
                    body: "Shared client delivery".into(),
                    addressed_to: vec!["Grace".into()],
                },
            ],
            oldest_retained_sequence: 1,
            next_message_sequence: 3,
            next: None,
        }))
    }

    fn settle(component: &mut Component) {
        for _ in 0..100 {
            component.poll();
            if component.pending.is_none() {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("component did not settle");
    }

    #[test]
    fn room_and_message_reads_use_public_client_without_blocking_update() {
        let port = Port {
            replies: VecDeque::from([Ok(room_page()), Ok(messages_page())]),
        };
        let mut component = Component::new(ClientV2::new(ClientIdV2(7), port));
        settle(&mut component);
        assert_eq!(component.view(), View::Rooms);
        assert_eq!(component.update(Input::Open, true), Lifecycle::Running);
        settle(&mut component);
        assert_eq!(component.view(), View::Messages);

        let mut terminal = Terminal::new(TestBackend::new(70, 12)).unwrap();
        terminal
            .draw(|frame| component.draw_in(frame, Rect::new(5, 2, 50, 8), true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("Ship the bounded view"));
        assert!(rendered.contains("Rooms"));
        assert!(rendered.contains("Conversation"));
        assert!(rendered.contains("Ada"));
        for y in 0..12 {
            for x in 0..70 {
                if !(5..55).contains(&x) || !(2..10).contains(&y) {
                    assert_eq!(buffer[(x, y)].symbol(), " ");
                }
            }
        }
    }

    #[test]
    fn unknown_read_fences_different_input_until_exact_replay() {
        let port = Port {
            replies: VecDeque::from([Err(PortFailureV2::OutcomeUnknown(())), Ok(room_page())]),
        };
        let mut component = Component::new(ClientV2::new(ClientIdV2(9), port));
        settle(&mut component);
        assert!(component.uncertain.is_some());
        component.update(Input::Open, true);
        assert!(component.pending.is_none());
        component.update(Input::Refresh, true);
        settle(&mut component);
        assert!(component.uncertain.is_none());
        assert_eq!(component.rooms.len(), 1);
    }

    #[test]
    fn message_control_shares_the_observation_client_and_refreshes_owner_state() {
        let port = Port {
            replies: VecDeque::from([
                Ok(room_page()),
                Ok(messages_page()),
                Ok(ReplyV2::Accepted(OutcomeV2::MessageAccepted {
                    revision: 5,
                    sequence: 2,
                })),
                Ok(messages_page_after_delivery()),
            ]),
        };
        let mut component = Component::new(ClientV2::new(ClientIdV2(11), port));
        settle(&mut component);
        component.update(Input::Open, true);
        settle(&mut component);
        let target = component.message_target().unwrap();
        let mut composer =
            ComposeMessageComponent::new_with_handle(component.control_handle(), target);
        composer.insert_text("Shared client delivery");
        composer.update(ComposeMessageInput::NextField, true);
        composer.insert_text("Grace");
        composer.update(ComposeMessageInput::NextField, true);
        composer.update(ComposeMessageInput::Arm, true);
        composer.update(ComposeMessageInput::Confirm, true);
        for _ in 0..100 {
            composer.poll();
            if !composer.request_pending() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(composer.disposition(), ComposeMessageDisposition::Applied);
        component.update(Input::Refresh, true);
        settle(&mut component);
        assert_eq!(component.message_revision, Some(5));
        assert_eq!(component.messages.len(), 2);
        assert_eq!(component.messages[1].body, "Shared client delivery");
    }
}
