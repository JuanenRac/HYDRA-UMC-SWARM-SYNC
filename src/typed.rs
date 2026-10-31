// =============================================================================
// HYDRA-UMC-SWARM-SYNC - src/typed.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! Typed values: what a node replicates under the key `robot_state/<robot_id>` is a `RobotState` of the SDK, not a free string.
//!
//! The map itself stays a map of strings (it merges any key), but a value under the prefix `robot_state/` is checked, before it
//! touches the map, against the `RobotState` contract of the SDK (vendored under `schemas/`, Draft 2020-12, formats checked): it is
//! JSON, it passes the schema, its `robot_id` is the one in the key and the quaternion of its pose has length 1. A deletion
//! (a tombstone) has no value and is always accepted. Every other key is untouched.
//!
//! Two more kinds of value are typed, the ones a pair of orchestrators needs to share: `mission/<mission_id>` (a `MissionRecord`) and
//! `zone_reservation/<zone_id>` (a `ZoneReservation`). Both carry an `attempt`, the fencing token: the attempt is also the GENERATION of
//! the entry (`attempt + 1`, since generations start at 1), and the generation is compared before the time. So the record of a newer
//! attempt wins over any record of an older one whatever their clocks say, and a node that was cut off and writes late with its old
//! attempt loses on every node once the two meet again. A write whose generation is not `attempt + 1` is refused.

use std::sync::OnceLock;

use jsonschema::Validator;
use serde_json::Value;

/// The keys that hold a robot state start with this.
pub const ROBOT_STATE_PREFIX: &str = "robot_state/";
/// The keys that hold a mission start with this.
pub const MISSION_PREFIX: &str = "mission/";
/// The keys that hold the reservation of a work zone start with this.
pub const ZONE_PREFIX: &str = "zone_reservation/";

/// The statuses of a mission (the lifecycle of the orchestrator).
pub const MISSION_STATUSES: [&str; 6] = [
    "queued",
    "assigned",
    "running",
    "completed",
    "failed",
    "cancelled",
];
const MAX_ID_BYTES: usize = 64;

const ROBOT_STATE_SCHEMA: &str = include_str!("../schemas/robot-state.schema.json");

/// How far from 1 the length of a unit quaternion may be (the same tolerance as the SDK clients).
const QUATERNION_TOLERANCE: f64 = 1e-6;

fn validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema: Value = serde_json::from_str(ROBOT_STATE_SCHEMA)
            .unwrap_or_else(|error| panic!("the vendored robot state schema is not JSON: {error}"));
        jsonschema::draft202012::options()
            .should_validate_formats(true)
            .build(&schema)
            .unwrap_or_else(|error| {
                panic!("the vendored robot state schema does not compile: {error}")
            })
    })
}

fn short_text<'a>(object: &'a Value, field: &str) -> Result<&'a str, String> {
    match object.get(field).and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() && text.len() <= MAX_ID_BYTES => Ok(text),
        _ => Err(format!(
            "{field} must be a non-empty text of at most {MAX_ID_BYTES} bytes"
        )),
    }
}

fn whole_number(object: &Value, field: &str) -> Result<u64, String> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{field} must be a whole number, zero or more"))
}

fn only_fields(object: &Value, allowed: &[&str]) -> Result<(), String> {
    let Some(map) = object.as_object() else {
        return Err("it must be a JSON object".to_string());
    };
    for name in map.keys() {
        if !allowed.contains(&name.as_str()) {
            return Err(format!("unknown field {name:?}"));
        }
    }
    Ok(())
}

/// The attempt of a mission or a zone reservation (its fencing token), checked against its key. `Ok(None)` for a key that is neither.
fn fenced_attempt(key: &str, value: &str) -> Result<Option<u64>, String> {
    let (id_field, id, kind) = if let Some(id) = key.strip_prefix(MISSION_PREFIX) {
        ("mission_id", id, "mission")
    } else if let Some(id) = key.strip_prefix(ZONE_PREFIX) {
        ("zone_id", id, "zone reservation")
    } else {
        return Ok(None);
    };
    let record: Value =
        serde_json::from_str(value).map_err(|error| format!("a {kind} must be JSON: {error}"))?;
    let check = || -> Result<u64, String> {
        if kind == "mission" {
            only_fields(&record, &["mission_id", "status", "owner", "attempt"])?;
            let status = short_text(&record, "status")?;
            if !MISSION_STATUSES.contains(&status) {
                return Err(format!(
                    "status {status:?} is not one of {MISSION_STATUSES:?}"
                ));
            }
            short_text(&record, "owner")?;
        } else {
            only_fields(&record, &["zone_id", "holder", "attempt", "expires_at_ms"])?;
            short_text(&record, "holder")?;
            whole_number(&record, "expires_at_ms")?;
        }
        if short_text(&record, id_field)? != id {
            return Err(format!("{id_field} is not {id:?}, the one of the key"));
        }
        whole_number(&record, "attempt")
    };
    check()
        .map(Some)
        .map_err(|reason| format!("not a valid {kind}: {reason}"))
}

