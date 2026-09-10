// HYDRA-UMC-SWARM-SYNC - lamport.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
//
// A Lamport logical clock - NOT the real PTP (IEEE 1588) hardware
// timestamping the README describes, and not pretending to be. PTP
// needs real hardware timers/NICs to mean anything (sub-100ns jitter is
// not a software concept), so it stays deferred until there's real
// hardware to validate it against. What a logical clock CAN do without any hardware is give
// the CRDT merge in src/crdt.rs a real, testable, causally-consistent
// ordering: "did update A happen-before update B, or are they
// concurrent" - which is exactly what a state-based CRDT's merge needs
// to resolve conflicts deterministically, and it is provably correct on
// its own, in software, today.

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct LamportTime(pub u64);

/// SWARM-01 (P2 - "the max counter neither panics nor resets causal
/// order"): plain `+= 1` on this counter panics in a debug build and
/// silently WRAPS TO ZERO in a release build once it reaches `u64::MAX` -
/// a release-build wrap is the real danger: this clock would suddenly
/// look older than every event it has ever observed, letting already-
/// superseded writes win again. `tick()`/`observe()` below use
/// `checked_add` and return this explicit error instead, so the failure
/// is visible (and causal order is simply frozen, never reset) rather
/// than silently corrupted. In practice this needs on the order of 2^64
/// real local events/observations on a single node - astronomically far
/// from anything this system will see - but a clock this module's own
/// header comment calls "provably correct" shouldn't have a silent
/// failure mode at the edge of its own domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockOverflowError;

impl std::fmt::Display for ClockOverflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Lamport clock reached u64::MAX and cannot advance further without wrapping - refusing rather than resetting causal order"
        )
    }
}

impl std::error::Error for ClockOverflowError {}

#[derive(Debug, Clone, Default)]
pub struct LamportClock {
    time: u64,
}

impl LamportClock {
    pub fn new() -> Self {
        LamportClock { time: 0 }
    }

    /// A purely local event (e.g. this node updating its own status):
    /// advance the clock by one and return the new time.
    pub fn tick(&mut self) -> Result<LamportTime, ClockOverflowError> {
        self.time = self.time.checked_add(1).ok_or(ClockOverflowError)?;
        Ok(LamportTime(self.time))
    }

    /// Receiving a remote timestamp: the standard Lamport rule - jump to
    /// one past whichever is later (ours or theirs), so every event this
    /// node produces after this point is provably ordered after the
    /// remote one it just learned about.
    pub fn observe(&mut self, remote: LamportTime) -> Result<LamportTime, ClockOverflowError> {
        let base = self.time.max(remote.0);
        self.time = base.checked_add(1).ok_or(ClockOverflowError)?;
        Ok(LamportTime(self.time))
    }

    // Not called by main.rs today (tick()'s own return value is enough
    // for the CLI demo) - kept as real, obvious API for a live daemon
    // that needs to read the clock without advancing it (e.g. attaching
    // a timestamp to an outgoing heartbeat between ticks). Only exercised
    // by this module's own #[cfg(test)] overflow tests above (to confirm
    // a refused tick()/observe() never mutated the counter), which a
    // plain `cargo build` doesn't compile - hence #[allow(dead_code)]
    // still applies to that build, not just the pre-existing gap.
    #[allow(dead_code)]
    pub fn current(&self) -> LamportTime {
        LamportTime(self.time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_strictly_increases() {
        let mut clock = LamportClock::new();
        let a = clock.tick().unwrap();
        let b = clock.tick().unwrap();
        assert!(b > a);
    }

    #[test]
    fn observe_jumps_past_a_later_remote_time() {
        let mut clock = LamportClock::new();
        clock.tick().unwrap(); // local time = 1
        let result = clock.observe(LamportTime(10)).unwrap();
        assert_eq!(result, LamportTime(11));
    }

    #[test]
    fn observe_still_advances_when_remote_is_earlier() {
        let mut clock = LamportClock::new();
        for _ in 0..5 {
            clock.tick().unwrap(); // local time = 5
        }
        let result = clock.observe(LamportTime(2)).unwrap();
        // Even though the remote time is behind, Lamport's rule still
        // strictly advances - an observed event is still a new event.
        assert_eq!(result, LamportTime(6));
    }

    #[test]
    fn tick_refuses_to_wrap_at_u64_max() {
        let mut clock = LamportClock { time: u64::MAX };
        assert_eq!(clock.tick(), Err(ClockOverflowError));
        // Refusing must not have mutated the counter into a smaller,
        // wrapped value - it must still read as u64::MAX, never reset.
        assert_eq!(clock.current(), LamportTime(u64::MAX));
    }

    #[test]
    fn observe_refuses_to_wrap_at_u64_max() {
        let mut clock = LamportClock { time: 3 };
        assert_eq!(
            clock.observe(LamportTime(u64::MAX)),
            Err(ClockOverflowError)
        );
        assert_eq!(clock.current(), LamportTime(3));
    }
}
