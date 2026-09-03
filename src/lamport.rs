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
    pub fn tick(&mut self) -> LamportTime {
        self.time += 1;
        LamportTime(self.time)
    }

    /// Receiving a remote timestamp: the standard Lamport rule - jump to
    /// one past whichever is later (ours or theirs), so every event this
    /// node produces after this point is provably ordered after the
    /// remote one it just learned about.
    pub fn observe(&mut self, remote: LamportTime) -> LamportTime {
        self.time = self.time.max(remote.0) + 1;
        LamportTime(self.time)
    }

    // Not called by main.rs today (tick()'s own return value is enough
    // for the CLI demo) - kept as real, obvious API for a live daemon
    // that needs to read the clock without advancing it (e.g. attaching
    // a timestamp to an outgoing heartbeat between ticks).
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
        let a = clock.tick();
        let b = clock.tick();
        assert!(b > a);
    }

    #[test]
    fn observe_jumps_past_a_later_remote_time() {
        let mut clock = LamportClock::new();
        clock.tick(); // local time = 1
        let result = clock.observe(LamportTime(10));
        assert_eq!(result, LamportTime(11));
    }

    #[test]
    fn observe_still_advances_when_remote_is_earlier() {
        let mut clock = LamportClock::new();
        for _ in 0..5 {
            clock.tick(); // local time = 5
        }
        let result = clock.observe(LamportTime(2));
        // Even though the remote time is behind, Lamport's rule still
        // strictly advances - an observed event is still a new event.
        assert_eq!(result, LamportTime(6));
    }
}
