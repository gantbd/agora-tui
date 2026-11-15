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

`agora-tui-local-client` is the selected private Unix interpretation for a
standalone host. It validates the socket type, mode and owner, negotiates the
bounded Agora handshake, enforces the server request budget and classifies
pre-dispatch failures separately from outcome-unknown transmission. The
participant value remains an identity claim checked against daemon-established
peer facts; it grants no authority.

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
```

```sh
cargo test --workspace --all-targets --locked --offline
```
