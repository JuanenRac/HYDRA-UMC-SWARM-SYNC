// HYDRA-UMC-SWARM-SYNC - tests/startup.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
//
// What a supervisor sees when the node cannot start: a failed start with a
// nonzero exit status, not a process that says it is listening and then
// exits with success.

use std::process::{Command, Stdio};

#[test]
fn a_state_file_that_cannot_be_read_stops_the_start_with_a_nonzero_status() {
    let dir = std::env::temp_dir().join(format!("swarm-sync-startup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("bad.json");
    std::fs::write(&bad, "this is not a state file").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_hydra-umc-swarm-sync"))
        .args(["serve", "--port", "0", "--state-file"])
        .arg(&bad)
        .stdin(Stdio::null())
        .output()
        .expect("the binary must run");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("could not load state"), "{stderr}");
    assert!(
        !stderr.contains("HTTP API listening"),
        "it must not announce a listener it will not keep: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_max_time_jump_that_is_not_a_number_stops_the_start() {
    let output = Command::new(env!("CARGO_BIN_EXE_hydra-umc-swarm-sync"))
        .args(["serve", "--port", "0", "--max-time-jump", "lots"])
        .stdin(Stdio::null())
        .output()
        .expect("the binary must run");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn gossip_settings_that_cannot_be_used_stop_the_start() {
    for args in [
        vec!["--peer", "https://10.0.0.7:8112"],
        vec!["--peer", "10.0.0.7"],
        vec!["--peer"],
        vec!["--gossip-interval-ms", "10"],
        vec!["--gossip-interval-ms", "often"],
        vec!["--max-generation-jump", "0"],
        vec!["--max-generation-jump", "many"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_hydra-umc-swarm-sync"))
            .args(["serve", "--port", "0"])
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .expect("the binary must run");
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
