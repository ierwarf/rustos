/// `RunOwnerWord` against its lock-free metadata writers. The word packs the
/// custody (generation and state) with run-intent and wait-arm bits. Metadata
/// writers (`set_runnable`, `publish_wait_arm`, ...) edit in place through one
/// load/decide/CAS loop; the retirement CAS advances the generation twice
/// (`Retiring`, then `Retired`) and the second step is an exact-value CAS that
/// cannot tolerate any other writer. A writer therefore refuses every terminal
/// word instead of retrying into it. Shuttle PCT permutations of the Loom kernel; the model excludes the run queues,
/// the cleanup cell and the generation-width limit.
mod owner_word_kernel {
    use super::*;

    pub const LIVE: usize = 1;
    pub const RETIRING: usize = 2;
    pub const RETIRED: usize = 3;
    pub const RUN: usize = 1;
    pub const WAIT: usize = 2;

    pub fn pack(generation: usize, state: usize, meta: usize) -> usize {
        generation << 8 | state << 4 | meta
    }

    pub fn generation(word: usize) -> usize {
        word >> 8
    }

    pub fn state(word: usize) -> usize {
        (word >> 4) & 0xf
    }

    pub fn meta(word: usize) -> usize {
        word & 0xf
    }

    /// `edit_in_place`: the generation, state and CPU never change. `guarded`
    /// is the production rule (refuse a word that is not editable); the
    /// unguarded form is the pre-fix writer that retried into a terminal word.
    pub fn edit(word: &AtomicUsize, guarded: bool, set: usize, clear: usize) {
        // ORDERING: the CAS below validates the loaded value exactly.
        let mut observed = word.load(Ordering::Acquire);
        loop {
            if guarded && state(observed) != LIVE {
                return;
            }
            let next = pack(
                generation(observed),
                state(observed),
                (meta(observed) | set) & !clear,
            );
            if next == observed {
                return;
            }
            // ORDERING: SeqCst is the owner-word linearization point.
            match word.compare_exchange(observed, next, Ordering::SeqCst, Ordering::Acquire) {
                Ok(_) => return,
                Err(current) => observed = current,
            }
        }
    }

    /// `retire_slot`: `true` when the second (completion) CAS won.
    pub fn retire(word: &AtomicUsize) -> bool {
        loop {
            let observed = word.load(Ordering::Acquire);
            assert_eq!(state(observed), LIVE, "single retirer expected");
            // Terminal words are canonical: generation + 1, no metadata.
            let retiring = pack(generation(observed) + 1, RETIRING, 0);
            if word
                .compare_exchange(observed, retiring, Ordering::SeqCst, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            let retired = pack(generation(retiring) + 1, RETIRED, 0);
            return word
                .compare_exchange(retiring, retired, Ordering::SeqCst, Ordering::Acquire)
                .is_ok();
        }
    }

    pub fn case(guarded: bool) {
        shuttle::check_pct(move || {
            let word = Arc::new(AtomicUsize::new(pack(1, LIVE, 0)));
            let retirer = {
                let word = word.clone();
                thread::spawn(move || retire(&word))
            };
            // publish_frame's late run-intent write, then a wait arm replacing it.
            let writer = {
                let word = word.clone();
                thread::spawn(move || {
                    edit(&word, guarded, RUN, 0);
                    edit(&word, guarded, WAIT, RUN);
                })
            };
            let completed = retirer.join().unwrap();
            writer.join().unwrap();
            assert!(completed, "owner word invariant: retire completion lost to a metadata writer");
            let last = word.load(Ordering::Acquire);
            assert_eq!(state(last), RETIRED);
            assert_eq!(meta(last), 0, "owner word invariant: terminal word carries metadata");
            assert_eq!(generation(last), 3, "owner word invariant: generation advanced exactly twice");
        }, iterations(), pct_depth());
    }
}

#[test]
fn owner_word_terminal_beats_late_metadata_writers_pct() {
    owner_word_kernel::case(true);
}
