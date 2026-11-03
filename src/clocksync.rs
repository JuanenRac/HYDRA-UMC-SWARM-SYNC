// =============================================================================
// HYDRA-UMC-SWARM-SYNC - src/clocksync.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! Clock synchronisation by software: the offset and the round trip between this node and each
//! of its peers, measured the way NTP does it, with the jitter of the measurements.
//!
//! One probe takes four instants: `t0` this node sends, `t1` the peer receives, `t2` the peer
//! answers, `t3` this node receives (all in microseconds since the Unix epoch, each taken from
//! the clock of the machine that names it). Then
//!
//! * `offset = ((t1 - t0) + (t2 - t3)) / 2`: how far the peer's clock is ahead of ours;
//! * `rtt = (t3 - t0) - (t2 - t1)`: the time the packets spent on the network.
//!
//! A probe whose instants cannot be right (time going backwards, a round trip above the limit)
//! is counted as rejected and never enters the window. The estimate is the offset of the probe
//! with the smallest round trip in the window (the one least disturbed by the network); the
//! jitter is the mean absolute difference between consecutive offsets.
//!
//! This is NOT PTP: no hardware timestamps, so the precision is that of the network and of the
//! scheduler of the operating system (milliseconds on a LAN). It is the step before PTP and says
//! so in the numbers it publishes. Nothing here changes the clock of the machine or the Lamport
//! order of the map: it only measures and reports.

use std::collections::VecDeque;

/// The configuration of the measurements (section `clock_sync` of the file given with `--clock-sync-config`).
#[derive(Debug, Clone, PartialEq)]
pub struct ClockConfig {
    pub interval_ms: u64,
    /// how many recent probes per peer the estimate looks at
    pub window: usize,
    /// a probe with a round trip above this is rejected
    pub max_rtt_us: u64,
}

pub const DEFAULT_INTERVAL_MS: u64 = 10_000;
pub const DEFAULT_WINDOW: usize = 8;
pub const DEFAULT_MAX_RTT_US: u64 = 2_000_000;

impl Default for ClockConfig {
    fn default() -> Self {
        ClockConfig {
            interval_ms: DEFAULT_INTERVAL_MS,
            window: DEFAULT_WINDOW,
            max_rtt_us: DEFAULT_MAX_RTT_US,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ClockSection {
    #[serde(default)]
    enabled: bool,
    interval_ms: Option<u64>,
    window: Option<usize>,
    max_rtt_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    clock_sync: Option<ClockSection>,
    metrics: Option<MetricsSection>,
}

/// The `metrics` section: `GET /metrics` in the text format of Prometheus. Off unless `enabled`; closed, the route answers `404` like an unknown path.
/// Open, it answers a caller on this machine, or any caller that presents the shared secret when the node has one.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MetricsSection {
    #[serde(default)]
    enabled: bool,
}

/// Whether the configuration file opens `GET /metrics` (off when there is no file, no section or `enabled` is false).
pub fn parse_metrics(text: &str) -> Result<bool, String> {
    if text.trim().is_empty() {
        return Ok(false);
    }
    let file: FileConfig = serde_json::from_str(text).map_err(|e| format!("configuration: {e}"))?;
    Ok(file.metrics.is_some_and(|s| s.enabled))
}

/// Reads the configuration file. `Ok(None)` means the measurements are off (no section or `enabled` false).
pub fn parse_config(text: &str) -> Result<Option<ClockConfig>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let file: FileConfig =
        serde_json::from_str(text).map_err(|e| format!("clock sync configuration: {e}"))?;
    let Some(section) = file.clock_sync else {
        return Ok(None);
    };
    if !section.enabled {
        return Ok(None);
    }
    let mut config = ClockConfig::default();
    if let Some(v) = section.interval_ms {
        if !(1_000..=3_600_000).contains(&v) {
            return Err("clock_sync.interval_ms must be between 1000 and 3600000".into());
        }
        config.interval_ms = v;
    }
    if let Some(v) = section.window {
        if !(1..=1_000).contains(&v) {
            return Err("clock_sync.window must be between 1 and 1000".into());
        }
        config.window = v;
    }
    if let Some(v) = section.max_rtt_ms {
        if !(1..=60_000).contains(&v) {
            return Err("clock_sync.max_rtt_ms must be between 1 and 60000".into());
        }
        config.max_rtt_us = v * 1_000;
    }
    Ok(Some(config))
}

/// The offset and the round trip of one probe, or `None` when its four instants cannot be right.
pub fn compute(t0: u64, t1: u64, t2: u64, t3: u64) -> Option<(i64, u64)> {
    if t3 < t0 || t2 < t1 {
        return None;
    }
    let total = t3 - t0;
    let peer = t2 - t1;
    if peer > total {
        return None;
    }
    let offset = ((t1 as i128 - t0 as i128) + (t2 as i128 - t3 as i128)) / 2;
    Some((offset as i64, total - peer))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub offset_us: i64,
    pub rtt_us: u64,
}

#[derive(Debug, Clone, Default)]
pub struct PeerClock {
    window: VecDeque<Sample>,
    pub accepted: u64,
    pub rejected: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClockStats {
    pub samples: usize,
    pub offset_us: i64,
    pub rtt_min_us: u64,
    pub rtt_avg_us: u64,
    pub rtt_max_us: u64,
    pub jitter_us: u64,
}

impl PeerClock {
    pub fn record(&mut self, probe: Option<(i64, u64)>, config: &ClockConfig) {
        match probe {
            Some((offset_us, rtt_us)) if rtt_us <= config.max_rtt_us => {
                self.accepted += 1;
                self.window.push_back(Sample { offset_us, rtt_us });
                while self.window.len() > config.window {
                    self.window.pop_front();
                }
            }
            _ => self.rejected += 1,
        }
    }

