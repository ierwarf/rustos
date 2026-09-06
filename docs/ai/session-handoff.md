# Session Handoff

**Role:** volatile routing note for resuming this checkout. It is not build,
runtime, formal, or hardware evidence. Source, the live goal tracker, and fresh
command output win when they disagree with this page.

## Current change set

This checkout is completing the unified address-space authority and production
copy-on-write change set. All source changes in this tree belong to that one
change set. Do not split or discard them without first reconstructing their
contract and formal dependencies.

The worktree is intentionally dirty until this single validated change set is
committed. Preserve every scoped path during continuation and audit live Git
state before editing.

The target is:

- one VMA authority for eager, demand-paged, Linux, and Windows reservations;
- explicit `Reserved` versus `Committed` state;
- a boot-sized physical-frame descriptor ledger with bounded exact
  `(root, virtual address, frame)` aliases;
- Linux anonymous-fork COW plus a common private-file/section COW role for
  Linux `MAP_PRIVATE` and Windows `PAGE_WRITECOPY` frontends;
- copy/split, unmap, decommit, retirement, and reclamation transitions that
  reconcile PTEs, VMA authority, and the frame ledger;
- mutable pager policy published by seqlock, with a bounded IRQ-off read and a
  harmless fault-around fallback.

The architecture and invariant owners are
`address-space-lifecycle-contract.md`, `fork-cow-contract.md`,
`pager-protocol-contract.md`, and `page-table-reclamation-contract.md`.

## Implemented ownership model

`pager_vma` is the reservation and commitment authority. The former eager-only
`regions` vector and inert Windows allocation vector are gone. Windows reserve
and commit semantics therefore do not create a third or fourth allocation
ledger.

`frame_descriptor_ledger` replaces both `owned_frames` and the table-only lazy
ledger. It records frame role and bounded exact aliases. Anonymous-fork and
private-file/section sharing are distinct roles in the same frame-oriented
authority, so Windows section COW does not require re-keying an anonymous-only
ledger later.

Fork publishes a child only after reservation cloning, parent write downgrade,
exact alias installation, acknowledged invalidation, and ledger reconciliation.
Clone-internal failure restores the parent's downgraded PTEs. A later
pre-activation cancellation removes every child alias, but may leave the sole
parent alias read-only/COW; its next logical write promotes it in place. Do not
claim that every late rollback eagerly restores a writable parent PTE.

Exception-time COW uses a try-only mutation path. Normal-context kernel copyout
uses a bounded resolver tied to the retained target root and exact live process
identity. It does not infer authority from the current CR3.

## Copyout failure class and hardening

The direct-store probe originally hid a generic bug: `copy_to_user` validation
rejected a legitimate read-only COW PTE before the architecture could raise a
write fault. `pread(fd, untouched_cow_page, ...)` consequently returned
`EFAULT` in the child even though a direct user store worked.

The durable rule is that kernel copyout is a logical user write. The PS wrapper
must prove an exact live process/MM binding and a committed, writable, private
VMA before asking MM to split COW. MM must resolve against that retained root,
then translate and validate again so the old physical frame cannot enter the
write proof. Read-only usercopy keeps the old fast path. Shared, reserved,
non-writable, device, memfd, and unknown objects fail closed.

The regression witnesses cover both anonymous-fork and private-file/image
roles, rejected authority classes, a `pread` into an untouched child COW page,
and parent/child divergence on two pages. The TLA lifecycle also requires a
logical write and proves that cancellation leaves no invisible child alias.

## Mandatory completion discipline

Every bug fix must name and harden the violated invariant and add a witness for
the whole failure class, not only the observed input. Every source-code commit
must update an applicable Markdown owner/flow contract in the same commit. A
source-only commit is incomplete. This is a default completion rule, not an
optional follow-up requested per bug.

Before committing this change set, the fresh source hash must satisfy:

1. formatting and `git diff --check`;
2. focused MM, PS, compat, pagerd, and user-ABI tests;
3. `cargo xtask check` and the formal-contract registry checks;
4. `formal/verify-all.sh --profile pr`, including mutation witnesses;
5. the 8-vCPU isolated `fork_cow_private_write` KVM probe, with its final PASS
   line and process exit code zero;
6. Serena diagnostics plus focused ast-grep and CodeGraph impact probes;
7. a final diff audit proving every staged path belongs to this change set.

The KVM probe is also the performance witness. Compare its cycle distribution
with the same-tree mmap/munmap anchor; do not promote a one-off number into a
new baseline without the benchmark contract's required repetitions.

## Scope boundary

The shared private-file/section role and split mechanism are present. Publishing
actual Linux file-backed `MAP_PRIVATE` VMAs and wiring Win32 section APIs are
frontend work, not permission to introduce parallel reservation or frame
ledgers. Those frontends must use the authority and roles established here.

Do not edit tracked files while a KVM or sealed formal lane is running. If a
build is interrupted, resume it without `clean` or `distclean`. Never bypass
hooks or signing.

## Refresh rule

When resuming, inspect the live goal and `git status` first. Replace this page's
volatile status if the goal changes; keep durable invariant history in the
owner contracts above. If this exact change set is already committed and the
tree is clean, no open work remains under this handoff.
