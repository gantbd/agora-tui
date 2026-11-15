#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

use agora_client::v2::{ClientErrorV2, ClientV2, PortFailureV2, ServicePortV2};
use agora_protocol::v2::{ClientIdV2, CommandV2, OutcomeV2, ReplyV2, RequestV2, RoomPageV2};
use agora_tui_local_client::{ConnectError, LocalClientLimits, LocalServicePort};
use agora_wire::{
    WireLimits,
    local_handshake::{
        CLIENT_HELLO_BYTES_V1, ServerIncarnationV1, decode_client_hello_v1, encode_server_hello_v1,
    },
    v2::{WireReplyV2, WireRequestV2, decode_request_v2, encode_reply_v2},
};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn socket_path() -> PathBuf {
    let directory = PathBuf::from(format!(
        "/tmp/agora-tui-client-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    directory.join("a.sock")
}

fn wire_limits() -> WireLimits {
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

fn accept_connect(listener: UnixListener) -> std::os::unix::net::UnixStream {
    let (mut stream, _) = listener.accept().unwrap();
    let mut hello = [0_u8; CLIENT_HELLO_BYTES_V1];
    stream.read_exact(&mut hello).unwrap();
    decode_client_hello_v1(&hello).unwrap();
    stream
        .write_all(&encode_server_hello_v1(ServerIncarnationV1::new([7; 16]), 8_192, 4).unwrap())
        .unwrap();
    let WireRequestV2::Connect { participant_id } =
        decode_request_v2(&read_frame(&mut stream), wire_limits()).unwrap()
    else {
        panic!("connect expected");
    };
    assert_eq!(participant_id, "alice");
    write_frame(
        &mut stream,
        &encode_reply_v2(&WireReplyV2::Connected(ClientIdV2(3)), wire_limits()).unwrap(),
    );
    stream
}

fn bind(path: &Path) -> UnixListener {
    let listener = UnixListener::bind(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    listener
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    fs::remove_dir(path.parent().unwrap()).unwrap();
}

#[test]
fn private_socket_connects_and_executes_public_v2_request() {
    let path = socket_path();
    let listener = bind(&path);
    let server = thread::spawn(move || {
        let mut stream = accept_connect(listener);
        let WireRequestV2::Execute { client, request } =
            decode_request_v2(&read_frame(&mut stream), wire_limits()).unwrap()
        else {
            panic!("execute expected");
        };
        assert_eq!(client, ClientIdV2(3));
        assert!(matches!(request.command, CommandV2::Rooms { .. }));
        write_frame(
            &mut stream,
            &encode_reply_v2(
                &WireReplyV2::Executed(ReplyV2::Accepted(OutcomeV2::Rooms(RoomPageV2 {
                    revision: 5,
                    rooms: Vec::new(),
                    next: None,
                }))),
                wire_limits(),
            )
            .unwrap(),
        );
    });
    let (client_id, port) =
        LocalServicePort::connect(&path, "alice", LocalClientLimits::default()).unwrap();
    let mut client = ClientV2::new(client_id, port);
    let ReplyV2::Accepted(OutcomeV2::Rooms(page)) = client
        .execute(CommandV2::Rooms {
            cursor: None,
            limit: 32,
        })
        .unwrap()
    else {
        panic!("rooms expected");
    };
    assert_eq!(page.revision, 5);
    server.join().unwrap();
    cleanup(&path);
}

#[test]
fn lost_reply_is_outcome_unknown_and_wrong_client_is_not_sent() {
    let path = socket_path();
    let listener = bind(&path);
    let server = thread::spawn(move || {
        let mut stream = accept_connect(listener);
        let _ = read_frame(&mut stream);
    });
    let (client_id, mut port) =
        LocalServicePort::connect(&path, "alice", LocalClientLimits::default()).unwrap();
    assert!(matches!(
        port.execute(
            ClientIdV2(99),
            RequestV2 {
                request_id: agora_protocol::v2::RequestIdV2(0),
                command: CommandV2::Capabilities,
            }
        ),
        Err(PortFailureV2::NotSent(_))
    ));
    let mut client = ClientV2::new(client_id, port);
    assert!(matches!(
        client.execute(CommandV2::Capabilities),
        Err(ClientErrorV2::OutcomeUnknown(_))
    ));
    server.join().unwrap();
    cleanup(&path);
}

#[test]
fn permissive_socket_is_refused_before_connect() {
    let path = socket_path();
    let listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(matches!(
        LocalServicePort::connect(&path, "alice", LocalClientLimits::default()),
        Err(ConnectError::UnsafeEndpoint)
    ));
    drop(listener);
    cleanup(&path);
}
