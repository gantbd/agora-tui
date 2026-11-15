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
    let socket = PathBuf::from(args.next().ok_or_else(usage)?);
    let participant = args
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(usage)?;
    if args.next().is_some() {
        return Err(usage().into());
    }

    let (client_id, port) = agora_tui_local_client::LocalServicePort::connect(
        &socket,
        &participant,
        agora_tui_local_client::LocalClientLimits::default(),
    )
    .map_err(|error| std::io::Error::other(format!("Agora connection refused: {error:?}")))?;
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

    let reply = client
        .execute(CommandV2::Rooms {
            cursor: None,
            limit: 32,
        })
        .map_err(|error| std::io::Error::other(format!("Agora read failed: {error:?}")))?;
    match reply {
        ReplyV2::Accepted(OutcomeV2::Rooms(page)) => {
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
                    "peer": "agora",
                    "capability": "rooms.read.v2",
                    "status": "current",
                    "revision": page.revision,
                    "rooms": rooms,
                    "truncated": page.next.is_some(),
                }))?
            );
            Ok(())
        }
        ReplyV2::Refused(reason) => {
            Err(std::io::Error::other(format!("Agora read refused: {reason:?}")).into())
        }
        ReplyV2::Accepted(_) => Err(std::io::Error::other("unexpected Agora reply").into()),
    }
}

fn usage() -> &'static str {
    "usage: agora-tui <interactive|headless> SOCKET PARTICIPANT\nheadless emits one bounded versioned rooms.read.v2 JSON observation without constructing TUI state"
}

#[cfg(not(unix))]
fn main() {
    eprintln!("agora-tui's selected standalone client requires Unix");
    std::process::exit(1);
}
