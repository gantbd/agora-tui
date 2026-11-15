# Agora TUI roadmap

`agora-tui` is the reusable presentation component. It remains separate from
both Agora's standalone CLI/application and Workbench's CLI/application.

## Current delivery

The separately packaged `agora-tui-cli` now owns argument parsing, selected
Unix transport, terminal lifecycle, and process exit. Its interactive mode
hosts the reusable component. Its headless mode reads the same public client
directly and has a private-socket integration test; it constructs no TUI state.
The peer publishes a closed interactive/headless presentation capability
manifest, and the standalone CLI can disclose it without connecting. Hosts may
negotiate that manifest, but Agora's service remains the authority owner.
Standalone headless failures retain the same versioned envelope and distinguish
connection refusal, not-sent access, outcome-unknown delivery, refusal, and
invalid replies while exiting unsuccessfully.

## Delivery order

1. Extend the current read-only room/message projection through public Agora
   client contracts, preserving bounds, revision binding, and explicit
   unavailable or outcome-unknown states.
2. Add mutations only with exact confirmation, invocation identity, current
   authorization, and replay/reconciliation behavior supplied by Agora.
3. Keep terminal lifecycle, argument parsing, transport selection, daemon
   startup, global shortcuts, and process exit out of this crate.
4. [Initial slice qualified] Qualify the component under two hosts: Agora's standalone CLI and Workbench.
   Both must route the same closed input and lifecycle outputs.
5. [Qualified] Standalone and Workbench headless paths use Agora's public client
   contract directly, report the explicit `rooms.read.v2` capability, and
   construct neither the component nor fabricated cursor state.

The standalone Agora CLI may run this TUI interactively or execute bounded
headless commands. Workbench may reuse this TUI while attaching to Agora or
hosting its service runtime under one composite lifecycle. Neither host acquires
Agora room/message authority from the presentation.
