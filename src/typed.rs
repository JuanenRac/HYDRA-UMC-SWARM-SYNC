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
//! The missions are not typed here: they stay a pending item of the swarm contract.

use std::sync::OnceLock;

use jsonschema::Validator;
use serde_json::Value;

/// The keys that hold a robot state start with this.
pub const ROBOT_STATE_PREFIX: &str = "robot_state/";

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

/// Why `value` cannot be the value of `key`; `Ok` for a key that is not a robot state (the map is untyped there) or a valid one.
pub fn check_value(key: &str, value: &str) -> Result<(), String> {
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
        assert_eq!(check_value("mission/m-1", "anything at all"), Ok(()));
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
}
