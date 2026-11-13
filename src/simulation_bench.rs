// =============================================================================
// HYDRA-UMC-SWARM-SYNC - src/simulation_bench.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The simulation bench of the swarm state: simulated cells, each with its own logical clock and its own map, write and delete at random while a
//! simulated network drops, duplicates, delays and reorders the states they send, splits the swarm into groups that cannot hear each other, and
//! cells lose their clock or their whole state. Every run comes from a fixed seed. Once the network heals and the cells exchange states, all of them
//! must hold exactly the same entries, and that state must be the join of what the cells held, whatever the order of the join. Nothing here
//! touches a robot or a network: a pass means "passed in simulation".

use crate::crdt::LwwMap;
use crate::lamport::{LamportClock, LamportTime};

type State = Vec<(String, Option<String>, LamportTime, u64, u64)>;

const CELLS: usize = 5;
const KEYS: usize = 8;
const STEPS: usize = 600;

/// A small generator with a fixed seed.
struct Lcg(u64);

impl Lcg {
    fn unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
    fn below(&mut self, n: usize) -> usize {
        ((self.unit() * n as f64) as usize).min(n - 1)
    }
    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
}

#[derive(Clone)]
struct Cell {
    id: u64,
    clock: LamportClock,
    map: LwwMap<String, String>,
}

impl Cell {
    fn new(id: u64) -> Cell {
        Cell {
            id,
            clock: LamportClock::new(),
            map: LwwMap::new(),
        }
    }

    fn write(&mut self, key: &str, value: &str) {
        let time = self.clock.tick().unwrap();
        self.map
            .set(key.to_string(), value.to_string(), time, self.id);
    }

    fn delete(&mut self, key: &str) {
        let time = self.clock.tick().unwrap();
        self.map.remove(key.to_string(), time, self.id);
    }

    fn receive(&mut self, state: &LwwMap<String, String>) {
        if let Some(remote) = state.max_time() {
            self.clock.observe(remote).unwrap();
        }
        self.map = self.map.merge(state);
    }
}

struct Message {
    deliver_at: usize,
    to: usize,
    state: LwwMap<String, String>,
}

/// Who can hear whom at a step: two windows in which the swarm is cut into groups.
fn same_side(step: usize, a: usize, b: usize) -> bool {
    let group = |cell: usize| -> usize {
        if (100..300).contains(&step) {
            usize::from(cell >= 2)
        } else if (400..500).contains(&step) {
            usize::from(cell >= 1)
        } else {
            0
        }
    };
    group(a) == group(b)
}

fn state_of(map: &LwwMap<String, String>) -> State {
    map.entries_with_stamps()
}

/// Runs the swarm through `STEPS` steps of writes, deletes, lossy sends and faults, and returns the cells (with the network still unhealed).
fn run(seed: u64, faults: bool) -> Vec<Cell> {
    let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut cells: Vec<Cell> = (0..CELLS).map(|i| Cell::new(i as u64 + 1)).collect();
    let mut queue: Vec<Message> = Vec::new();
    for step in 0..STEPS {
        // the messages that are due arrive, in no particular order
        let mut due = Vec::new();
        let mut index = 0;
        while index < queue.len() {
            if queue[index].deliver_at <= step {
                due.push(queue.swap_remove(index));
            } else {
                index += 1;
            }
        }
        for message in due {
            cells[message.to].receive(&message.state);
        }
        let who = rng.below(CELLS);
        let key = format!("key-{}", rng.below(KEYS));
        let roll = rng.unit();
        if roll < 0.25 {
            let value = format!("v{step}-{who}");
            cells[who].write(&key, &value);
        } else if roll < 0.35 {
            cells[who].delete(&key);
        } else if roll < 0.95 {
            let to = rng.below(CELLS);
            if to != who && same_side(step, who, to) && !rng.chance(0.1) {
                let copies = if rng.chance(0.1) { 2 } else { 1 };
                for _ in 0..copies {
                    queue.push(Message {
                        deliver_at: step + 1 + rng.below(40),
                        to,
                        state: cells[who].map.clone(),
                    });
                }
            }
        } else if faults {
            // a cell crashes and comes back empty (its clock is kept, as a restart that reloads the clock does)
            cells[who].map = LwwMap::new();
        }
    }
    cells
}

/// The network heals: every cell hears every other until nothing changes.
fn heal(cells: &mut [Cell]) {
    for _ in 0..CELLS {
        for from in 0..cells.len() {
            let state = cells[from].map.clone();
            for to in 0..cells.len() {
                if to != from {
                    cells[to].receive(&state);
                }
            }
        }
    }
}

