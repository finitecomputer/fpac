//! Which cached extents leave first when the cache is past its bound
//! (docs/design.md, The cache): S3-FIFO (Yang et al., SOSP 2023), chosen
//! because a one-pass scan cannot flush out what is used again, and it is
//! three queues and a counter per extent, nothing more.
//!
//! - An extent the cache takes for the first time goes into the **small**
//!   queue (a tenth of the bound). One that is used again while there
//!   (its count above zero) moves to the **main** queue when it reaches the
//!   small queue's end; one that is not leaves, and its number is kept in
//!   the **ghost** queue (numbers only).
//! - An extent taken again while its number is a ghost goes straight to
//!   main: it was used twice within about a cache's worth of traffic.
//! - Main is a clock: an extent at its end with a count above zero goes
//!   back to its start with the count lowered; one at zero leaves.
//!
//! So a build's one-pass read of its outputs passes through small and
//! out, and the agent's hot files, in main, stay. An extent read ahead
//! (`Taken::Ahead`) counts its first read as its taking, not as a use, so
//! read-ahead does not promote a scan either.
//!
//! The policy only chooses. The caller drops: it never drops an extent the
//! store cannot (dirty, shipping) or one a write is reading; such an
//! extent goes back to main's start and is asked again later.

use std::collections::{HashMap, HashSet, VecDeque};

/// The most a count goes to (two bits, as the paper's).
const FREQ_MAX: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// Fetched for a read, or written: the taking is its first use.
    Used,
    /// Fetched ahead of a read that has not come yet.
    Ahead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Queue {
    Small,
    Main,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    queue: Queue,
    freq: u8,
    /// Read ahead and not read yet: its first read is its taking.
    ahead: bool,
}

#[derive(Debug)]
pub struct Evictor {
    small_max: usize,
    ghost_max: usize,
    small: VecDeque<u64>,
    main: VecDeque<u64>,
    ghost: VecDeque<u64>,
    ghosts: HashSet<u64>,
    entries: HashMap<u64, Entry>,
}

impl Evictor {
    /// A policy for a cache of `bound` extents.
    pub fn new(bound: usize) -> Evictor {
        assert!(bound >= 10, "a cache of at least ten extents");
        Evictor { small_max: bound / 10, ghost_max: bound, small: VecDeque::new(), main: VecDeque::new(), ghost: VecDeque::new(), ghosts: HashSet::new(), entries: HashMap::new() }
    }

    /// Extents the policy knows to be in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The cache took `extent` (a fetch, a write, a read ahead); if it
    /// holds it already, this is a use.
    pub fn taken(&mut self, extent: u64, how: Taken) {
        if self.entries.contains_key(&extent) {
            if how == Taken::Used {
                self.used(extent);
            }
            return;
        }
        let queue = if self.ghosts.remove(&extent) {
            Queue::Main
        } else {
            Queue::Small
        };
        match queue {
            Queue::Small => self.small.push_back(extent),
            Queue::Main => self.main.push_back(extent),
        }
        self.entries.insert(extent, Entry { queue, freq: 0, ahead: how == Taken::Ahead });
    }

    /// `extent` was read or written while cached.
    pub fn used(&mut self, extent: u64) {
        if let Some(e) = self.entries.get_mut(&extent) {
            if e.ahead {
                e.ahead = false;
            } else {
                e.freq = (e.freq + 1).min(FREQ_MAX);
            }
        }
    }

    /// `extent` left the cache another way (a punch): forgotten.
    pub fn gone(&mut self, extent: u64) {
        // its queue slot is dropped lazily, when it comes up
        self.entries.remove(&extent);
    }

