#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use agora_protocol::v2::{
    AdmissionPolicyV2, ClientIdV2, CommandV2, OutcomeV2, ReplyV2, RoleV2, RoomPageV2, RoomSummaryV2,
};
use agora_wire::{
    WireLimits,
    local_handshake::{
        CLIENT_HELLO_BYTES_V1, ServerIncarnationV1, decode_client_hello_v1, encode_server_hello_v1,
    },
    v2::{WireReplyV2, WireRequestV2, decode_request_v2, encode_reply_v2},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn headless_cli_reports_connection_refusal_as_versioned_json() {
    let socket = std::env::temp_dir().join(format!(
        "agora-tui-missing-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_agora-tui"))
        .args(["headless", socket.to_str().unwrap(), "operator"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["version"], 1);
    assert_eq!(report["complete"], false);
    assert_eq!(report["capability"], "rooms.read.v2");
    assert_eq!(report["status"], "connect_refused");
}

#[test]
fn headless_cli_reads_rooms_without_constructing_terminal_state() {
    let root = std::env::temp_dir().join(format!(
        "agora-tui-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.join("agora.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let server = std::thread::spawn(move || serve(listener));

    let output = Command::new(env!("CARGO_BIN_EXE_agora-tui"))
        .args(["headless", socket.to_str().unwrap(), "operator"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["version"], 1);
    assert_eq!(report["mode"], "headless");
    assert_eq!(report["complete"], true);
    assert_eq!(report["capability"], "rooms.read.v2");
    assert_eq!(report["status"], "current");
    assert_eq!(report["revision"], 11);
    assert_eq!(
        report["rooms"],
        serde_json::json!([{"id":"profile-room", "name":"Profile Room"}])
    );
    assert_eq!(report["truncated"], false);
    server.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}

fn serve(listener: UnixListener) {
    let (mut stream, _) = listener.accept().unwrap();
    let mut hello = [0_u8; CLIENT_HELLO_BYTES_V1];
    stream.read_exact(&mut hello).unwrap();
    decode_client_hello_v1(&hello).unwrap();
    stream
        .write_all(&encode_server_hello_v1(ServerIncarnationV1::new([7; 16]), 8_192, 2).unwrap())
        .unwrap();
    let WireRequestV2::Connect { participant_id } =
        decode_request_v2(&read_frame(&mut stream), limits()).unwrap()
    else {
        panic!("connect expected");
    };
    assert_eq!(participant_id, "operator");
    write_frame(
        &mut stream,
        &encode_reply_v2(&WireReplyV2::Connected(ClientIdV2(4)), limits()).unwrap(),
    );
    let WireRequestV2::Execute { client, request } =
        decode_request_v2(&read_frame(&mut stream), limits()).unwrap()
    else {
        panic!("execute expected");
    };
    assert_eq!(client, ClientIdV2(4));
    assert!(matches!(request.command, CommandV2::Rooms { .. }));
    let reply = ReplyV2::Accepted(OutcomeV2::Rooms(RoomPageV2 {
        revision: 11,
        rooms: vec![RoomSummaryV2 {
            room_id: "profile-room".into(),
            name: "Profile Room".into(),
            admission: AdmissionPolicyV2::InviteOnly,
            observer_role: RoleV2::Member,
            member_count: 2,
            retained_message_count: 1,
            next_message_sequence: 2,
        }],
        next: None,
    }));
    write_frame(
        &mut stream,
        &encode_reply_v2(&WireReplyV2::Executed(reply), limits()).unwrap(),
    );
}

fn limits() -> WireLimits {
    WireLimits {
        max_frame_bytes: 8_192,
        max_text_bytes: 16 * 1_024,
        max_fields: 100_000,
        max_list_items: 4_096,
    }
}

fn read_frame(stream: &mut impl Read) -> Vec<u8> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).unwrap();
    let mut frame = vec![0_u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut frame).unwrap();
    frame
}

fn write_frame(stream: &mut impl Write, frame: &[u8]) {
    stream
        .write_all(&(frame.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(frame).unwrap();
}
