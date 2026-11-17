//! Multiline, addressed, exact-confirmed Agora message composition.

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

use agora_client::{
    message_command::{
        SendMessageErrorV1, SendMessageRequestV1, required_send_confirmation, send_message,
    },
    v2::{ClientErrorV2, ClientV2, ServicePortV2},
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::{ControlHandle, ControlResult, Lifecycle, MessageTargetV1};

pub const MAX_MESSAGE_BODY_BYTES: usize = 16 * 1024;
pub const MAX_ADDRESSEES: usize = 64;
pub const MAX_PARTICIPANT_ID_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComposeMessageInput {
    Character(char),
    Paste(String),
    Backspace,
    Newline,
    NextField,
    PreviousField,
    Arm,
    Confirm,
    RetryExact,
    Close,
}

pub fn translate_compose_message_key(key: KeyEvent) -> Option<ComposeMessageInput> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Esc) => Some(ComposeMessageInput::Close),
        (KeyModifiers::NONE, KeyCode::Backspace) => Some(ComposeMessageInput::Backspace),
        (KeyModifiers::NONE, KeyCode::Tab) => Some(ComposeMessageInput::NextField),
        (KeyModifiers::SHIFT, KeyCode::BackTab) => Some(ComposeMessageInput::PreviousField),
        (KeyModifiers::NONE, KeyCode::Enter) => Some(ComposeMessageInput::Newline),
        (KeyModifiers::CONTROL, KeyCode::Enter) => Some(ComposeMessageInput::Confirm),
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(character)) => {
            Some(ComposeMessageInput::Character(character))
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComposeMessageDisposition {
    Editing,
    Armed,
    Pending,
    Applied,
    Refused,
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Field {
    Body,
    Addressees,
    Review,
}

impl Field {
    const fn next(self) -> Self {
        match self {
            Self::Body => Self::Addressees,
            Self::Addressees => Self::Review,
            Self::Review => Self::Body,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::Body => Self::Review,
            Self::Addressees => Self::Body,
            Self::Review => Self::Addressees,
        }
    }
}

enum WorkerRequest {
    Submit(SendMessageRequestV1),
}

struct WorkerReply {
    disposition: ComposeMessageDisposition,
    notice: String,
}

pub struct ComposeMessageComponent {
    requests: SyncSender<WorkerRequest>,
    replies: Receiver<WorkerReply>,
    target: MessageTargetV1,
    body: String,
    addressees: String,
    field: Field,
    disposition: ComposeMessageDisposition,
    retained: Option<SendMessageRequestV1>,
    notice: String,
}

impl ComposeMessageComponent {
    pub fn new_with_handle(handle: ControlHandle, target: MessageTargetV1) -> Self {
        Self::spawn(target, move |request_rx, reply_tx| {
            while let Ok(WorkerRequest::Submit(request)) = request_rx.recv() {
                let result = handle.send_message(request);
                let reply = match result {
                    ControlResult::Applied(receipt) => WorkerReply {
                        disposition: ComposeMessageDisposition::Applied,
                        notice: format!(
                            "Agora accepted message #{} in {} at revision {}",
                            receipt.sequence, receipt.room_id, receipt.revision
                        ),
                    },
                    ControlResult::OutcomeUnknown => WorkerReply {
                        disposition: ComposeMessageDisposition::OutcomeUnknown,
                        notice:
                            "Agora message outcome unknown; r replays the exact retained request"
                                .into(),
                    },
                    ControlResult::Refused => WorkerReply {
                        disposition: ComposeMessageDisposition::Refused,
                        notice:
                            "Agora refused the exact message proposal; refresh the conversation"
                                .into(),
                    },
                    ControlResult::Failed => WorkerReply {
                        disposition: ComposeMessageDisposition::Failed,
                        notice: "Agora message failed without a durable owner receipt".into(),
                    },
                };
                if reply_tx.send(reply).is_err() {
                    break;
                }
            }
        })
    }

    pub fn new<P>(mut client: ClientV2<P>, target: MessageTargetV1) -> Self
    where
        P: ServicePortV2 + Send + 'static,
        P::Error: Send + 'static,
    {
        Self::spawn(target, move |request_rx, reply_tx| {
            while let Ok(WorkerRequest::Submit(request)) = request_rx.recv() {
                let confirmation = required_send_confirmation(&request);
                let result = send_message(&mut client, request, &confirmation);
                if reply_tx.send(classify(result)).is_err() {
                    break;
                }
            }
        })
    }

    pub fn new_with_connector<P, F, E>(connector: F, target: MessageTargetV1) -> Self
    where
        P: ServicePortV2 + Send + 'static,
        P::Error: Send + 'static,
        F: FnOnce() -> Result<ClientV2<P>, E> + Send + 'static,
        E: std::fmt::Debug + Send + 'static,
    {
        Self::spawn(target, move |request_rx, reply_tx| {
            let Ok(WorkerRequest::Submit(first_request)) = request_rx.recv() else {
                return;
            };
            let mut client = match connector() {
                Ok(client) => client,
                Err(error) => {
                    let _ = reply_tx.send(WorkerReply {
                        disposition: ComposeMessageDisposition::Failed,
                        notice: format!(
                            "Agora message connection refused before submission: {error:?}"
                        ),
                    });
                    return;
                }
            };
            let mut request = first_request;
            loop {
                let confirmation = required_send_confirmation(&request);
                let result = send_message(&mut client, request, &confirmation);
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

    fn spawn<F>(target: MessageTargetV1, worker: F) -> Self
    where
        F: FnOnce(Receiver<WorkerRequest>, SyncSender<WorkerReply>) + Send + 'static,
    {
        let (request_tx, request_rx) = sync_channel(1);
        let (reply_tx, reply_rx) = sync_channel(1);
        std::thread::spawn(move || worker(request_rx, reply_tx));
        Self {
            requests: request_tx,
            replies: reply_rx,
            target,
            body: String::new(),
            addressees: String::new(),
            field: Field::Body,
            disposition: ComposeMessageDisposition::Editing,
            retained: None,
            notice: "Compose a message; Tab moves to addressing and review".into(),
        }
    }

    pub fn poll(&mut self) {
        match self.replies.try_recv() {
            Ok(reply) => {
                self.disposition = reply.disposition;
                self.notice = reply.notice;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected)
                if self.disposition == ComposeMessageDisposition::Pending =>
            {
                self.disposition = ComposeMessageDisposition::OutcomeUnknown;
                self.notice =
                    "Agora message worker disconnected after submission; exact replay required"
                        .into();
            }
            Err(TryRecvError::Disconnected) => {}
        }
    }

    pub fn update(&mut self, input: ComposeMessageInput, focused: bool) -> Lifecycle {
        if !focused {
            return Lifecycle::Running;
        }
        if input == ComposeMessageInput::Close {
            if matches!(
                self.disposition,
                ComposeMessageDisposition::Pending | ComposeMessageDisposition::OutcomeUnknown
            ) {
                self.notice =
                    "Message delivery is unresolved; wait or replay the exact retained request"
                        .into();
                return Lifecycle::Running;
            }
            return Lifecycle::CloseRequested;
        }
        match self.disposition {
            ComposeMessageDisposition::Editing => self.update_editing(input),
            ComposeMessageDisposition::Armed => match input {
                ComposeMessageInput::Confirm | ComposeMessageInput::Newline => {
                    self.submit_retained()
                }
                _ => {
                    self.disposition = ComposeMessageDisposition::Editing;
                    self.notice = "Message delivery disarmed".into();
                }
            },
            ComposeMessageDisposition::OutcomeUnknown
                if matches!(
                    input,
                    ComposeMessageInput::RetryExact | ComposeMessageInput::Character('r')
                ) =>
            {
                self.submit_retained();
            }
            ComposeMessageDisposition::Pending
            | ComposeMessageDisposition::Applied
            | ComposeMessageDisposition::Refused
            | ComposeMessageDisposition::Failed
            | ComposeMessageDisposition::OutcomeUnknown => {}
        }
        Lifecycle::Running
    }

    fn update_editing(&mut self, input: ComposeMessageInput) {
        match input {
            ComposeMessageInput::Character(character) => match self.field {
                Field::Body => push_bounded(&mut self.body, character, MAX_MESSAGE_BODY_BYTES),
                Field::Addressees => push_bounded(
                    &mut self.addressees,
                    character,
                    MAX_ADDRESSEES * (MAX_PARTICIPANT_ID_BYTES + 1),
                ),
                Field::Review if character == 'c' => self.arm(),
                Field::Review => {}
            },
            ComposeMessageInput::Paste(text) => self.insert_text(&text),
            ComposeMessageInput::Backspace => match self.field {
                Field::Body => {
                    self.body.pop();
                }
                Field::Addressees => {
                    self.addressees.pop();
                }
                Field::Review => {}
            },
            ComposeMessageInput::Newline if self.field == Field::Body => {
                push_bounded(&mut self.body, '\n', MAX_MESSAGE_BODY_BYTES)
            }
            ComposeMessageInput::Newline => self.field = self.field.next(),
            ComposeMessageInput::NextField => self.field = self.field.next(),
            ComposeMessageInput::PreviousField => self.field = self.field.previous(),
            ComposeMessageInput::Arm | ComposeMessageInput::Confirm
                if self.field == Field::Review =>
            {
                self.arm()
            }
            ComposeMessageInput::RetryExact
            | ComposeMessageInput::Arm
            | ComposeMessageInput::Confirm
            | ComposeMessageInput::Close => {}
        }
    }

    pub fn insert_text(&mut self, text: &str) {
        if self.disposition != ComposeMessageDisposition::Editing {
            return;
        }
        let max = match self.field {
            Field::Body => MAX_MESSAGE_BODY_BYTES,
            Field::Addressees => MAX_ADDRESSEES * (MAX_PARTICIPANT_ID_BYTES + 1),
            Field::Review => return,
        };
        let target = match self.field {
            Field::Body => &mut self.body,
            Field::Addressees => &mut self.addressees,
            Field::Review => return,
        };
        for character in text.chars() {
            push_bounded(target, character, max);
        }
    }

    fn arm(&mut self) {
        match self.proposal() {
            Ok(request) => {
                self.retained = Some(request);
                self.disposition = ComposeMessageDisposition::Armed;
                self.notice =
                    "Message armed; Enter submits exactly once, any other key disarms".into();
            }
            Err(notice) => self.notice = notice,
        }
    }

    fn proposal(&self) -> Result<SendMessageRequestV1, String> {
        if self.body.trim().is_empty() {
            return Err("Message body must contain non-whitespace text".into());
        }
        let mut addressed_to = self
            .addressees
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if addressed_to.len() > MAX_ADDRESSEES {
            return Err(format!("Address at most {MAX_ADDRESSEES} participants"));
        }
        if addressed_to
            .iter()
            .any(|participant| participant.len() > MAX_PARTICIPANT_ID_BYTES)
        {
            return Err(format!(
                "Participant ids are limited to {MAX_PARTICIPANT_ID_BYTES} bytes"
            ));
        }
        addressed_to.sort();
        addressed_to.dedup();
        Ok(SendMessageRequestV1 {
            room_id: self.target.room_id.clone(),
            expected_revision: self.target.expected_revision,
            logical_time: self.target.logical_time,
            body: self.body.clone(),
            addressed_to,
        })
    }

    fn submit_retained(&mut self) {
        let Some(request) = self.retained.clone() else {
            self.disposition = ComposeMessageDisposition::Failed;
            self.notice = "No exact message proposal retained".into();
            return;
        };
        if self
            .requests
            .try_send(WorkerRequest::Submit(request))
            .is_ok()
        {
            self.disposition = ComposeMessageDisposition::Pending;
            self.notice = "Agora message pending; do not close or submit another effect".into();
        } else {
            self.disposition = ComposeMessageDisposition::Failed;
            self.notice = "Agora message worker unavailable before submission".into();
        }
    }

    pub fn draw_in(&self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let title = if focused {
            " Compose Agora message • focused "
        } else {
            " Compose Agora message "
        };
        let selected = |field| {
            if self.field == field {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            }
        };
        let recipients = self
            .proposal()
            .map(|request| request.addressed_to.len())
            .unwrap_or(0);
        let mut lines = vec![Line::from(vec![
            Span::styled("body       ", selected(Field::Body)),
            Span::raw(format!(
                "{} / {MAX_MESSAGE_BODY_BYTES} bytes",
                self.body.len()
            )),
        ])];
        for line in self.body.lines().take(8) {
            lines.push(Line::from(format!("  {line}")));
        }
        if self.body.is_empty() {
            lines.push(Line::styled(
                "  (empty)",
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.extend([
            Line::from(""),
            Line::from(vec![
                Span::styled("addressed  ", selected(Field::Addressees)),
                Span::raw(if self.addressees.is_empty() {
                    "everyone in room".into()
                } else {
                    self.addressees.clone()
                }),
            ]),
            Line::from(vec![
                Span::styled("review     ", selected(Field::Review)),
                Span::raw(format!(
                    "{:?} · {recipients} recipient(s) · revision {} · logical time {}",
                    self.disposition, self.target.expected_revision, self.target.logical_time
                )),
            ]),
        ]);
        if let Some(request) = self.retained.as_ref() {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                required_send_confirmation(request),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        lines.extend([
            Line::from(""),
            Line::styled(
                "Enter newline · Tab fields · comma-separated recipients · c arm · Enter confirm",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        if self.disposition == ComposeMessageDisposition::OutcomeUnknown {
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

    pub const fn disposition(&self) -> ComposeMessageDisposition {
        self.disposition
    }

    pub fn notice(&self) -> &str {
        &self.notice
    }

    pub fn request_pending(&self) -> bool {
        self.disposition == ComposeMessageDisposition::Pending
    }

    pub fn recovery_required(&self) -> bool {
        self.disposition == ComposeMessageDisposition::OutcomeUnknown
    }
}

fn push_bounded(target: &mut String, character: char, max_bytes: usize) {
    if (!character.is_control() || character == '\n')
        && target.len() + character.len_utf8() <= max_bytes
    {
        target.push(character);
    }
}

fn classify<E>(
    result: Result<agora_client::message_command::SendMessageReceiptV1, SendMessageErrorV1<E>>,
) -> WorkerReply {
    match result {
        Ok(receipt) => WorkerReply {
            disposition: ComposeMessageDisposition::Applied,
            notice: format!(
                "Agora accepted message #{} in {} at revision {}",
                receipt.sequence, receipt.room_id, receipt.revision
            ),
        },
        Err(SendMessageErrorV1::Client(ClientErrorV2::OutcomeUnknown(_)))
        | Err(SendMessageErrorV1::Client(ClientErrorV2::UnresolvedOutcome)) => WorkerReply {
            disposition: ComposeMessageDisposition::OutcomeUnknown,
            notice: "Agora message outcome unknown; r replays the exact retained request".into(),
        },
        Err(SendMessageErrorV1::Refused(_))
        | Err(SendMessageErrorV1::ConfirmationRefused { .. }) => WorkerReply {
            disposition: ComposeMessageDisposition::Refused,
            notice: "Agora refused the exact message proposal; refresh the conversation".into(),
        },
        Err(SendMessageErrorV1::Client(ClientErrorV2::NotSent(_)))
        | Err(SendMessageErrorV1::Client(ClientErrorV2::RequestIdExhausted))
        | Err(SendMessageErrorV1::UnexpectedReply) => WorkerReply {
            disposition: ComposeMessageDisposition::Failed,
            notice: "Agora message failed without a durable owner receipt".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_client::v2::PortFailureV2;
    use agora_protocol::v2::{ClientIdV2, OutcomeV2, ReplyV2, RequestV2};
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    struct Port {
        requests: Arc<Mutex<Vec<RequestV2>>>,
        replies: VecDeque<Result<ReplyV2, PortFailureV2<&'static str>>>,
    }

    impl ServicePortV2 for Port {
        type Error = &'static str;

        fn execute(
            &mut self,
            _: ClientIdV2,
            request: RequestV2,
        ) -> Result<ReplyV2, PortFailureV2<Self::Error>> {
            self.requests.lock().unwrap().push(request);
            self.replies.pop_front().expect("fixture reply")
        }
    }

    fn target() -> MessageTargetV1 {
        MessageTargetV1 {
            room_id: "atlas".into(),
            expected_revision: 9,
            logical_time: 12,
        }
    }

    fn settle(component: &mut ComposeMessageComponent) {
        for _ in 0..100 {
            component.poll();
            if !component.request_pending() {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("component did not settle");
    }

    fn compose_and_submit(component: &mut ComposeMessageComponent) {
        component.insert_text("first line\nsecond line");
        component.update(ComposeMessageInput::NextField, true);
        component.insert_text("grace, ada, grace");
        component.update(ComposeMessageInput::NextField, true);
        component.update(ComposeMessageInput::Arm, true);
        component.update(ComposeMessageInput::Confirm, true);
    }

    #[test]
    fn composes_multiline_body_and_canonical_recipient_set() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let port = Port {
            requests: Arc::clone(&requests),
            replies: [Ok(ReplyV2::Accepted(OutcomeV2::MessageAccepted {
                revision: 10,
                sequence: 12,
            }))]
            .into(),
        };
        let mut component =
            ComposeMessageComponent::new(ClientV2::new(ClientIdV2(5), port), target());
        compose_and_submit(&mut component);
        settle(&mut component);
        assert_eq!(component.disposition(), ComposeMessageDisposition::Applied);
        let requests = requests.lock().unwrap();
        assert!(matches!(
            &requests[0].command,
            agora_protocol::v2::CommandV2::Speak {
                room_id,
                expected_revision: 9,
                logical_time: 12,
                body,
                addressed_to,
            } if room_id == "atlas"
                && body == "first line\nsecond line"
                && addressed_to == &["ada".to_owned(), "grace".to_owned()]
        ));
    }

    #[test]
    fn uncertain_delivery_only_replays_the_identical_request() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let port = Port {
            requests: Arc::clone(&requests),
            replies: [
                Err(PortFailureV2::OutcomeUnknown("lost")),
                Ok(ReplyV2::Accepted(OutcomeV2::MessageAccepted {
                    revision: 10,
                    sequence: 12,
                })),
            ]
            .into(),
        };
        let mut component =
            ComposeMessageComponent::new(ClientV2::new(ClientIdV2(5), port), target());
        compose_and_submit(&mut component);
        settle(&mut component);
        assert_eq!(
            component.disposition(),
            ComposeMessageDisposition::OutcomeUnknown
        );
        component.update(ComposeMessageInput::RetryExact, true);
        settle(&mut component);
        assert_eq!(component.disposition(), ComposeMessageDisposition::Applied);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
    }
}
