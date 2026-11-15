//! Test-only real Agora daemon for standalone CLI qualification.
#![cfg(unix)]
#![forbid(unsafe_code)]

use std::{os::unix::net::UnixStream, path::PathBuf, time::Duration};

use aegis_client::ServicePort;
use aegis_protocol::{
    DecisionHandleV1, DecisionSummaryV1, EngineNamespaceV1, OutcomeV1, ReplyV1, SessionHandleV1,
};
use agora_aegis_verifier::{AegisContextV1, AegisVerifierV1};
use agora_engine::{
    AdmissionPolicy, CommandV2, Engine, LogicalTime, Participant, ParticipantId, ParticipantKind,
    RoomId,
};
use agora_host::{HostLimits, HostTimeV1, VerifiedSubjectV1, v2::HostV2};
use agora_local_daemon::{
    AuthenticatorV1, DaemonCleanupV1, DaemonLimitsV1, DaemonV1, HostClockV1, ServerIncarnationV1,
};
use agora_service::v2::{ServiceLimitsV2, ServiceV2};
use agora_wire::WireLimits;

struct AuthorityPort;

impl ServicePort for AuthorityPort {
    type Error = ();

    fn execute(
        &mut self,
        _: SessionHandleV1,
        command: aegis_protocol::CommandV1,
    ) -> Result<ReplyV1, Self::Error> {
        let aegis_protocol::CommandV1::Authorize { action, .. } = command else {
            return Err(());
        };
        Ok(ReplyV1::Accepted(OutcomeV1::Authorized(
            DecisionSummaryV1 {
                handle: DecisionHandleV1(1),
                epoch: 1,
                expires_at: u64::MAX,
                engine: EngineNamespaceV1::Agora,
                action,
            },
        )))
    }
}

struct Authentication;

impl AuthenticatorV1 for Authentication {
    type Error = ();

    fn authenticate(&mut self, _: &UnixStream) -> Result<AegisContextV1, Self::Error> {
        Ok(AegisContextV1::new(
            "operator",
            VerifiedSubjectV1::from_verifier([7; 16]),
            SessionHandleV1(1),
            aegis_protocol::GrantHandleV1(1),
        ))
    }
}

struct Clock(u64);

impl HostClockV1 for Clock {
    fn now(&mut self) -> HostTimeV1 {
        let value = self.0;
        self.0 += 1;
        HostTimeV1(value)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("socket path required")?;
    let mut engine = Engine::new();
    engine
        .apply(CommandV2::RegisterParticipant(Participant::new(
            "operator",
            "Operator",
            ParticipantKind::Human,
        )))
        .map_err(|error| format!("participant fixture refused: {error:?}"))?;
    engine
        .apply(CommandV2::CreateRoom {
            room_id: RoomId::from("live-room"),
            name: "Live Room".into(),
            owner: ParticipantId::from("operator"),
            admission: AdmissionPolicy::InviteOnly,
        })
        .map_err(|error| format!("room fixture refused: {error:?}"))?;
    engine
        .apply(CommandV2::Speak {
            room_id: RoomId::from("live-room"),
            author: ParticipantId::from("operator"),
            at: LogicalTime(1),
            body: "Standalone CLI reached the real Agora daemon".into(),
        })
        .map_err(|error| format!("message fixture refused: {error:?}"))?;
    let host = HostV2::new(
        ServiceV2::new(engine, ServiceLimitsV2::default()),
        AegisVerifierV1::new(AuthorityPort, [5; 16]),
        HostLimits {
            max_sessions: 4,
            max_retired_sessions: 8,
            wire: WireLimits {
                max_frame_bytes: 8_192,
                max_text_bytes: 16 * 1_024,
                max_fields: 100_000,
                max_list_items: 4_096,
            },
        },
    );
    let mut daemon = DaemonV1::bind_new(
        &socket,
        host,
        Authentication,
        Clock(1),
        ServerIncarnationV1::new([9; 16]),
        DaemonLimitsV1 {
            max_frame_bytes: 8_192,
            max_frames_after_connect: 16,
            io_timeout: Duration::from_secs(2),
        },
    )
    .map_err(|error| format!("daemon bind refused: {error:?}"))?;
    daemon
        .serve_next()
        .map_err(|error| format!("daemon serve failed: {error:?}"))?;
    let _ = daemon.begin_drain();
    let stopped = daemon.stop();
    if stopped.cleanup != DaemonCleanupV1::Clean {
        return Err("daemon cleanup incomplete".into());
    }
    Ok(())
}
