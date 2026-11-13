// HYDRA-UMC-SWARM-SYNC - tests/cli_tools.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
//
// What an operator sees from the small commands (`writer-id`, `reseal`) and from the refusals of `serve` that come before any port is bound
// (the clock configuration, the retention window, the identity key). None of these starts a server.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_hydra-umc-swarm-sync"))
        .args(args)
        .env_remove("SWARM_SYNC_SHARED_SECRET")
        .env_remove("SWARM_SYNC_STATE_FILE")
        .env_remove("SWARM_SYNC_CLOCK_CONFIG")
        .env_remove("HYDRA_UMC_SWARM_SYNC_IDENTITY_KEY_FILE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "swarm-sync-tools-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn file_in(dir: &Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path.to_string_lossy().to_string()
}

#[test]
fn writer_id_prints_a_stable_number_that_differs_per_user() {
    let (code, first, _) = run(&["writer-id", "alice"]);
    assert_eq!(code, Some(0));
    let (_, again, _) = run(&["writer-id", "alice"]);
    let (_, other, _) = run(&["writer-id", "bob"]);
    assert_eq!(first, again);
    assert_ne!(first, other);
    assert!(first.trim().parse::<u64>().is_ok(), "{first}");
}

#[test]
fn writer_id_needs_a_user_name() {
    for args in [
        vec!["writer-id"],
        vec!["writer-id", ""],
        vec!["writer-id", "--x"],
    ] {
        let (code, _, stderr) = run(&args);
        assert_eq!(code, Some(2), "{args:?}");
        assert!(
            stderr.contains("Usage: hydra-umc-swarm-sync writer-id"),
            "{stderr}"
        );
    }
}

#[test]
fn reseal_refuses_every_bad_invocation_before_it_writes_anything() {
    let dir = scratch_dir();
    let state = file_in(
        &dir,
        "state.json",
        r#"{"entries":[{"key":"a","value":"1","time":1,"writer":7,"generation":1}]}"#,
    );
    let out = dir.join("out.json").to_string_lossy().to_string();

    let (code, _, stderr) = run(&["reseal"]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("Usage: hydra-umc-swarm-sync reseal"),
        "{stderr}"
    );

    let (code, _, stderr) = run(&["reseal", "--state-file", &state, "--out", &out]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("at least one --map"), "{stderr}");

    let (code, _, stderr) = run(&[
        "reseal",
        "--state-file",
        &state,
        "--out",
        &out,
        "--map",
        "7",
    ]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("--map needs OLD=USER"), "{stderr}");

    let (code, _, stderr) = run(&[
        "reseal",
        "--state-file",
        &state,
        "--out",
        &out,
        "--map",
        "seven=alice",
    ]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("is not a writer number"), "{stderr}");

    let (code, _, stderr) = run(&[
        "reseal",
        "--state-file",
        &state,
        "--out",
        &state,
        "--map",
        "7=alice",
    ]);
    assert_eq!(code, Some(2));
    assert!(stderr.contains("never overwrites"), "{stderr}");

    let missing = dir.join("no-such-state.json").to_string_lossy().to_string();
    let (code, _, stderr) = run(&[
        "reseal",
        "--state-file",
        &missing,
        "--out",
        &out,
        "--map",
        "7=alice",
    ]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("could not read"), "{stderr}");

    let junk = file_in(&dir, "junk.json", "not json");
    let (code, _, stderr) = run(&[
        "reseal",
        "--state-file",
        &junk,
        "--out",
        &out,
        "--map",
        "7=alice",
    ]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("not valid JSON"), "{stderr}");
    assert!(!Path::new(&out).exists(), "a refused reseal writes nothing");
}

#[test]
fn reseal_gives_the_entries_of_the_old_writer_to_the_writer_of_the_user_and_leaves_the_input_alone()
{
    let dir = scratch_dir();
    let text = r#"{"entries":[{"key":"a","value":"1","time":1,"writer":7,"generation":1},{"key":"b","value":null,"time":2,"writer":8,"generation":1}]}"#;
    let state = file_in(&dir, "state.json", text);
    let out = dir.join("resealed.json").to_string_lossy().to_string();
    let (code, stdout, stderr) = run(&[
        "reseal",
        "--state-file",
        &state,
        "--out",
        &out,
        "--map",
        "7=alice",
    ]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("1 entries resealed"), "{stdout}");
    let (_, id, _) = run(&["writer-id", "alice"]);
    let resealed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(resealed["entries"][0]["writer"].to_string(), id.trim());
    assert_eq!(resealed["entries"][1]["writer"], 8);
    assert_eq!(std::fs::read_to_string(&state).unwrap(), text);
}

#[test]
fn serve_refuses_a_clock_configuration_a_retention_window_and_an_identity_key_it_cannot_use() {
    let dir = scratch_dir();
    let missing = dir.join("no-such.json").to_string_lossy().to_string();
    let bad_clock = file_in(
        &dir,
        "bad-clock.json",
        r#"{"clock_sync":{"enabled":true,"interval_ms":5}}"#,
    );
    let not_json = file_in(&dir, "not-json.json", "{{{");
    for (args, fragment) in [
        (
            vec!["serve", "--port", "0", "--clock-sync-config", &missing],
            "could not read",
        ),
        (
            vec!["serve", "--port", "0", "--clock-sync-config", &bad_clock],
            "interval_ms",
        ),
        (
            vec!["serve", "--port", "0", "--clock-sync-config", &not_json],
            "configuration",
        ),
        (
            vec!["serve", "--port", "0", "--tombstone-retention-ms", "1000"],
            "at least 60000",
        ),
        (
            vec!["serve", "--port", "0", "--tombstone-retention-ms", "soon"],
            "at least 60000",
        ),
        (
            vec!["serve", "--port", "0", "--identity-key-file", &missing],
            "--identity-key-file",
        ),
    ] {
        let (code, _, stderr) = run(&args);
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains(fragment), "{args:?}: {stderr}");
        assert!(!stderr.contains("HTTP API listening"), "{args:?}: {stderr}");
    }
}
