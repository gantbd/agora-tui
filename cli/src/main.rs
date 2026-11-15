//! Standalone interactive and headless Agora application.
#![forbid(unsafe_code)]

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;

    let mut args = std::env::args_os().skip(1);
    let Some(mode) = args.next() else {
        return Err(usage().into());
    };
    if mode == "--help" || mode == "-h" {
        println!("{}", usage());
        return Ok(());
    }
    if mode == "capabilities" {
        if args.next().is_some() {
            return Err(usage().into());
        }
        let manifest = agora_tui::CAPABILITIES;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": manifest.schema,
                "peer": manifest.peer,
                "interactive": manifest.interactive,
                "headless": manifest.headless,
            }))?
        );
        return Ok(());
    }
    if mode != "interactive" && mode != "headless" {
        return Err(usage().into());
    }
    let parsed = (|| {
        let socket = PathBuf::from(args.next().ok_or_else(usage)?);
        let participant = args
            .next()
            .and_then(|value| value.into_string().ok())
            .ok_or_else(usage)?;
        if args.next().is_some() {
            return Err(usage());
        }
        Ok::<_, &'static str>((socket, participant))
    })();
    let (socket, participant) = match parsed {
        Ok(parsed) => parsed,
        Err(error) if mode == "headless" => {
            return headless_failure("configuration_refused", error);
        }
        Err(error) => return Err(error.into()),
    };

    let connection = agora_tui_local_client::LocalServicePort::connect(
        &socket,
        &participant,
        agora_tui_local_client::LocalClientLimits::default(),
    );
    let (client_id, port) = match connection {
        Ok(connection) => connection,
        Err(error) if mode == "headless" => {
            return headless_failure(
                "connect_refused",
                &format!("Agora connection refused: {error:?}"),
            );
        }
        Err(error) => {
            return Err(
                std::io::Error::other(format!("Agora connection refused: {error:?}")).into(),
            );
        }
    };
    let client = agora_client::v2::ClientV2::new(client_id, port);
    if mode == "interactive" {
        run_interactive(agora_tui::Component::new(client))
    } else if mode == "headless" {
        run_headless(client)
    } else {
        Err(usage().into())
    }
}

#[cfg(unix)]
fn run_interactive(mut component: agora_tui::Component) -> Result<(), Box<dyn std::error::Error>> {
    use crossterm::event::{self, Event};
    use std::time::Duration;

    ratatui::run(|terminal| -> Result<(), Box<dyn std::error::Error>> {
        loop {
            component.poll();
            terminal.draw(|frame| component.draw_in(frame, frame.area(), true))?;
            if !event::poll(Duration::from_millis(50))? {
                continue;
            }
            match event::read()? {
                Event::Resize(..) => {}
                Event::Key(key) => {
                    let Some(input) = agora_tui::translate_key(key) else {
                        continue;
                    };
                    if component.update(input, true) == agora_tui::Lifecycle::CloseRequested {
                        break;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })
}

#[cfg(unix)]
fn run_headless(
    mut client: agora_client::v2::ClientV2<agora_tui_local_client::LocalServicePort>,
) -> Result<(), Box<dyn std::error::Error>> {
    use agora_protocol::v2::{CommandV2, OutcomeV2, ReplyV2};

    let reply = match client.execute(CommandV2::Rooms {
        cursor: None,
        limit: 32,
    }) {
        Ok(reply) => reply,
        Err(error) => {
            use agora_client::v2::ClientErrorV2;
            let status = match error {
                ClientErrorV2::NotSent(_) => "not_sent",
                ClientErrorV2::OutcomeUnknown(_) | ClientErrorV2::UnresolvedOutcome => {
                    "outcome_unknown"
                }
                ClientErrorV2::RequestIdExhausted => "request_id_exhausted",
            };
            return headless_failure(status, "Agora read did not produce a current reply");
        }
    };
    match reply {
        ReplyV2::Accepted(OutcomeV2::Rooms(page)) => {
            let manifest = agora_tui::CAPABILITIES;
            let rooms = page
                .rooms
                .into_iter()
                .map(|room| {
                    serde_json::json!({
                        "id": room.room_id,
                        "name": room.name,
                    })
                })
                .collect::<Vec<_>>();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "version": 1,
                    "mode": "headless",
                    "complete": true,
                    "peer": manifest.peer,
                    "capability": manifest.headless[0],
                    "status": "current",
                    "revision": page.revision,
                    "rooms": rooms,
                    "truncated": page.next.is_some(),
                }))?
            );
            Ok(())
        }
        ReplyV2::Refused(reason) => {
            headless_failure("refused", &format!("Agora read refused: {reason:?}"))
        }
        ReplyV2::Accepted(_) => headless_failure("invalid_reply", "unexpected Agora reply"),
    }
}

#[cfg(unix)]
fn headless_failure(status: &str, message: &str) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = agora_tui::CAPABILITIES;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "mode": "headless",
            "complete": false,
            "peer": manifest.peer,
            "capability": manifest.headless[0],
            "status": status,
        }))?
    );
    Err(std::io::Error::other(message).into())
}

fn usage() -> &'static str {
    "usage: agora-tui capabilities | agora-tui <interactive|headless> SOCKET PARTICIPANT\ncapabilities prints the peer-owned presentation manifest without connecting; headless emits one bounded versioned rooms.read.v2 JSON observation without constructing TUI state"
}

#[cfg(not(unix))]
fn main() {
    eprintln!("agora-tui's selected standalone client requires Unix");
    std::process::exit(1);
}