fn join(order: &[usize], cells: &[Cell]) -> LwwMap<String, String> {
    let mut joined: LwwMap<String, String> = LwwMap::new();
    for &i in order {
        joined = joined.merge(&cells[i].map);
    }
    joined
}

#[test]
fn after_a_lossy_partitioned_network_heals_every_cell_holds_the_same_entries() {
    for seed in 1..=20 {
        let mut cells = run(seed, false);
        heal(&mut cells);
        let first = state_of(&cells[0].map);
        for (i, cell) in cells.iter().enumerate() {
            assert_eq!(state_of(&cell.map), first, "seed {seed}: cell {i}");
        }
        assert!(!first.is_empty(), "seed {seed}: the run wrote something");
    }
}

#[test]
fn cells_that_crash_and_come_back_empty_still_converge() {
    for seed in 21..=40 {
        let mut cells = run(seed, true);
        heal(&mut cells);
        let first = state_of(&cells[0].map);
        for (i, cell) in cells.iter().enumerate() {
            assert_eq!(state_of(&cell.map), first, "seed {seed}: cell {i}");
        }
    }
}

#[test]
fn the_converged_state_is_the_join_of_the_cells_whatever_the_order_of_the_join() {
    for seed in 41..=50 {
        let mut cells = run(seed, true);
        let forward: Vec<usize> = (0..CELLS).collect();
        let backward: Vec<usize> = (0..CELLS).rev().collect();
        let mut shuffled: Vec<usize> = (0..CELLS).collect();
        let mut rng = Lcg(seed);
        for i in (1..CELLS).rev() {
            shuffled.swap(i, rng.below(i + 1));
        }
        let expected = state_of(&join(&forward, &cells));
        assert_eq!(state_of(&join(&backward, &cells)), expected, "seed {seed}");
        assert_eq!(state_of(&join(&shuffled, &cells)), expected, "seed {seed}");
        heal(&mut cells);
        assert_eq!(state_of(&cells[0].map), expected, "seed {seed}");
    }
}

#[test]
fn hearing_the_same_state_twice_changes_nothing() {
    for seed in 51..=55 {
        let cells = run(seed, false);
        for cell in &cells {
            let once = cell.map.merge(&cells[0].map);
            let twice = once.merge(&cells[0].map);
            assert_eq!(state_of(&once), state_of(&twice), "seed {seed}");
        }
    }
}

#[test]
fn the_same_seed_gives_the_same_swarm() {
    let first: Vec<State> = run(9, true).iter().map(|c| state_of(&c.map)).collect();
    let second: Vec<State> = run(9, true).iter().map(|c| state_of(&c.map)).collect();
    assert_eq!(first, second);
}

#[test]
fn a_cell_that_lost_its_clock_and_reuses_a_stamp_for_another_value_is_refused_not_merged_silently()
{
    let mut before = Cell::new(1);
    before.write("door", "open");
    // the same writer after a restart with no clock: the same stamp, another value
    let mut after = Cell::new(1);
    after.write("door", "closed");
    assert!(before.map.merge_report(&after.map).is_err());
    assert!(after.map.merge_report(&before.map).is_err());
    // an identical duplicate is not a collision
    let copy = before.map.clone();
    assert!(before.map.merge_report(&copy).is_ok());
}

#[test]
fn a_delete_heard_by_a_cell_beats_the_stale_copy_of_a_cell_that_was_cut_off() {
    let mut a = Cell::new(1);
    let mut b = Cell::new(2);
    a.write("door", "open");
    b.receive(&a.map);
    // b deletes while a is cut off; a writes nothing new, it still holds the stale copy
    b.delete("door");
    let stale = a.map.clone();
    b.receive(&stale);
    a.receive(&b.map);
    assert_eq!(a.map.get(&"door".to_string()), None);
    assert_eq!(b.map.get(&"door".to_string()), None);
    assert_eq!(state_of(&a.map), state_of(&b.map));
}

#[test]
fn a_write_after_a_cell_heard_everything_beats_what_it_heard() {
    let mut a = Cell::new(1);
    let mut b = Cell::new(2);
    a.write("mode", "manual");
    b.write("mode", "auto");
    a.receive(&b.map);
    b.receive(&a.map);
    // a has heard the state of b: its next write is later than both
    a.write("mode", "stop");
    b.receive(&a.map);
    assert_eq!(
        a.map.get(&"mode".to_string()).map(String::as_str),
        Some("stop")
    );
    assert_eq!(
        b.map.get(&"mode".to_string()).map(String::as_str),
        Some("stop")
    );
}
