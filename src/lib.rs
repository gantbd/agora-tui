//! Composable, read-only Ratatui presentation over Agora's public V2 client.
#![forbid(unsafe_code)]

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

use agora_client::v2::{ClientErrorV2, ClientV2, ServicePortV2};
use agora_protocol::v2::{CommandV2, MessageV2, OutcomeV2, ReplyV2, RoomSummaryV2};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Block, Borders, List, ListItem, Paragraph},
};

const PAGE_LIMIT: u32 = 32;

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

pub struct Component {
    requests: SyncSender<Request>,
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
        let (request_tx, request_rx) = sync_channel::<Request>(1);
        let (reply_tx, reply_rx) = sync_channel::<WorkerReply>(1);
        std::thread::spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let result = client
                    .execute(request.command())
                    .map_err(|failure| match failure {
                        ClientErrorV2::NotSent(_) => Failure::NotSent,
                        ClientErrorV2::OutcomeUnknown(_) => Failure::OutcomeUnknown,
                        ClientErrorV2::UnresolvedOutcome => Failure::UnresolvedOutcome,
                        ClientErrorV2::RequestIdExhausted => Failure::RequestIdExhausted,
                    });
                if reply_tx.send(WorkerReply { request, result }).is_err() {
                    break;
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
            rooms_truncated: false,
            messages_truncated: false,
            notice: "Agora ready; refresh queued".into(),
        };
        component.offer(Request::Rooms);
        component
    }

    pub fn poll(&mut self) {
        loop {
            match self.replies.try_recv() {
                Ok(reply) => self.admit(reply),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.notice = "Agora worker unavailable".into();
                    break;
                }
            }
        }
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
        match self.active_room {
            Some(_) => {
                let items = self.messages.iter().enumerate().map(|(index, message)| {
                    let marker = if index == self.message_selection {
                        ">"
                    } else {
                        " "
                    };
                    ListItem::new(Line::from(format!(
                        "{marker} {} · {}: {}",
                        message.sequence, message.author, message.body
                    )))
                });
                frame.render_widget(
                    List::new(items).block(Block::default().title("Messages")),
                    body,
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
        frame.render_widget(
            Paragraph::new(format!(
                "{}{} · ↑/↓ select · Enter open · Esc back · r refresh",
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
        match self.requests.try_send(request.clone()) {
            Ok(()) => {
                self.pending = Some(request);
                self.notice = "Agora request pending".into();
            }
            Err(_) => self.notice = "Agora worker busy".into(),
        }
    }

    fn admit(&mut self, reply: WorkerReply) {
        if self.pending.as_ref() != Some(&reply.request) {
            self.notice = "Discarded stale Agora reply".into();
            return;
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
            }
            Ok(ReplyV2::Accepted(OutcomeV2::Messages(page)))
                if reply.request == Request::Messages(page.room_id.clone()) =>
            {
                self.active_room = Some(page.room_id);
                self.messages = page.messages;
                self.message_revision = Some(page.revision);
                self.messages_truncated = page.next.is_some();
                self.message_selection = 0;
                self.uncertain = None;
                self.notice = format!("Observed {} messages", self.messages.len());
            }
            Ok(ReplyV2::Refused(refusal)) => {
                self.uncertain = None;
                self.notice = format!("Agora request refused: {refusal:?}");
            }
            Ok(_) => {
                self.uncertain = None;
                self.notice = "Agora returned an unexpected reply".into();
            }
            Err(Failure::OutcomeUnknown) => {
                self.uncertain = Some(reply.request);
                self.notice = "Agora read outcome unknown; press r to replay exactly".into();
            }
            Err(Failure::NotSent) => self.notice = "Agora request was not sent".into(),
            Err(Failure::UnresolvedOutcome) => {
                self.notice = "Agora client has an unresolved outcome".into();
            }
            Err(Failure::RequestIdExhausted) => {
                self.notice = "Agora request identity exhausted".into();
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
}
