/// The per-slot resident cell against an arbitrarily delayed revoking hook. The
/// cell word is 0 (empty), a generation (published) or `BUSY` (one CPU moves the
/// payload). Generation 1 is parked and its owner transition already happened, but its
/// hook is still pending, while the woken task has run, blocked again in
/// generation 3 and parks into the same cell. The
/// parker claims the cell by CAS (retiring a stale published cell itself), fills
/// it, publishes with a SeqCst swap and re-reads the owner word; a hook or a
/// re-read that finds the owner moved takes the cell back. The payload is a
/// `loom::cell::UnsafeCell`, so any access outside a held `BUSY` claim is a
/// data race Loom reports. The bounded model excludes the pin/permit types and
/// the lane (see the TLA+ model) and uses two generations of one slot.
mod resident_cell_kernel {
    use super::*;
    use loom::cell::UnsafeCell;

    pub const BUSY: usize = usize::MAX;
    pub const BLOCKED: usize = 1;
    pub const RUNNING: usize = 0;

    pub struct Cell {
        pub word: AtomicUsize,
        pub payload: UnsafeCell<usize>,
        pub owner: AtomicUsize,
        pub dropped: [AtomicUsize; 4],
    }

    // SAFETY: the payload is accessed only by the CPU that holds the word BUSY.
    unsafe impl Sync for Cell {}

    pub fn owner(generation: usize, blocked: usize) -> usize {
        generation << 1 | blocked
    }

    fn drain(cell: &Cell, expected: usize) {
        let tag = cell.payload.with(|payload| unsafe { *payload });
        assert_eq!(tag, expected, "resident cell invariant: drained a foreign payload");
        cell.dropped[tag].fetch_add(1, Ordering::Relaxed);
    }

    /// `claim_empty`; `claimed` is the pre-fix behaviour (assert an empty cell).
    fn claim(cell: &Cell, generation: usize, claimed: bool) -> bool {
        if !claimed {
            assert_eq!(
                cell.word.load(Ordering::Relaxed),
                0,
                "resident cell invariant: parked over live loans"
            );
            cell.word.store(BUSY, Ordering::Relaxed);
            return true;
        }
        loop {
            match cell.word.load(Ordering::Relaxed) {
                BUSY => return false,
                newer if newer > generation => return false,
                held => {
                    assert_ne!(held, generation, "resident cell parked twice for one generation");
                    if cell
                        .word
                        .compare_exchange(held, BUSY, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                    {
                        if held != 0 {
                            drain(cell, held);
                        }
                        return true;
                    }
                    thread::yield_now();
                }
            }
        }
    }

    pub fn take(cell: &Cell, generation: usize) {
        if cell
            .word
            .compare_exchange(generation, BUSY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            drain(cell, generation);
            // ORDERING: Release orders the payload read before any claim.
            cell.word.store(0, Ordering::Release);
        }
    }

    pub fn park(cell: &Cell, generation: usize, claimed: bool) {
        if !claim(cell, generation, claimed) {
            // Obsolete park: release the task's own loans.
            cell.dropped[generation].fetch_add(1, Ordering::Relaxed);
            return;
        }
        cell.payload.with_mut(|payload| unsafe { *payload = generation });
        // ORDERING: SeqCst publication pairs with the SeqCst owner CAS and hook load.
        let previous = cell.word.swap(generation, Ordering::SeqCst);
        assert_eq!(previous, BUSY, "resident cell lost its fill claim");
        // Loom does not order SeqCst accesses against each other; the fence is
        // the store-buffer barrier the x86 `xchg` provides (see the herd7 litmus).
        fence(Ordering::SeqCst);
        if cell.owner.load(Ordering::SeqCst) != owner(generation, BLOCKED) {
            take(cell, generation);
        }
    }

    pub fn hook(cell: &Cell, previous: usize) {
        // ORDERING: SeqCst pairs with the parker's publication swap; the fence
        // is the barrier of the owner CAS that preceded this hook.
        fence(Ordering::SeqCst);
        if cell.word.load(Ordering::SeqCst) == previous {
            take(cell, previous);
        }
    }

    pub fn case(claimed: bool) {
        loom::model(move || {
            // Generation 1 is parked; its owner transition (to Running in
            // generation 2) already happened and its hook is still pending.
            let cell = Arc::new(Cell {
                word: AtomicUsize::new(1),
                payload: UnsafeCell::new(1),
                owner: AtomicUsize::new(owner(2, RUNNING)),
                dropped: std::array::from_fn(|_| AtomicUsize::new(0)),
            });
            // The woken task blocks again in generation 3 and parks.
            let task = {
                let cell = cell.clone();
                thread::spawn(move || {
                    cell.owner
                        .compare_exchange(
                            owner(2, RUNNING),
                            owner(3, BLOCKED),
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .unwrap();
                    park(&cell, 3, claimed);
                })
            };
            // The evictor runs generation 1's late hook, then may take the task
            // out of generation 3 while its park is still filling or publishing;
            // that hook may run before the publication, in which case only the
            // parker's own re-read sees the transition.
            let evictor = {
                let cell = cell.clone();
                thread::spawn(move || {
                    hook(&cell, 1);
                    let won = cell
                        .owner
                        .compare_exchange(
                            owner(3, BLOCKED),
                            owner(4, RUNNING),
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok();
                    if won {
                        hook(&cell, 3);
                    }
                    won
                })
            };
            task.join().unwrap();
            let moved = evictor.join().unwrap();
            let word = cell.word.load(Ordering::Acquire);
            let first = cell.dropped[1].load(Ordering::Relaxed);
            let third = cell.dropped[3].load(Ordering::Relaxed);
            assert_eq!(first, 1, "resident cell invariant: generation 1 loans dropped {first} times");
            assert_eq!(
                third + usize::from(word == 3),
                1,
                "resident cell invariant: generation 3 loans dropped {third} times, parked {word}"
            );
            assert!(
                word == 0 || word == 3,
                "resident cell invariant: cell left {word} at rest"
            );
            if moved {
                assert_eq!(word, 0, "resident cell invariant: cell outlived its generation");
            }
        });
    }
}

#[test]
fn resident_cell_delayed_hook_reparks_without_overwrite_or_double_drop() {
    resident_cell_kernel::case(true);
}

#[test]
#[should_panic(expected = "resident cell invariant")]
fn resident_cell_pre_fix_park_asserts_an_empty_cell() {
    // Negative sensitivity fixture: the pre-fix park asserted an empty cell, so a
    // late hook for the previous generation made it panic over live loans.
    resident_cell_kernel::case(false);
}
