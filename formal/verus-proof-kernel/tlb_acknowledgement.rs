use vstd::prelude::*;

verus! {

/// Unbounded mathematical form of the exact-generation check in
/// kernel-hal::arch::tlb_shootdown::acknowledgements_complete.  The executable
/// Rust function and its focused test remain the source-level counterpart;
/// this kernel proves the arbitrary-length partition rather than pretending
/// to execute ring0 or an IPI handler in Verus.
pub open spec fn acknowledgements_complete(
    targets: Seq<bool>,
    acknowledgements: Seq<u64>,
    generation: u64,
) -> bool {
    targets.len() == acknowledgements.len()
        && forall |index: int|
            0 <= index < targets.len()
                ==> !targets[index] || acknowledgements[index] == generation
}

proof fn complete_requires_an_exact_acknowledgement_for_each_target(
    targets: Seq<bool>,
    acknowledgements: Seq<u64>,
    generation: u64,
    index: int,
)
    requires
        acknowledgements_complete(targets, acknowledgements, generation),
        0 <= index < targets.len(),
    ensures
        !targets[index] || acknowledgements[index] == generation,
{
    reveal(acknowledgements_complete);
}

proof fn mismatched_acknowledgement_shape_cannot_admit_reclaim(
    targets: Seq<bool>,
    acknowledgements: Seq<u64>,
    generation: u64,
)
    requires targets.len() != acknowledgements.len(),
    ensures !acknowledgements_complete(targets, acknowledgements, generation),
{
}

} // verus!

fn main() {}
