//! Real-service proof that an exact room-creation retry reuses its request identity.
#![cfg(unix)]
#![forbid(unsafe_code)]

use std::path::PathBuf;

use agora_client::{
    room_command::{CreateRoomErrorV1, CreateRoomRequestV1, create_room},
    v2::{ClientV2, PortFailureV2, ServicePortV2},
};
use agora_protocol::v2::{AdmissionPolicyV2, ClientIdV2, ReplyV2, RequestV2};

struct LoseFirstReply<P> {
    inner: P,
    lose: bool,
}

impl<P: ServicePortV2> ServicePortV2 for LoseFirstReply<P> {
    type Error = &'static str;

    fn execute(
        &mut self,
        client: ClientIdV2,
        request: RequestV2,
    ) -> Result<ReplyV2, PortFailureV2<Self::Error>> {
        let reply = self
            .inner
            .execute(client, request)
            .map_err(|error| match error {
                PortFailureV2::NotSent(_) => PortFailureV2::NotSent("inner not sent"),
                PortFailureV2::OutcomeUnknown(_) => {
                    PortFailureV2::OutcomeUnknown("inner outcome unknown")
                }
            })?;
        if self.lose {
            self.lose = false;
            Err(PortFailureV2::OutcomeUnknown("discarded real reply"))
        } else {
            Ok(reply)
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("socket required")?;
    let (client_id, port) = agora_tui_local_client::LocalServicePort::connect(
        &socket,
        "operator",
        agora_tui_local_client::LocalClientLimits::default(),
    )
    .map_err(|error| format!("connect refused: {error:?}"))?;
    let mut client = ClientV2::new(
        client_id,
        LoseFirstReply {
            inner: port,
            lose: true,
        },
    );
    let selected = CreateRoomRequestV1 {
        room_id: "replayed-room".into(),
        name: "Replayed Room".into(),
        admission: AdmissionPolicyV2::InviteOnly,
        expected_revision: 3,
    };
    let confirmation =
        "CREATE AGORA ROOM replayed-room NAMED Replayed Room ADMISSION INVITE ONLY AT REVISION 3";
    assert!(matches!(
        create_room(&mut client, selected.clone(), confirmation),
        Err(CreateRoomErrorV1::Client(
            agora_client::v2::ClientErrorV2::OutcomeUnknown("discarded real reply")
        ))
    ));
    let receipt = create_room(&mut client, selected, confirmation)
        .map_err(|error| format!("exact replay failed: {error:?}"))?;
    if receipt.revision != 4 {
        return Err(format!("unexpected replay receipt: {receipt:?}").into());
    }
    println!("PASS: exact Agora room creation replay returned revision 4");
    Ok(())
}
