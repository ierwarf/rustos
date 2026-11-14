//! Retention policy for the shared-library cache objects pagerd published.
//!
//! - **Owner:** pagerd decides which warmed object to give back under memory
//!   pressure. Ring0 only reports free/reserve pages and, once an object is
//!   retired, reclaims frames that no mapping or pin still holds.
//! - **Boundary:** the pressure sample is a kernel snapshot; it names no object
//!   and grants no authority. An object is retired only through its handle.
//! - **Lifecycle:** `record` after seal, `plan` on a pressure sample, `forget`
//!   once the retire call has been issued. Retirement is permanent for this
//!   pager lease; re-warming is a separate policy decision.
//! - **Concurrency:** owned by the single warm/custodian thread.
//! - **Failure:** a full table refuses to record (the object stays published
//!   until the lease ends, never silently untracked-and-evicted).
//! - **Forbidden:** any frame, mapping or path decision; heat claims the kernel
//!   does not measure (order is the manifest's static priority).

use rustos_user_abi::syscall::LIBRARY_OBJECT_MAX_OBJECTS;

/// Pressure begins this many kernel reserves above the reserve itself, so the
/// cache is shed before `rootd`'s governor has a kill to weigh against it. A
/// runaway allocator crosses from here to the reserve in well under one poll
/// period (measured: 4 threads drained ~270k pages in < 1 s on 1 vCPU), so the
/// margin has to be wide: 32x the 3936-page reserve is ~490 MiB.
pub const HIGH_WATER_RESERVE_MULTIPLE: u64 = 32;

/// One kernel sample: free pages, the user-class reserve, and how many
/// allocations the watermark has refused since boot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Pressure {
    pub free_pages: u64,
    pub min_free_pages: u64,
    pub watermark_denials: u64,
}

impl Pressure {
    /// Free pages that end the pressure episode.
    pub const fn high_water(self) -> u64 {
        self.min_free_pages.saturating_mul(HIGH_WATER_RESERVE_MULTIPLE)
    }

    /// Pages the cache should try to give back; zero when there is no
    /// pressure. A zero reserve means the kernel reported nothing usable, which
    /// is never pressure.
    pub const fn shortfall(self) -> u64 {
        if self.min_free_pages == 0 {
            return 0;
        }
        self.high_water().saturating_sub(self.free_pages)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Held {
    object: u64,
    pages: u32,
    /// Position in the manifest's selection order; larger is colder.
    rank: u16,
}

pub struct Custody {
    held: [Option<Held>; LIBRARY_OBJECT_MAX_OBJECTS],
}

impl Default for Custody {
    fn default() -> Self {
        Self::new()
    }
}

impl Custody {
    pub const fn new() -> Self {
        Self { held: [None; LIBRARY_OBJECT_MAX_OBJECTS] }
    }

    /// Starts tracking a sealed object. False when the table is full or the
    /// handle is already tracked.
    pub fn record(&mut self, object: u64, pages: u64, rank: u16) -> bool {
        let Ok(pages) = u32::try_from(pages) else { return false };
        if object == 0 || pages == 0 || self.held.iter().flatten().any(|held| held.object == object) {
            return false;
        }
        match self.held.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some(Held { object, pages, rank });
                true
            }
            None => false,
        }
    }

    pub fn forget(&mut self, object: u64) {
        for slot in &mut self.held {
            if slot.is_some_and(|held| held.object == object) {
                *slot = None;
            }
        }
    }

    fn pages_of(&self, object: u64) -> u64 {
        self.held
            .iter()
            .flatten()
            .find(|held| held.object == object)
            .map_or(0, |held| u64::from(held.pages))
    }

    pub fn objects(&self) -> usize {
        self.held.iter().flatten().count()
    }

    pub fn pages(&self) -> u64 {
        self.held.iter().flatten().map(|held| u64::from(held.pages)).sum()
    }

