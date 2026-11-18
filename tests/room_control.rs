use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use agora_client::v2::{ClientV2, PortFailureV2, ServicePortV2};
use agora_protocol::v2::{AdmissionPolicyV2, ClientIdV2, CommandV2, OutcomeV2, ReplyV2, RequestV2};
use agora_tui::{CreateRoomComponent, CreateRoomDisposition, CreateRoomInput, Lifecycle};
use ratatui::{Terminal, backend::TestBackend};

struct Port {
    requests: Arc<Mutex<Vec<RequestV2>>>,
    replies: VecDeque<Result<ReplyV2, PortFailureV2<()>>>,
}

impl ServicePortV2 for Port {
    type Error = ();

    fn execute(
        &mut self,
        client: ClientIdV2,
        request: RequestV2,
    ) -> Result<ReplyV2, PortFailureV2<Self::Error>> {
        assert_eq!(client, ClientIdV2(7));
        self.requests.lock().unwrap().push(request);
        self.replies.pop_front().expect("fixture reply")
    }
}

fn component(
    replies: VecDeque<Result<ReplyV2, PortFailureV2<()>>>,
) -> (CreateRoomComponent, Arc<Mutex<Vec<RequestV2>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let client = ClientV2::new(
        ClientIdV2(7),
        Port {
            requests: Arc::clone(&requests),
            replies,
        },
    );
    (CreateRoomComponent::new(client, 11), requests)
}

fn enter_proposal(component: &mut CreateRoomComponent) {
    for character in "delivery".chars() {
        component.update(CreateRoomInput::Character(character), true);
    }
    component.update(CreateRoomInput::NextField, true);
    for character in "Delivery Room".chars() {
        component.update(CreateRoomInput::Character(character), true);
    }
    component.update(CreateRoomInput::NextField, true);
    component.update(CreateRoomInput::ToggleAdmission, true);
    component.update(CreateRoomInput::NextField, true);
    component.update(CreateRoomInput::Character('c'), true);
    assert_eq!(component.disposition(), CreateRoomDisposition::Armed);
    component.update(CreateRoomInput::Confirm, true);
}

fn settle(component: &mut CreateRoomComponent) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while component.disposition() == CreateRoomDisposition::Pending {
        component.poll();
        assert!(Instant::now() < deadline, "room control did not settle");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn bounded_form_submits_one_exact_revision_bound_room() {
    let (mut component, requests) = component(VecDeque::from([Ok(ReplyV2::Accepted(
        OutcomeV2::RoomCreated { revision: 12 },
    ))]));
    enter_proposal(&mut component);
    settle(&mut component);
    assert_eq!(component.disposition(), CreateRoomDisposition::Applied);
    let receipt = component.applied_receipt().expect("applied owner receipt");
    assert_eq!(receipt.room_id, "delivery");
    assert_eq!(receipt.revision, 12);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(matches!(
        &requests[0].command,
        CommandV2::CreateRoom {
            room_id,
            name,
            admission: AdmissionPolicyV2::Open,
            expected_revision: 11,
        } if room_id == "delivery" && name == "Delivery Room"
    ));

    let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
    terminal
        .draw(|frame| component.draw_in(frame, frame.area(), true))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("CREATE AGORA ROOM delivery"));
    assert!(rendered.contains("Applied"));
}

#[test]
fn composed_journey_prefill_still_requires_review_arm_and_confirm() {
    let (mut component, requests) = component(VecDeque::from([Ok(ReplyV2::Accepted(
        OutcomeV2::RoomCreated { revision: 12 },
    ))]));
    component
        .prefill(
            "chronos-task-17",
            "Task · chronos-task-17",
            AdmissionPolicyV2::InviteOnly,
        )
        .unwrap();
    assert_eq!(component.disposition(), CreateRoomDisposition::Editing);
    assert!(requests.lock().unwrap().is_empty());
    component.update(CreateRoomInput::Confirm, true);
    assert_eq!(component.disposition(), CreateRoomDisposition::Armed);
    assert!(requests.lock().unwrap().is_empty());
    component.update(CreateRoomInput::Confirm, true);
    settle(&mut component);
    assert_eq!(component.disposition(), CreateRoomDisposition::Applied);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(matches!(
        &requests[0].command,
        CommandV2::CreateRoom {
            room_id,
            name,
            admission: AdmissionPolicyV2::InviteOnly,
            expected_revision: 11,
        } if room_id == "chronos-task-17" && name == "Task · chronos-task-17"
    ));
}

#[test]
fn unknown_outcome_allows_only_explicit_exact_replay() {
    let (mut component, requests) = component(VecDeque::from([
        Err(PortFailureV2::OutcomeUnknown(())),
        Ok(ReplyV2::Accepted(OutcomeV2::RoomCreated { revision: 12 })),
    ]));
    enter_proposal(&mut component);
    settle(&mut component);
    assert_eq!(
        component.disposition(),
        CreateRoomDisposition::OutcomeUnknown
    );
    assert_eq!(
        component.update(CreateRoomInput::Close, true),
        Lifecycle::Running
    );
    component.update(CreateRoomInput::Confirm, true);
    assert_eq!(requests.lock().unwrap().len(), 1);
    component.update(CreateRoomInput::Character('r'), true);
    settle(&mut component);
    assert_eq!(component.disposition(), CreateRoomDisposition::Applied);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
}

#[test]
fn connector_is_deferred_until_confirmation_and_runs_off_the_update_path() {
    let connected = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let connected_by_worker = Arc::clone(&connected);
    let worker_requests = Arc::clone(&requests);
    let mut component = CreateRoomComponent::new_with_connector(
        move || -> Result<ClientV2<Port>, ()> {
            connected_by_worker.store(true, Ordering::SeqCst);
            Ok(ClientV2::new(
                ClientIdV2(7),
                Port {
                    requests: worker_requests,
                    replies: VecDeque::from([Ok(ReplyV2::Accepted(OutcomeV2::RoomCreated {
                        revision: 12,
                    }))]),
                },
            ))
        },
        11,
    );
    assert!(!connected.load(Ordering::SeqCst));
    enter_proposal(&mut component);
    settle(&mut component);
    assert!(connected.load(Ordering::SeqCst));
    assert_eq!(requests.lock().unwrap().len(), 1);
}