    pub fn stats(&self) -> Option<ClockStats> {
        let best = self.window.iter().min_by_key(|s| s.rtt_us)?;
        let n = self.window.len() as u64;
        let sum: u64 = self.window.iter().map(|s| s.rtt_us).sum();
        let jumps: Vec<u64> = self
            .window
            .iter()
            .zip(self.window.iter().skip(1))
            .map(|(a, b)| a.offset_us.abs_diff(b.offset_us))
            .collect();
        let jitter_us = if jumps.is_empty() {
            0
        } else {
            jumps.iter().sum::<u64>() / jumps.len() as u64
        };
        Some(ClockStats {
            samples: self.window.len(),
            offset_us: best.offset_us,
            rtt_min_us: best.rtt_us,
            rtt_avg_us: sum / n,
            rtt_max_us: self.window.iter().map(|s| s.rtt_us).max().unwrap_or(0),
            jitter_us,
        })
    }
}

/// Microseconds since the Unix epoch on this machine.
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_ahead_by_a_known_amount_and_a_symmetric_network_is_measured_exactly() {
        // peer clock = ours + 5000; 1000 each way; the peer takes 200 to answer
        let t0 = 1_000_000;
        let t1 = t0 + 1_000 + 5_000;
        let t2 = t1 + 200;
        let t3 = t0 + 1_000 + 200 + 1_000;
        assert_eq!(compute(t0, t1, t2, t3), Some((5_000, 2_000)));
    }

    #[test]
    fn a_peer_behind_gives_a_negative_offset() {
        let (offset, rtt) = compute(
            10_000_000,
            9_999_000 + 500,
            9_999_000 + 700,
            10_000_000 + 1_200,
        )
        .unwrap();
        assert!(offset < -900 && offset > -1_100, "{offset}");
        assert_eq!(rtt, 1_000);
    }

    #[test]
    fn instants_that_cannot_be_right_are_not_a_probe() {
        assert_eq!(compute(10, 5, 6, 9), None, "we received before we sent");
        assert_eq!(
            compute(10, 20, 15, 30),
            None,
            "the peer answered before it received"
        );
        assert_eq!(
            compute(10, 20, 100, 30),
            None,
            "the peer took longer than the whole exchange"
        );
    }