    /// The next extent to drop, asking `can_drop` of each candidate; at
    /// most `tries` candidates (each queue's length bounds a pass). None
    /// when nothing it was asked about may go.
    pub fn victim(&mut self, mut can_drop: impl FnMut(u64) -> bool) -> Option<u64> {
        // Bounded: each pass looks at an extent at most once per queue
        // turn; a kept one is put back, so twice the length ends it.
        let mut tries = 2 * (self.small.len() + self.main.len()) + 2;
        // main's extents kept in a row: once all of them were, small's turn
        let mut kept = 0;
        while tries > 0 {
            tries -= 1;
            let from_small = !self.small.is_empty() && (self.small.len() > self.small_max || self.main.is_empty() || kept >= self.main.len());
            let next = if from_small { self.small.pop_front() } else { self.main.pop_front() };
            let x = next?;
            let Some(mut e) = self.entries.get(&x).copied() else { continue };
            if e.queue != if from_small { Queue::Small } else { Queue::Main } {
                continue;
            }
            if from_small {
                if e.freq > 0 {
                    e.queue = Queue::Main;
                    e.freq = 0;
                    self.entries.insert(x, e);
                    self.main.push_back(x);
                    continue;
                }
            } else if e.freq > 0 {
                e.freq -= 1;
                self.entries.insert(x, e);
                self.main.push_back(x);
                continue;
            }
            if !can_drop(x) {
                // dirty, shipping or being read: kept, asked again later
                if !from_small {
                    kept += 1;
                }
                e.queue = Queue::Main;
                self.entries.insert(x, e);
                self.main.push_back(x);
                continue;
            }
            self.entries.remove(&x);
            if from_small {
                self.ghosts.insert(x);
                self.ghost.push_back(x);
                // Bounded: one out for each one in.
                while self.ghost.len() > self.ghost_max {
                    if let Some(g) = self.ghost.pop_front() {
                        self.ghosts.remove(&g);
                    }
                }
            }
            return Some(x);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache of `bound` extents driven by `reads`, dropping through the
    /// policy: answers the hits.
    fn run(ev: &mut Evictor, cached: &mut HashSet<u64>, bound: usize, reads: impl Iterator<Item = u64>) -> usize {
        let mut hits = 0;
        for x in reads {
            if cached.contains(&x) {
                hits += 1;
                ev.used(x);
                continue;
            }
            cached.insert(x);
            ev.taken(x, Taken::Used);
            while cached.len() > bound {
                let v = ev.victim(|_| true).expect("something to drop");
                assert!(cached.remove(&v));
            }
        }
        hits
    }

    // Goal: a one-pass scan larger than the cache does not flush out a hot
    // set that is used again; under FIFO it would. Method: a cache of 1,000
    // extents, a hot set of 300 read three times, a scan of 10,000 others,
    // then the hot set again.
    #[test]
    fn a_scan_does_not_flush_out_what_is_used_again() {
        let (bound, mut cached) = (1000, HashSet::new());
        let mut ev = Evictor::new(bound);
        for _ in 0..3 {
            run(&mut ev, &mut cached, bound, 0..300);
        }
        run(&mut ev, &mut cached, bound, 100_000..110_000);
        let hits = run(&mut ev, &mut cached, bound, 0..300);
        assert_eq!(hits, 300, "the hot set survived the scan");
        assert!(cached.len() <= bound);
    }

    // Goal: what is read ahead and then read once is a scan, not a hot
    // set: it leaves before the hot set. Method: a hot set used twice;
    // 5,000 extents read ahead, each then read once; the hot set again.
    #[test]
    fn read_ahead_does_not_promote_a_scan() {
        let (bound, mut cached) = (1000, HashSet::new());
        let mut ev = Evictor::new(bound);
        for _ in 0..2 {
            run(&mut ev, &mut cached, bound, 0..300);
        }
        for x in 50_000..55_000u64 {
            cached.insert(x);
            ev.taken(x, Taken::Ahead);
            ev.used(x);
            while cached.len() > bound {
                let v = ev.victim(|_| true).unwrap();
                cached.remove(&v);
            }
        }
        assert_eq!(run(&mut ev, &mut cached, bound, 0..300), 300);
    }

    // Goal: an extent the caller may not drop stays, and the next one that
    // may goes instead; nothing droppable left answers None. Method: three
    // extents, two kept.
    #[test]
    fn kept_extents_are_skipped() {
        let mut ev = Evictor::new(10);
        for x in 1..=3 {
            ev.taken(x, Taken::Used);
        }
        assert_eq!(ev.victim(|x| x == 3), Some(3));
        assert_eq!(ev.victim(|_| false), None);
        assert_eq!(ev.len(), 2);
        ev.gone(1);
        assert_eq!(ev.victim(|_| true), Some(2));
        assert_eq!(ev.victim(|_| true), None);
    }

    // Goal: an extent taken again soon after it left (its number a ghost)
    // goes to main, and outlives a scan. Method: one extent taken, dropped,
    // taken again; then a scan of the cache's size.
    #[test]
    fn a_ghost_taken_again_goes_to_main() {
        let (bound, mut cached) = (100, HashSet::new());
        let mut ev = Evictor::new(bound);
        run(&mut ev, &mut cached, bound, std::iter::once(7));
        run(&mut ev, &mut cached, bound, 1000..1150);
        assert!(!cached.contains(&7), "dropped by the scan once");
        run(&mut ev, &mut cached, bound, std::iter::once(7));
        run(&mut ev, &mut cached, bound, 2000..2200);
        assert!(cached.contains(&7), "back in main, it outlives the scan");
    }
}
