use std::{fs, path::Path};

#[test]
fn presentation_depends_only_on_agoras_public_client_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(root.join("Cargo.toml")).expect("manifest");
    assert!(manifest.contains("agora-client"));
    assert!(manifest.contains("agora-protocol"));
    for forbidden in [
        "agora-engine",
        "agora-host",
        "agora-service",
        "agora-local-daemon",
        "agora-local-transport",
    ] {
        assert!(!manifest.contains(forbidden), "imports {forbidden}");
    }
    assert!(manifest.contains("publish = false"));

    let readme = fs::read_to_string(root.join("README.md")).expect("README");
    assert!(readme.contains("owns no terminal lifecycle"));
    assert!(readme.contains("agora-tui-local-client"));

    let cli = fs::read_to_string(root.join("cli/Cargo.toml")).expect("CLI manifest");
    assert!(cli.contains("name = \"agora-tui-cli\""));
    assert!(cli.contains("agora-tui = { path = \"..\" }"));
    let entry = fs::read_to_string(root.join("cli/src/main.rs")).expect("CLI entry point");
    assert!(entry.contains("interactive"));
    assert!(entry.contains("headless"));
    assert!(entry.contains("Component::new"));
    assert!(entry.contains("CommandV2::Rooms"));
    assert!(entry.contains("agora_tui::CAPABILITIES"));
    assert!(
        fs::read_to_string(root.join("src/lib.rs"))
            .unwrap()
            .contains("messages.read.v2")
    );
}

#[test]
fn standalone_live_gate_uses_the_real_daemon_in_both_modes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gate = fs::read_to_string(root.join("tests/live/real_daemon.sh"))
        .expect("real-daemon live gate");
    for required in [
        "real_daemon_fixture",
        "headless",
        "interactive",
        "rooms.read.v2",
        "tmux",
        "--locked",
        "--offline",
    ] {
        assert!(gate.contains(required), "live gate omits {required}");
    }
    for forbidden in ["cargo publish", "git push", "gh release", "curl "] {
        assert!(!gate.contains(forbidden), "live gate performs {forbidden}");
    }
}
