# Agora TUI

`agora-tui` is an unpublished composable Ratatui presentation for Agora's
public Protocol V2 client. It owns room and message projection, selection,
input translation and rendering. It owns no terminal lifecycle, transport,
authentication, conversation state or authority.

The initial read-only slice lists one bounded room page and opens one bounded
message page. Requests run on a presentation-owned worker so service waits do
not block the terminal update path. An outcome-unknown read retains the exact
command and only `Refresh` may replay it; the component never substitutes a
different request or infers success.

Embed with `Component::new(client)`, call `poll()`, route closed `Input` values
through `update`, and render with `draw_in(frame, rect, focused)`. The host owns
terminal setup, global navigation, focus and process exit.

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
cargo run -p agora-tui-cli --bin agora-tui --locked --offline -- capabilities
```

Headless emits a versioned `rooms.read.v2` JSON observation with exact revision,
bounded room identities, truncation, freshness status, and no presentation
state.
The `capabilities` command prints the peer-owned interactive/headless manifest
without connecting to a service; it describes host composition support, not
participant authority.

```sh
cargo test --workspace --all-targets --locked --offline
sh tests/live/real_daemon.sh
```

The live gate launches Agora's real local daemon with explicitly test-only
authority inputs, then drives this standalone executable through both headless
room observation and interactive room/message projection. It does not turn the
TUI or CLI into an authority owner or define Agora's production daemon policy.
