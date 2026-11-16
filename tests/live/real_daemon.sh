#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
scratch=$(mktemp -d /tmp/agora-tui-live.XXXXXX)
chmod 700 "$scratch"
target="$scratch/target"
socket="$scratch/agora.sock"
log="$scratch/daemon.log"
tmux_socket="$scratch/tmux.sock"
daemon_pid=

cleanup() {
  if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
  fi
  tmux -S "$tmux_socket" kill-server >/dev/null 2>&1 || true
  rm -rf "$scratch"
}
trap cleanup EXIT HUP INT TERM

cargo build --manifest-path "$repo/Cargo.toml" --package agora-tui-cli \
  --bin agora-tui --example real_daemon_fixture --example create_room_replay \
  --locked --offline --target-dir "$target"

start_daemon() {
  rm -f "$socket"
  "$target/debug/examples/real_daemon_fixture" "$socket" >"$log" 2>&1 &
  daemon_pid=$!
  attempt=0
  while [ ! -S "$socket" ]; do
    if ! kill -0 "$daemon_pid" 2>/dev/null || [ "$attempt" -eq 200 ]; then
      sed -n '1,120p' "$log" >&2
      echo 'FAIL: real Agora daemon fixture did not become ready' >&2
      exit 1
    fi
    attempt=$((attempt + 1))
    sleep 0.025
  done
}

finish_daemon() {
  wait "$daemon_pid"
  daemon_pid=
}

start_daemon
if "$target/debug/agora-tui" create-room "$socket" operator stale-room \
  'Stale Room' invite-only 2 \
  'CREATE AGORA ROOM stale-room NAMED Stale Room ADMISSION INVITE ONLY AT REVISION 2' \
  >"$scratch/stale.json" 2>"$scratch/stale.err"; then
  echo 'FAIL: stale Agora room creation was accepted' >&2
  exit 1
fi
finish_daemon
python3 - "$scratch/stale.json" <<'PYTHON'
import json, sys
report = json.load(open(sys.argv[1]))
assert not report["complete"]
assert report["capability"] == "rooms.create.v2"
assert report["status"] == "refused"
PYTHON
echo 'PASS: real Agora daemon refused stale standalone room creation'

start_daemon
created=$("$target/debug/agora-tui" create-room "$socket" operator delivery-room \
  'Delivery Room' invite-only 3 \
  'CREATE AGORA ROOM delivery-room NAMED Delivery Room ADMISSION INVITE ONLY AT REVISION 3')
finish_daemon
printf '%s' "$created" | python3 -c '
import json, sys
report = json.load(sys.stdin)
assert report == {
    "version": 1,
    "peer": "agora",
    "capability": "rooms.create.v2",
    "complete": True,
    "status": "created",
    "room_id": "delivery-room",
    "revision": 4,
}
'
echo 'PASS: real Agora daemon -> exact-confirmed standalone room creation'

start_daemon
"$target/debug/examples/create_room_replay" "$socket"
finish_daemon

start_daemon
headless=$("$target/debug/agora-tui" headless "$socket" operator)
finish_daemon
printf '%s' "$headless" | python3 -c '
import json, sys
report = json.load(sys.stdin)
assert report["complete"] and report["mode"] == "headless"
assert report["peer"] == "agora" and report["capability"] == "rooms.read.v2"
assert report["status"] == "current"
assert report["rooms"] == [{"id": "live-room", "name": "Live Room"}]
'
echo 'PASS: real Agora daemon -> standalone headless CLI'

start_daemon
tmux -S "$tmux_socket" -f /dev/null new-session -d -x 120 -y 28 \
  -s agora-tui "$target/debug/agora-tui interactive $socket operator"

expect() {
  marker=$1
  attempt=0
  while [ "$attempt" -lt 200 ]; do
    screen=$(tmux -S "$tmux_socket" capture-pane -p -t agora-tui)
    if printf '%s' "$screen" | grep -Fq "$marker"; then
      return
    fi
    attempt=$((attempt + 1))
    sleep 0.025
  done
  printf '%s\n' "$screen" >&2
  echo "FAIL: standalone Agora TUI omitted $marker" >&2
  exit 1
}

expect 'AGORA · rooms · revision'
expect 'Live Room'
tmux -S "$tmux_socket" send-keys -t agora-tui Enter
expect 'Standalone CLI reached the real Agora daemon'
tmux -S "$tmux_socket" send-keys -t agora-tui q

attempt=0
while tmux -S "$tmux_socket" has-session -t agora-tui 2>/dev/null; do
  if [ "$attempt" -eq 200 ]; then
    echo 'FAIL: standalone Agora TUI did not exit' >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 0.025
done
finish_daemon
echo 'PASS: real Agora daemon -> standalone interactive TUI'
