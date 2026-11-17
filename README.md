# Agora TUI

`agora-tui` is a source-distributed composable Ratatui presentation for Agora's
public Protocol V2 client. It owns room and message projection, selection,
input translation and rendering. It owns no terminal lifecycle, transport,
authentication, conversation state or authority.

The observation surface lists one bounded room page, then keeps a
room navigator beside a wrapped author/message timeline so room context does
not disappear while reading. Requests run on a presentation-owned worker so
service waits do not block the terminal update path. An outcome-unknown read
retains the exact command and only `Refresh` may replay it; the component never
substitutes a different request or infers success.

Embed with `Component::new(client)`, call `poll()`, route closed `Input` values
through `update`, and render with `draw_in(frame, rect, focused)`. The host owns
terminal setup, global navigation, focus and process exit.

Hosts with Agora control authority may separately compose
`CreateRoomComponent`. Its bounded form captures room identity, name, admission
policy, and the exact observed room revision; arm/confirm submits once on a
worker. Pending and outcome-unknown requests cannot be dismissed, and recovery
requires an explicit replay of the byte-identical retained request on the same
client session.

`ComposeMessageComponent` is the separate write-authority surface for real
conversation work. It accepts a bounded multiline body and an optional
comma-separated set of up to 64 participant ids, canonicalizes that complete
set, and binds delivery to the room revision and owner-provided next sequence
observed by `Component::message_target()`. Arm/confirm performs one attempt on
a worker and reports the owner revision and message sequence. Content and
recipient identities remain out of receipts and instrumentation; uncertain
delivery fences dismissal and permits only exact retained-request replay.
When embedded beside `Component`, construct controls from
`Component::control_handle()`. This serializes observation and guarded effects
through the same peer-owned `ClientV2` request namespace, matching Agora's
single-session local owner and preserving one uncertainty fence. The connector
constructors remain available for standalone control-only hosts.

`agora-local-client`, owned by Agora, supplies explicit private Unix
interpretations for the conversation-only and routed composition transports;
`agora-tui-local-client` is a compatibility facade. Both transports validate the socket
type, mode and owner, negotiate the bounded Agora handshake, enforce the server
request budget, close the logical session before transport teardown, and
classify pre-dispatch failures separately from outcome-unknown transmission.
Routing changes framing only: the public Protocol V2 client and participant
claim are unchanged, and the claim grants no authority.

See [ROADMAP.md](ROADMAP.md) for the standalone CLI, Workbench composition, and
headless-parity delivery boundary.

The separate `agora-tui-cli` package owns argument parsing, Unix transport
selection, terminal lifecycle, and process exit. It can host the component or
read the same public client directly without constructing UI state:

```sh
cargo run -p agora-tui-cli --bin agora-tui --locked --offline -- \
  interactive /private/tmp/agora.sock operator
cargo run -p agora-tui-cli --bin agora-tui --locked --offline -- \
  headless /private/tmp/agora.sock operator
cargo run -p agora-tui-cli --bin agora-tui --locked --offline -- \
  create-room /private/tmp/agora.sock operator delivery 'Delivery Room' \
  invite-only 7 \
  'CREATE AGORA ROOM delivery NAMED Delivery Room ADMISSION INVITE ONLY AT REVISION 7'
cargo run -p agora-tui-cli --bin agora-tui --locked --offline -- capabilities
```

Headless emits a versioned `rooms.read.v2` JSON observation with exact revision,
bounded room identities, truncation, freshness status, and no presentation
state. The separate `create-room` command reuses Agora's owner-client
`rooms.create.v2` guard. It requires an exact room/revision confirmation before
connection, attempts the actor-bound mutation once, and reports the durable
revision receipt or an explicit uncertain outcome.
The `capabilities` command prints the peer-owned interactive/headless manifest
without connecting to a service; it describes host composition support, not
participant authority.

```sh
make check
sh tests/live/real_daemon.sh
```

The live gate launches Agora's real local daemon with explicitly test-only
authority inputs, then drives this standalone executable through exact-confirmed
room creation, stale-revision refusal, deliberately lost-reply exact replay,
headless room observation and interactive room/message projection. It does not
turn the TUI or CLI into an authority owner or define Agora's production daemon
policy.