/// The generation an entry of `key` must carry for its `value`: `attempt + 1` for a mission or a zone reservation, None for any other
/// key. A value that is not a valid record is an error.
pub fn required_generation(key: &str, value: &str) -> Result<Option<u64>, String> {
    Ok(fenced_attempt(key, value)?.map(|attempt| attempt.saturating_add(1)))
}

/// Refuses an entry of a mission or a zone reservation whose generation is not `attempt + 1` (see the module docs).
pub fn check_generation(key: &str, value: &str, generation: u64) -> Result<(), String> {
    match required_generation(key, value)? {
        Some(required) if required != generation => Err(format!(
            "the generation of a record is its attempt plus one ({required}), not {generation}"
        )),
        _ => Ok(()),
    }
}

/// Why `value` cannot be the value of `key`; `Ok` for a key that is not typed (the map is untyped there) or a valid one.
pub fn check_value(key: &str, value: &str) -> Result<(), String> {
    fenced_attempt(key, value)?;
    let Some(robot_id) = key.strip_prefix(ROBOT_STATE_PREFIX) else {
        return Ok(());
    };
    let state: Value = serde_json::from_str(value)
        .map_err(|error| format!("a robot state must be JSON: {error}"))?;
    let mut problems: Vec<String> = validator()
        .iter_errors(&state)
        .map(|error| {
            let at = error.instance_path().to_string();
            if at.is_empty() {
                error.to_string()
            } else {
                format!("{at}: {error}")
            }
        })
        .collect();
    problems.sort();
    problems.dedup();
    if problems.is_empty() {
        if state.get("robot_id").and_then(Value::as_str) != Some(robot_id) {
            problems.push(format!(
                "robot_id is not {robot_id:?}, the robot of the key"
            ));
        }
        let orientation = state.get("pose").and_then(|pose| pose.get("orientation"));
        if let Some(orientation) = orientation {
            let part = |name: &str| orientation.get(name).and_then(Value::as_f64);
            if let (Some(w), Some(x), Some(y), Some(z)) =
                (part("w"), part("x"), part("y"), part("z"))
            {
                let length = (w * w + x * x + y * y + z * z).sqrt();
                if !length.is_finite() || (length - 1.0).abs() > QUATERNION_TOLERANCE {
                    problems.push(format!(
                        "the quaternion of the pose has length {length}, not 1"
                    ));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("not a valid robot state: {}", problems.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{"schema_version": "1.0", "robot_id": "arm-1", "state": "busy", "updated_at_utc": "2024-01-01T00:00:00Z",
        "battery_percent": 80, "pose": {"schema_version": "1.0", "frame": "base", "position": {"x": 0, "y": 0, "z": 0.3}, "orientation": {"w": 1, "x": 0, "y": 0, "z": 0}}}"#;

    #[test]
    fn a_valid_robot_state_is_accepted() {
        assert_eq!(check_value("robot_state/arm-1", VALID), Ok(()));
    }

    #[test]
    fn other_keys_are_not_typed() {
        assert_eq!(check_value("robot-1", "estop-cleared"), Ok(()));
        assert_eq!(check_value("missions/m-1", "anything at all"), Ok(()));
        assert_eq!(check_value("robot_states/x", "not json"), Ok(()));
    }

    #[test]
    fn a_free_string_or_broken_json_under_the_prefix_is_refused() {
        assert!(check_value("robot_state/arm-1", "estop-cleared")
            .unwrap_err()
            .contains("must be JSON"));
        assert!(check_value("robot_state/arm-1", "").is_err());
        assert!(check_value("robot_state/arm-1", "[]").is_err());
    }

    #[test]
    fn a_state_that_breaks_the_contract_is_described() {
        let bad = VALID
            .replace("busy", "sleeping")
            .replace("2024-01-01T00:00:00Z", "later");
        let why = check_value("robot_state/arm-1", &bad).unwrap_err();
        assert!(why.contains("/state"), "{why}");
        assert!(why.contains("/updated_at_utc"), "{why}");
        let battery = VALID.replace("80", "180");
        assert!(check_value("robot_state/arm-1", &battery)
            .unwrap_err()
            .contains("battery_percent"));
    }

    #[test]
    fn the_robot_of_the_key_and_the_robot_of_the_value_must_match() {
        let why = check_value("robot_state/arm-2", VALID).unwrap_err();
        assert!(why.contains("the robot of the key"), "{why}");
    }

    #[test]
    fn the_pose_quaternion_must_have_unit_length() {
        let bad = VALID.replace(r#""w": 1, "x": 0"#, r#""w": 1, "x": 1"#);
        assert!(check_value("robot_state/arm-1", &bad)
            .unwrap_err()
            .contains("not 1"));
    }

    const MISSION: &str = r#"{"mission_id":"m-1","status":"running","owner":"orch-a","attempt":0}"#;
    const ZONE: &str =
        r#"{"zone_id":"cell-2","holder":"arm-1","attempt":3,"expires_at_ms":1700000005000}"#;

    #[test]
    fn a_mission_and_a_zone_reservation_are_typed() {
        assert_eq!(check_value("mission/m-1", MISSION), Ok(()));
        assert_eq!(check_value("zone_reservation/cell-2", ZONE), Ok(()));
        assert!(check_value("mission/m-1", "running")
            .unwrap_err()
            .contains("must be JSON"));
        // the id of the value is the one of the key
        assert!(check_value("mission/m-2", MISSION)
            .unwrap_err()
            .contains("the one of the key"));
        assert!(check_value("zone_reservation/cell-9", ZONE)
            .unwrap_err()
            .contains("the one of the key"));
        // a status outside the lifecycle, an unknown field, a missing or negative attempt, an empty owner
        for bad in [
            MISSION.replace("running", "sleeping"),
            MISSION.replace("\"attempt\":0", "\"attempt\":0,\"extra\":1"),
            MISSION.replace("\"attempt\":0", "\"attempt\":-1"),
            MISSION.replace(",\"attempt\":0", ""),
            MISSION.replace("orch-a", " "),
            MISSION.replace("\"attempt\":0", "\"attempt\":1.5"),
        ] {
            assert!(check_value("mission/m-1", &bad).is_err(), "{bad}");
        }
        assert!(check_value(
            "zone_reservation/cell-2",
            &ZONE.replace(",\"expires_at_ms\":1700000005000", "")
        )
        .is_err());
        assert!(check_value(
            "zone_reservation/cell-2",
            &ZONE.replace("arm-1", &"h".repeat(65))
        )
        .is_err());
    }

    #[test]
    fn the_generation_of_a_record_is_its_attempt_plus_one() {
        assert_eq!(required_generation("mission/m-1", MISSION), Ok(Some(1)));
        assert_eq!(
            required_generation("zone_reservation/cell-2", ZONE),
            Ok(Some(4))
        );
        assert_eq!(required_generation("robot-1", "free text"), Ok(None));
        assert_eq!(check_generation("mission/m-1", MISSION, 1), Ok(()));
        assert!(check_generation("mission/m-1", MISSION, 2)
            .unwrap_err()
            .contains("attempt plus one (1)"));
        assert_eq!(check_generation("other", "x", 7), Ok(()));
    }

    #[test]
    fn the_record_the_orchestrator_exports_is_valid_and_its_generation_is_the_one_it_states() {
        // The same text is produced by HYDRA-UMC-ORCHESTRATOR (src/replica.rs): the two repositories agree on it.
        let from_orchestrator =
            r#"{"mission_id":"m-1","status":"running","owner":"node-a","attempt":1}"#;
        assert_eq!(check_value("mission/m-1", from_orchestrator), Ok(()));
        assert_eq!(
            check_generation("mission/m-1", from_orchestrator, 2),
            Ok(())
        );
        let queued = r#"{"mission_id":"m-2","status":"queued","owner":"orch-a","attempt":0}"#;
        assert_eq!(check_generation("mission/m-2", queued, 1), Ok(()));
    }
}