    /// Picks objects to retire for `shortfall` pages, coldest first (largest
    /// rank; a larger object first among equals so fewer retirements cover the
    /// need), writing handles into `out`. Returns how many were chosen and the
    /// pages they hold. Never chooses more than needed: it stops once the sum
    /// reaches `shortfall`, but retires everything when the whole cache is
    /// smaller than the shortfall.
    pub fn plan(&self, shortfall: u64, out: &mut [u64]) -> (usize, u64) {
        if shortfall == 0 {
            return (0, 0);
        }
        let mut chosen = 0;
        let mut covered = 0_u64;
        let mut taken = [false; LIBRARY_OBJECT_MAX_OBJECTS];
        while covered < shortfall && chosen < out.len() {
            let next = self
                .held
                .iter()
                .enumerate()
                .filter(|(index, held)| held.is_some() && !taken[*index])
                .max_by_key(|(_, held)| {
                    let held = held.expect("filtered to Some");
                    (held.rank, held.pages, core::cmp::Reverse(held.object))
                })
                .map(|(index, _)| index);
            let Some(index) = next else { break };
            taken[index] = true;
            let held = self.held[index].expect("selected a held slot");
            out[chosen] = held.object;
            chosen += 1;
            covered += u64::from(held.pages);
        }
        (chosen, covered)
    }
}