    #[test]
    fn the_estimate_is_the_offset_of_the_probe_with_the_smallest_round_trip() {
        let config = ClockConfig::default();
        let mut peer = PeerClock::default();
        peer.record(Some((900, 5_000)), &config);
        peer.record(Some((1_000, 400)), &config);
        peer.record(Some((1_300, 9_000)), &config);
        let stats = peer.stats().unwrap();
        assert_eq!(
            (stats.offset_us, stats.rtt_min_us, stats.rtt_max_us),
            (1_000, 400, 9_000)
        );
        assert_eq!(stats.rtt_avg_us, (5_000 + 400 + 9_000) / 3);
        assert_eq!(stats.samples, 3);
    }

    #[test]
    fn jitter_is_the_mean_jump_between_consecutive_offsets() {
        let config = ClockConfig::default();
        let mut peer = PeerClock::default();
        for offset in [100, 160, 100, 140] {
            peer.record(Some((offset, 1_000)), &config);
        }
        assert_eq!(peer.stats().unwrap().jitter_us, (60 + 60 + 40) / 3);
        let mut single = PeerClock::default();
        single.record(Some((5, 10)), &config);
        assert_eq!(single.stats().unwrap().jitter_us, 0);
    }

    #[test]
    fn the_window_is_bounded_and_slow_or_impossible_probes_are_rejected() {
        let config = ClockConfig {
            window: 3,
            max_rtt_us: 1_000,
            ..ClockConfig::default()
        };
        let mut peer = PeerClock::default();
        for i in 0..10 {
            peer.record(Some((i, 500)), &config);
        }
        peer.record(Some((7, 1_001)), &config);
        peer.record(None, &config);
        assert_eq!(peer.stats().unwrap().samples, 3);
        assert_eq!((peer.accepted, peer.rejected), (10, 2));
    }

    #[test]
    fn no_probe_no_stats() {
        assert!(PeerClock::default().stats().is_none());
    }

    #[test]
    fn metrics_are_off_unless_the_section_enables_them() {
        assert!(!parse_metrics("").unwrap());
        assert!(!parse_metrics("{}").unwrap());
        assert!(!parse_metrics(r#"{"metrics":{"enabled":false}}"#).unwrap());
        assert!(parse_metrics(r#"{"metrics":{"enabled":true}}"#).unwrap());
        assert!(parse_metrics(r#"{"metrics":{"enabled":true,"bogus":1}}"#).is_err());
        assert_eq!(
            parse_config(r#"{"metrics":{"enabled":true}}"#).unwrap(),
            None
        );
    }

    #[test]
    fn the_configuration_is_off_unless_enabled_and_checks_its_values() {
        assert_eq!(parse_config("").unwrap(), None);
        assert_eq!(parse_config("{}").unwrap(), None);
        assert_eq!(
            parse_config(r#"{"clock_sync":{"enabled":false}}"#).unwrap(),
            None
        );
        assert_eq!(
            parse_config(r#"{"clock_sync":{"enabled":true}}"#).unwrap(),
            Some(ClockConfig::default())
        );
        let c = parse_config(
            r#"{"clock_sync":{"enabled":true,"interval_ms":2000,"window":4,"max_rtt_ms":50}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!((c.interval_ms, c.window, c.max_rtt_us), (2_000, 4, 50_000));
        for bad in [
            r#"{"clock_sync":{"enabled":true,"interval_ms":10}}"#,
            r#"{"clock_sync":{"enabled":true,"window":0}}"#,
            r#"{"clock_sync":{"enabled":true,"max_rtt_ms":0}}"#,
            r#"{"clock_sync":{"enabled":true,"intervall_ms":5000}}"#,
            "not json",
        ] {
            assert!(parse_config(bad).is_err(), "{bad}");
        }
    }
}
