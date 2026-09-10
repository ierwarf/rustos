# Session Handoff

**Role:** volatile resume note for the current checkout. Live goal state, Git
status/diff, source, and fresh command output override this file.

## Current change set

The local checkout is completing one unified address-space/COW change set and
is intentionally dirty. Preserve every scoped tracked/untracked path; inspect
live Git state before editing, and do not split or discard the set without
reconstructing its contract/formal dependencies.

Target/owners:

- `pager_vma` is the single reservation/commit authority with explicit
  `Reserved` versus `Committed` state.
- `frame_descriptor_ledger` owns frame roles and bounded exact
  `(root, virtual address, frame)` aliases.
- Anonymous-fork and private-file/section COW share that frame authority;
  Linux `MAP_PRIVATE` and Windows `PAGE_WRITECOPY` frontends must not create a
  parallel allocation ledger.
- Copy/split, unmap, decommit, retirement, and reclamation reconcile PTE, VMA,
  and frame-ledger state. Pager policy uses bounded seqlock publication.
- Durable contracts: `address-space-lifecycle-contract.md`,
  `fork-cow-contract.md`, `pager-protocol-contract.md`, and
  `page-table-reclamation-contract.md`.

## Critical current invariants

Fork publishes a child only after reservation clone, parent write downgrade,
exact alias installation, acknowledged invalidation, and ledger reconciliation.
Clone-internal failure restores parent downgrades. Pre-activation cancellation
removes child aliases but may leave the sole parent alias read-only/COW; the
next logical write may promote it in place.

Exception-time COW is try-only. Kernel copyout is a logical user write: prove
an exact live process/MM binding and committed writable private VMA, resolve
against the retained target root, then translate/validate again. Shared,
reserved, non-writable, device, memfd, and unknown objects fail closed. Keep the
`pread`-into-untouched-child-COW regression plus parent/child divergence and
rejected-authority witnesses.

## Completion gate

Before committing the source change set, use fresh source state for:

1. formatting and `git diff --check`;
2. focused MM/PS/compat/pagerd/user-ABI tests;
3. `cargo xtask check` and formal-contract registry checks;
4. `formal/verify-all.sh --profile pr`, including mutation witnesses;
5. the isolated 8-vCPU `fork_cow_private_write` KVM probe with PASS + exit 0;
6. focused Serena references/diagnostics, compiler/regression impact checks;
7. final staged-diff ownership audit.

For this architectural/bug-fix set, update the applicable owner/flow contract
and invariant-level regression witness with the source change. Treat the KVM
probe as performance evidence only against the same-tree benchmark anchor.

## Resume rules

Do not edit tracked files while a KVM or sealed formal lane is running. Resume
interrupted builds without `clean`/`distclean`; never bypass hooks/signing.
Refresh this note when the live goal changes. If this change set is already
committed and the local tree is clean, it has no remaining handoff work.
