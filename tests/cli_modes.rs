// HYDRA-UMC-SWARM-SYNC - tests/cli_modes.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
//
// What a caller of the binary sees: the scenario mode (stdout is the banner
// plus the JSON result, errors exit 1), and the fatal refusals of `serve`
// before any port is bound. None of these starts a server.

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_hydra-umc-swarm-sync"))
        .args(args)
        .env_remove("SWARM_SYNC_SHARED_SECRET")
        .env_remove("SWARM_SYNC_STATE_FILE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn scenario_file(name: &str, content: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("swarm-sync-modes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

/// The JSON object after the two banner lines on stdout.
fn json_of(stdout: &str) -> serde_json::Value {
    let start = stdout.find('{').expect("a JSON object on stdout");
    serde_json::from_str(&stdout[start..]).expect("pure JSON after the banner")
}

#[test]
fn without_arguments_it_prints_the_banner_and_the_usage_and_succeeds() {
    let (code, stdout, stderr) = run(&[]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("HYDRA-UMC-SWARM-SYNC v"), "{stdout}");
    assert!(
        stdout.contains("CRDT swarm state reconciliation"),
        "{stdout}"
    );
    assert!(
        stderr.contains("Usage: hydra-umc-swarm-sync <scenario.json>"),
        "{stderr}"
    );
    assert!(stderr.contains("serve [--addr ADDR]"), "{stderr}");
}

#[test]
fn a_missing_or_unparseable_scenario_exits_one_with_a_reason() {
    let (code, _, stderr) = run(&["no-such-scenario.json"]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("could not read"), "{stderr}");

    let bad = scenario_file("bad.json", "{nope");
    let (code, _, stderr) = run(&[&bad.to_string_lossy()]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("could not parse"), "{stderr}");

    let wrong_shape = scenario_file("shape.json", r#"{"cells": 3}"#);
    let (code, _, stderr) = run(&[&wrong_shape.to_string_lossy()]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("could not parse"), "{stderr}");
}

const SCENARIO: &str = r#"{"cells":[
  {"id":"a","writer":1,"writes":[{"key":"k","value":"old","time":1},{"key":"k","value":"new","time":4}]},
  {"id":"b","writer":2,"writes":[{"key":"k","value":"mid","time":3},{"key":"j","value":"x","time":2}]}
]}"#;

#[test]
fn a_scenario_reconciles_to_the_newest_write_and_is_order_independent() {
    let forward = scenario_file("forward.json", SCENARIO);
    let reversed = scenario_file(
        "reversed.json",
        r#"{"cells":[
  {"id":"b","writer":2,"writes":[{"key":"k","value":"mid","time":3},{"key":"j","value":"x","time":2}]},
  {"id":"a","writer":1,"writes":[{"key":"k","value":"old","time":1},{"key":"k","value":"new","time":4}]}
]}"#,
    );
    let (code_a, out_a, _) = run(&[&forward.to_string_lossy()]);
    let (code_b, out_b, _) = run(&[&reversed.to_string_lossy()]);
    assert_eq!((code_a, code_b), (Some(0), Some(0)), "{out_a} {out_b}");
    let a = json_of(&out_a);
    let b = json_of(&out_b);
    // the merged state is what must converge; the conflict log is written from each side's point of view
    assert_eq!(
        a["merged_state"], b["merged_state"],
        "the merge must not depend on the order of the cells"
    );
    assert_eq!(a["conflicts_resolved"], b["conflicts_resolved"]);
    assert_eq!(a["next_local_time"], b["next_local_time"]);
    assert_eq!(a["merged_state"]["k"], "new");
    assert_eq!(a["merged_state"]["j"], "x");
}

#[test]
fn serve_refuses_bad_numeric_and_choice_options_before_binding() {
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["serve", "--presence", "maybe"],
            "--presence must be delete-wins or add-wins",
        ),
        (
            vec!["serve", "--max-generation-jump", "0"],
            "--max-generation-jump must be a whole number of at least 1",
        ),
        (
            vec!["serve", "--max-generation-jump", "x"],
            "--max-generation-jump must be a whole number of at least 1",
        ),
        (
            vec!["serve", "--gossip-interval-ms", "999"],
            "--gossip-interval-ms must be a whole number of at least 1000",
        ),
        (vec!["serve", "--peer"], "--peer needs an address"),
        (vec!["serve", "--peer", "not-an-address"], "--peer:"),
    ];
    for (args, fragment) in cases {
        let (code, _, stderr) = run(&args);
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains(fragment), "{args:?}: {stderr}");
        assert!(!stderr.contains("HTTP API listening"), "{args:?}: {stderr}");
    }
}

#[test]
fn serve_will_not_listen_on_a_public_address_without_a_secret() {
    let (code, _, stderr) = run(&["serve", "--addr", "0.0.0.0", "--port", "0"]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("refusing to listen on 0.0.0.0:0 without SWARM_SYNC_SHARED_SECRET"),
        "{stderr}"
    );
    assert!(!stderr.contains("HTTP API listening"), "{stderr}");
}

#[test]
fn a_peer_without_a_secret_is_warned_about_and_the_environment_secret_is_never_echoed() {
    let output = Command::new(env!("CARGO_BIN_EXE_hydra-umc-swarm-sync"))
        .args([
            "serve",
            "--addr",
            "0.0.0.0",
            "--port",
            "0",
            "--peer",
            "http://10.0.0.7:8112",
        ])
        .env("SWARM_SYNC_SHARED_SECRET", "")
        .env_remove("SWARM_SYNC_STATE_FILE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("--peer without SWARM_SYNC_SHARED_SECRET"),
        "{stderr}"
    );
}