/// What one retention pass needs from the outside world.
pub trait ReclaimHost {
    fn pressure(&mut self) -> Result<Pressure, i64>;
    /// Withdraws the object. `Err` means the kernel no longer knows the handle
    /// (a lease change dropped it) or refused; either way pagerd stops tracking.
    fn retire(&mut self, object: u64) -> Result<(), i64>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reclaimed {
    pub objects: u16,
    pub pages: u64,
    pub free_pages: u64,
}

/// One retention pass: sample pressure and, if the cache should shrink, retire
/// the coldest objects covering the shortfall. `None` when nothing was retired
/// (no pressure, empty custody, or an unreadable sample).
pub fn reclaim_step<H: ReclaimHost>(custody: &mut Custody, host: &mut H) -> Option<Reclaimed> {
    if custody.objects() == 0 {
        return None;
    }
    let pressure = host.pressure().ok()?;
    let shortfall = pressure.shortfall();
    let mut victims = [0_u64; LIBRARY_OBJECT_MAX_OBJECTS];
    let (count, _) = custody.plan(shortfall, &mut victims);
    let mut done = Reclaimed { free_pages: pressure.free_pages, ..Reclaimed::default() };
    for &object in &victims[..count] {
        let pages = custody.pages_of(object);
        if host.retire(object).is_ok() {
            done.objects += 1;
            done.pages += pages;
        }
        // A refused retire is still forgotten: a handle the kernel rejects is
        // stale, and retrying it forever would pin the custody table.
        custody.forget(object);
    }
    (done.objects != 0).then_some(done)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn custody(rows: &[(u64, u64, u16)]) -> Custody {
        let mut custody = Custody::new();
        for &(object, pages, rank) in rows {
            assert!(custody.record(object, pages, rank));
        }
        custody
    }

    fn pressure(free: u64, min: u64) -> Pressure {
        Pressure { free_pages: free, min_free_pages: min, watermark_denials: 0 }
    }

    fn plan(custody: &Custody, shortfall: u64) -> (Vec<u64>, u64) {
        let mut out = [0_u64; 16];
        let (count, covered) = custody.plan(shortfall, &mut out);
        (out[..count].to_vec(), covered)
    }

    #[test]
    fn no_pressure_or_unknown_reserve_retires_nothing() {
        assert_eq!(pressure(10_000, 100).shortfall(), 0);
        assert_eq!(pressure(3200, 100).shortfall(), 0, "exactly at the high water is calm");
        assert_eq!(pressure(0, 0).shortfall(), 0, "a zero reserve is a missing sample, not pressure");
        let held = custody(&[(1, 10, 0)]);
        assert_eq!(plan(&held, 0), (Vec::new(), 0));
    }

    #[test]
    fn shortfall_is_the_gap_to_the_high_water() {
        assert_eq!(pressure(500, 100).shortfall(), 2700);
        assert_eq!(pressure(0, 100).shortfall(), 3200);
    }

    #[test]
    fn coldest_object_goes_first_and_the_plan_stops_once_covered() {
        // Rank 0 is the most popular (selected first); rank 3 the coldest.
        let held = custody(&[(10, 100, 0), (11, 50, 1), (12, 40, 2), (13, 30, 3)]);
        assert_eq!(plan(&held, 20), (std::vec![13], 30));
        assert_eq!(plan(&held, 31), (std::vec![13, 12], 70));
        assert_eq!(plan(&held, 1_000), (std::vec![13, 12, 11, 10], 220));
    }

    #[test]
    fn among_equal_rank_the_larger_object_is_retired_first() {
        let held = custody(&[(1, 8, 5), (2, 64, 5), (3, 16, 5)]);
        assert_eq!(plan(&held, 1), (std::vec![2], 64));
    }

    #[test]
    fn a_forgotten_object_is_never_planned_again_and_slots_are_reused() {
        let mut held = custody(&[(1, 8, 0), (2, 8, 1)]);
        held.forget(2);
        assert_eq!(plan(&held, 100), (std::vec![1], 8));
        assert!(held.record(3, 4, 2));
        assert_eq!((held.objects(), held.pages()), (2, 12));
    }

    #[test]
    fn recording_refuses_duplicates_zero_and_overflow() {
        let mut held = Custody::new();
        assert!(held.record(1, 4, 0));
        assert!(!held.record(1, 4, 0));
        assert!(!held.record(0, 4, 0));
        assert!(!held.record(2, 0, 0));
        assert!(!held.record(3, u64::from(u32::MAX) + 1, 0));
        for object in 100..100 + LIBRARY_OBJECT_MAX_OBJECTS as u64 - 1 {
            assert!(held.record(object, 1, 0));
        }
        assert!(!held.record(9_999, 1, 0), "a full table refuses instead of overwriting");
    }

    #[test]
    fn the_plan_is_bounded_by_the_output_slice() {
        let held = custody(&[(1, 1, 0), (2, 1, 1), (3, 1, 2)]);
        let mut out = [0_u64; 2];
        assert_eq!(held.plan(100, &mut out), (2, 2));
        assert_eq!(out, [3, 2]);
    }

    struct Fake {
        pressure: Result<Pressure, i64>,
        retired: Vec<u64>,
        refuse: Vec<u64>,
    }
    impl ReclaimHost for Fake {
        fn pressure(&mut self) -> Result<Pressure, i64> {
            self.pressure
        }
        fn retire(&mut self, object: u64) -> Result<(), i64> {
            if self.refuse.contains(&object) {
                return Err(-116);
            }
            self.retired.push(object);
            Ok(())
        }
    }
    fn fake(free: u64, min: u64) -> Fake {
        Fake { pressure: Ok(pressure(free, min)), retired: Vec::new(), refuse: Vec::new() }
    }

    #[test]
    fn a_calm_machine_keeps_every_object() {
        let mut held = custody(&[(1, 10, 0), (2, 10, 1)]);
        let mut host = fake(10_000, 100);
        assert_eq!(reclaim_step(&mut held, &mut host), None);
        assert!(host.retired.is_empty());
        assert_eq!(held.objects(), 2);
    }

    #[test]
    fn pressure_retires_the_coldest_objects_that_cover_the_shortfall() {
        let mut held = custody(&[(1, 100, 0), (2, 50, 1), (3, 40, 2)]);
        // high water 3200, free 3170: shortfall 30 -> one object is enough.
        let mut host = fake(3170, 100);
        let done = reclaim_step(&mut held, &mut host).expect("pressure retires");
        assert_eq!(host.retired, std::vec![3]);
        assert_eq!((done.objects, done.pages, done.free_pages), (1, 40, 3170));
        assert_eq!(held.objects(), 2);
        // Still short on the next pass: the next coldest goes.
        let mut host = fake(3100, 100);
        reclaim_step(&mut held, &mut host).expect("still under pressure");
        assert_eq!(host.retired, std::vec![2, 1]);
        assert_eq!(held.objects(), 0);
        assert_eq!(reclaim_step(&mut held, &mut fake(0, 100)), None, "nothing left to give");
    }

    #[test]
    fn a_stale_handle_is_forgotten_and_does_not_count_as_reclaimed() {
        let mut held = custody(&[(1, 10, 0), (2, 10, 1)]);
        let mut host = fake(0, 100);
        host.refuse.push(2);
        let done = reclaim_step(&mut held, &mut host).expect("object 1 still retires");
        assert_eq!(host.retired, std::vec![1]);
        assert_eq!((done.objects, done.pages), (1, 10));
        assert_eq!(held.objects(), 0, "the rejected handle is no longer tracked");
    }

    #[test]
    fn an_unreadable_pressure_sample_retires_nothing() {
        let mut held = custody(&[(1, 10, 0)]);
        let mut host = Fake { pressure: Err(-1), retired: Vec::new(), refuse: Vec::new() };
        assert_eq!(reclaim_step(&mut held, &mut host), None);
        assert_eq!(held.objects(), 1);
    }
}
