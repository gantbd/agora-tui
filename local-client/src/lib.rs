//! Selected private Unix transport for the standalone Agora presentation.
#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("agora-tui-local-client currently supports only Unix hosts");

use std::{
    fs,
    io::{self, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::Path,
    time::Duration,
};

use agora_client::v2::{PortFailureV2, ServicePortV2};
use agora_protocol::v2::{ClientIdV2, RefusalV2, ReplyV2, RequestV2};
use agora_wire::{
    WireLimits,
    local_handshake::{
        COMPOSITION_SERVER_HELLO_BYTES_V1, SERVER_HELLO_BYTES_V1,
        decode_composition_server_hello_v1, decode_server_hello_v1, encode_client_hello_v1,
        encode_composition_client_hello_v1,
    },
    local_transient::{
        LocalRouteV1, decode_local_routed_frame_v1, decode_local_session_hello_v1,
        encode_local_routed_frame_v1,
    },
    v2::{WireReplyV2, WireRequestV2, decode_reply_v2, encode_request_v2},
};

const MAX_PARTICIPANT_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalClientLimits {
    pub io_timeout: Duration,
    pub max_text_bytes: usize,
    pub max_fields: usize,
    pub max_list_items: usize,
}

impl Default for LocalClientLimits {
    fn default() -> Self {
        Self {
            io_timeout: Duration::from_secs(5),
            max_text_bytes: 16 * 1_024,
            max_fields: 100_000,
            max_list_items: 4_096,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectError {
    UnsafeEndpoint,
    InvalidParticipant,
    Io,
    Handshake,
    Encode,
    Decode,
    Refused(RefusalV2),
    UnexpectedReply,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalAccessError {
    WrongClient,
    RequestBudgetExhausted,
    Encode,
    FrameLimit,
    Io,
    Decode,
    UnexpectedReply,
}

pub struct LocalServicePort {
    stream: UnixStream,
    client: ClientIdV2,
    wire_limits: WireLimits,
    remaining_requests: u32,
    routed: bool,
    connected: bool,
}

impl LocalServicePort {
    pub fn connect(
        socket: &Path,
        participant: &str,
        limits: LocalClientLimits,
    ) -> Result<(ClientIdV2, Self), ConnectError> {
        validate_endpoint(socket)?;
        if participant.is_empty()
            || participant.len() > MAX_PARTICIPANT_BYTES
            || participant.chars().any(char::is_control)
            || limits.io_timeout.is_zero()
            || limits.max_text_bytes == 0
            || limits.max_fields == 0
            || limits.max_list_items == 0
        {
            return Err(ConnectError::InvalidParticipant);
        }
        let mut stream = UnixStream::connect(socket).map_err(|_| ConnectError::Io)?;
        stream
            .set_read_timeout(Some(limits.io_timeout))
            .map_err(|_| ConnectError::Io)?;
        stream
            .set_write_timeout(Some(limits.io_timeout))
            .map_err(|_| ConnectError::Io)?;
        stream
            .write_all(&encode_client_hello_v1())
            .map_err(|_| ConnectError::Io)?;
        let mut hello_bytes = [0_u8; SERVER_HELLO_BYTES_V1];
        stream
            .read_exact(&mut hello_bytes)
            .map_err(|_| ConnectError::Io)?;
        let hello = decode_server_hello_v1(&hello_bytes).map_err(|_| ConnectError::Handshake)?;
        let wire_limits = WireLimits {
            max_frame_bytes: hello.max_frame_bytes() as usize,
            max_text_bytes: limits.max_text_bytes,
            max_fields: limits.max_fields,
            max_list_items: limits.max_list_items,
        };
        let connect = encode_request_v2(
            &WireRequestV2::Connect {
                participant_id: participant.to_owned(),
            },
            wire_limits,
        )
        .map_err(|_| ConnectError::Encode)?;
        write_frame_connect(&mut stream, &connect)?;
        let reply = read_frame_connect(&mut stream, wire_limits.max_frame_bytes)?;
        let client = match decode_reply_v2(&reply, wire_limits).map_err(|_| ConnectError::Decode)? {
            WireReplyV2::Connected(client) => client,
            WireReplyV2::ConnectRefused(refusal) => return Err(ConnectError::Refused(refusal)),
            _ => return Err(ConnectError::UnexpectedReply),
        };
        Ok((
            client,
            Self {
                stream,
                client,
                wire_limits,
                remaining_requests: hello.max_frames_after_connect(),
                routed: false,
                connected: true,
            },
        ))
    }

    /// Connects the same public conversation client through Agora's additive
    /// composition transport. Routing changes framing only; protocol,
    /// participant claim, request budget, and service authority are unchanged.
    pub fn connect_composition(
        socket: &Path,
        participant: &str,
        limits: LocalClientLimits,
    ) -> Result<(ClientIdV2, Self), ConnectError> {
        validate_endpoint(socket)?;
        if participant.is_empty()
            || participant.len() > MAX_PARTICIPANT_BYTES
            || participant.chars().any(char::is_control)
            || limits.io_timeout.is_zero()
            || limits.max_text_bytes == 0
            || limits.max_fields == 0
            || limits.max_list_items == 0
        {
            return Err(ConnectError::InvalidParticipant);
        }
        let mut stream = UnixStream::connect(socket).map_err(|_| ConnectError::Io)?;
        stream
            .set_read_timeout(Some(limits.io_timeout))
            .map_err(|_| ConnectError::Io)?;
        stream
            .set_write_timeout(Some(limits.io_timeout))
            .map_err(|_| ConnectError::Io)?;
        stream
            .write_all(&encode_composition_client_hello_v1())
            .map_err(|_| ConnectError::Io)?;
        let mut hello_bytes = [0_u8; COMPOSITION_SERVER_HELLO_BYTES_V1];
        stream
            .read_exact(&mut hello_bytes)
            .map_err(|_| ConnectError::Io)?;
        let hello = decode_composition_server_hello_v1(&hello_bytes)
            .map_err(|_| ConnectError::Handshake)?;
        let wire_limits = WireLimits {
            max_frame_bytes: hello.max_frame_bytes() as usize,
            max_text_bytes: limits.max_text_bytes,
            max_fields: limits.max_fields,
            max_list_items: limits.max_list_items,
        };
        let connect = encode_request_v2(
            &WireRequestV2::Connect {
                participant_id: participant.to_owned(),
            },
            wire_limits,
        )
        .map_err(|_| ConnectError::Encode)?;
        let connect = encode_local_routed_frame_v1(
            LocalRouteV1::Conversation,
            &connect,
            wire_limits.max_frame_bytes,
        )
        .map_err(|_| ConnectError::Encode)?;
        write_frame_connect(&mut stream, &connect)?;
        let reply = read_frame_connect(&mut stream, wire_limits.max_frame_bytes)?;
        let reply = decode_local_routed_frame_v1(&reply, wire_limits.max_frame_bytes)
            .map_err(|_| ConnectError::Decode)?;
        if reply.route() != LocalRouteV1::Conversation {
            return Err(ConnectError::Decode);
        }
        let client = match decode_reply_v2(reply.payload(), wire_limits)
            .map_err(|_| ConnectError::Decode)?
        {
            WireReplyV2::Connected(client) => client,
            WireReplyV2::ConnectRefused(refusal) => return Err(ConnectError::Refused(refusal)),
            _ => return Err(ConnectError::UnexpectedReply),
        };
        let control = read_frame_connect(&mut stream, wire_limits.max_frame_bytes)?;
        let control = decode_local_routed_frame_v1(&control, wire_limits.max_frame_bytes)
            .map_err(|_| ConnectError::Decode)?;
        if control.route() != LocalRouteV1::Control
            || decode_local_session_hello_v1(control.payload()).is_err()
        {
            return Err(ConnectError::Decode);
        }
        Ok((
            client,
            Self {
                stream,
                client,
                wire_limits,
                remaining_requests: hello.max_frames_after_connect(),
                routed: true,
                connected: true,
            },
        ))
    }

    fn disconnect(&mut self) {
        if !self.connected {
            return;
        }
        self.connected = false;
        let Ok(request) =
            encode_request_v2(&WireRequestV2::Disconnect(self.client), self.wire_limits)
        else {
            return;
        };
        let request = if self.routed {
            let Ok(routed) = encode_local_routed_frame_v1(
                LocalRouteV1::Conversation,
                &request,
                self.wire_limits.max_frame_bytes,
            ) else {
                return;
            };
            routed
        } else {
            request
        };
        if write_frame(&mut self.stream, &request).is_err() {
            return;
        }
        let Ok(reply) = read_frame(&mut self.stream, self.wire_limits.max_frame_bytes) else {
            return;
        };
        let reply = if self.routed {
            let Ok(routed) = decode_local_routed_frame_v1(&reply, self.wire_limits.max_frame_bytes)
            else {
                return;
            };
            if routed.route() != LocalRouteV1::Conversation {
                return;
            }
            routed.payload()
        } else {
            &reply
        };
        let _ = matches!(
            decode_reply_v2(reply, self.wire_limits),
            Ok(WireReplyV2::Disconnected)
        );
    }
}

impl Drop for LocalServicePort {
    fn drop(&mut self) {
        self.disconnect();
    }
}

impl ServicePortV2 for LocalServicePort {
    type Error = LocalAccessError;

    fn execute(
        &mut self,
        client: ClientIdV2,
        request: RequestV2,
    ) -> Result<ReplyV2, PortFailureV2<Self::Error>> {
        if client != self.client {
            return Err(PortFailureV2::NotSent(LocalAccessError::WrongClient));
        }
        if self.remaining_requests == 0 {
            return Err(PortFailureV2::NotSent(
                LocalAccessError::RequestBudgetExhausted,
            ));
        }
        let frame = encode_request_v2(
            &WireRequestV2::Execute { client, request },
            self.wire_limits,
        )
        .map_err(|_| PortFailureV2::NotSent(LocalAccessError::Encode))?;
        let frame = if self.routed {
            encode_local_routed_frame_v1(
                LocalRouteV1::Conversation,
                &frame,
                self.wire_limits.max_frame_bytes,
            )
            .map_err(|_| PortFailureV2::NotSent(LocalAccessError::FrameLimit))?
        } else {
            frame
        };
        write_frame(&mut self.stream, &frame)?;
        self.remaining_requests -= 1;
        let reply = read_frame(&mut self.stream, self.wire_limits.max_frame_bytes)?;
        let reply = if self.routed {
            let routed = decode_local_routed_frame_v1(&reply, self.wire_limits.max_frame_bytes)
                .map_err(|_| PortFailureV2::OutcomeUnknown(LocalAccessError::Decode))?;
            if routed.route() != LocalRouteV1::Conversation {
                return Err(PortFailureV2::OutcomeUnknown(
                    LocalAccessError::UnexpectedReply,
                ));
            }
            routed.payload()
        } else {
            &reply
        };
        match decode_reply_v2(reply, self.wire_limits)
            .map_err(|_| PortFailureV2::OutcomeUnknown(LocalAccessError::Decode))?
        {
            WireReplyV2::Executed(reply) => Ok(reply),
            _ => Err(PortFailureV2::OutcomeUnknown(
                LocalAccessError::UnexpectedReply,
            )),
        }
    }
}

fn validate_endpoint(path: &Path) -> Result<(), ConnectError> {
    let parent = path.parent().ok_or(ConnectError::UnsafeEndpoint)?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|_| ConnectError::UnsafeEndpoint)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| ConnectError::UnsafeEndpoint)?;
    let effective_uid = rustix::process::geteuid().as_raw();
    if !parent_metadata.file_type().is_dir()
        || parent_metadata.permissions().mode() & 0o022 != 0
        || parent_metadata.uid() != effective_uid
        || !metadata.file_type().is_socket()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != effective_uid
    {
        return Err(ConnectError::UnsafeEndpoint);
    }
    Ok(())
}

fn write_frame(
    stream: &mut UnixStream,
    payload: &[u8],
) -> Result<(), PortFailureV2<LocalAccessError>> {
    let length = u32::try_from(payload.len())
        .map_err(|_| PortFailureV2::NotSent(LocalAccessError::FrameLimit))?
        .to_be_bytes();
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length);
    frame.extend_from_slice(payload);
    let mut sent = 0;
    while sent < frame.len() {
        match stream.write(&frame[sent..]) {
            Ok(0) => return Err(classify_io(sent)),
            Ok(count) => sent += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(classify_io(sent)),
        }
    }
    Ok(())
}

fn classify_io(sent: usize) -> PortFailureV2<LocalAccessError> {
    if sent == 0 {
        PortFailureV2::NotSent(LocalAccessError::Io)
    } else {
        PortFailureV2::OutcomeUnknown(LocalAccessError::Io)
    }
}

fn read_frame(
    stream: &mut UnixStream,
    max_frame_bytes: usize,
) -> Result<Vec<u8>, PortFailureV2<LocalAccessError>> {
    let mut length = [0_u8; 4];
    stream
        .read_exact(&mut length)
        .map_err(|_| PortFailureV2::OutcomeUnknown(LocalAccessError::Io))?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > max_frame_bytes {
        return Err(PortFailureV2::OutcomeUnknown(LocalAccessError::FrameLimit));
    }
    let mut frame = vec![0_u8; length];
    stream
        .read_exact(&mut frame)
        .map_err(|_| PortFailureV2::OutcomeUnknown(LocalAccessError::Io))?;
    Ok(frame)
}

fn write_frame_connect(stream: &mut UnixStream, payload: &[u8]) -> Result<(), ConnectError> {
    let length = u32::try_from(payload.len())
        .map_err(|_| ConnectError::Encode)?
        .to_be_bytes();
    stream.write_all(&length).map_err(|_| ConnectError::Io)?;
    stream.write_all(payload).map_err(|_| ConnectError::Io)
}

fn read_frame_connect(
    stream: &mut UnixStream,
    max_frame_bytes: usize,
) -> Result<Vec<u8>, ConnectError> {
    let mut length = [0_u8; 4];
    stream
        .read_exact(&mut length)
        .map_err(|_| ConnectError::Io)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > max_frame_bytes {
        return Err(ConnectError::Decode);
    }
    let mut frame = vec![0_u8; length];
    stream
        .read_exact(&mut frame)
        .map_err(|_| ConnectError::Io)?;
    Ok(frame)
}
