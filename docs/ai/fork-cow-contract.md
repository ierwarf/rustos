# Fork private-COW admission contract

> **Status: design gate; not a shipped capability.**  Fork still performs an
> eager copy.  This document names the properties that must be implemented and
> proven together before a private anonymous mapping may be shared across a
> fork.  It does not describe pagerd's file-page COW policy.

## 0. Prerequisite

`address-space-lifecycle-contract.md` §1 must land first.  Fork does not
currently inherit the parent's pager VMAs at all, so the child has no
reservation to convert into a shared one, and every rule below assumes a child
whose reservation matches its parent's.

## 1. Scope and authority

The first implementation may cover only private, anonymous, normal user leaf
pages.  Shared mappings, pager-backed/file mappings, device/DMA mappings,
huge pages, and kernel mappings remain non-COW and must retain their existing
fork rules.  A PTE's writable bit is not the authority to write a private COW
page.  That authority is the conjunction of:

1. a live VMA that permits write,
2. a live per-root COW-leaf record for the exact virtual address and physical
   frame, and
3. an exact shared-frame ownership record.

The COW-leaf record is needed even when the PTE has a software COW tag: the
tag helps classify a hardware fault, but cannot recover a frame identity or
resolve a concurrent unmap/exit.  Both ledgers are fixed-capacity or
pre-admitted before publication.  An allocation, lookup, or lock that can
block is forbidden on the present-write fault path.

## 2. Frame and root states

Each shareable frame has one bounded ownership record:

```
Exclusive(root, va) -> Shared(readers >= 2) -> Replacing(root, va)
                         -> Exclusive(root, va) | Retired
```

`readers` counts exact live root/virtual mappings, not process handles or
VMA intent.  A root COW ledger maps each tagged leaf to that record and is
retained with the address-space object.  Therefore an old exec address space
can coexist with its replacement, and exit/unmap cannot release a physical
frame merely because a process slot or MM generation changed.

A reference reaches zero only after its PTE is no longer reachable *and* the
exact TLB invalidation generation has been acknowledged.  The final owner
then returns the data frame through the normal physical-frame lifecycle.

## 3. Fork transaction

Fork has one all-or-nothing publication boundary: the child becomes runnable.
Before that boundary it must:

1. validate every candidate VMA and pre-admit all child PTE and ledger
   capacity;
2. bind an additional exact frame reference and child COW-leaf record for
   each shareable page;
3. install the child leaf read-only and COW-tagged, and atomically downgrade
   the corresponding parent leaf to the same read-only COW form;
4. invalidate and receive acknowledgement for every parent translation that
   could still be writable; and
5. publish the child only after every record and acknowledgement is complete.

If any step fails, the child remains invisible.  Rollback restores every
parent leaf that it changed before releasing the child record or extra frame
reference; it may not expose a temporarily read-only parent, leak a frame, or
make a runnable child whose mapping ledger is incomplete.  `clone_user_space`
cannot remain an immutable `&self` byte-copy helper for this transaction.

## 4. Present-write fault

The current anonymous first-touch path proves only absent-to-present
publication.  A COW write therefore needs a distinct, non-allocating
present-write classification path that accepts only an exact present,
read-only, COW-tagged leaf backed by the live VMA and root ledger.

The winner claims `Replacing(root, va)`, preserves the original user/NX/cache
rights, copies to a newly admitted frame, replaces the PTE, and acknowledges
the exact invalidation before retiring its old shared reference.  If it sees
that the frame has one remaining mapping, it may promote that exact COW leaf
to writable only after the same VMA/ledger revalidation and invalidation
protocol.  Concurrent writers, unmap, protect, exec, and exit either retry
against the new leaf identity or fail closed; none may consume the same
reference twice.

## 5. Protection and teardown

`mprotect(PROT_WRITE)` updates VMA intent only.  It must not make a COW-tagged
PTE writable; a subsequent write fault resolves the private copy.  `munmap`,
partial split/trim, exec replacement, and address-space retirement must first
remove the exact COW-leaf record, invalidate the translation, await the
generation acknowledgement, and only then decrement the shared-frame record.

Exec keeps the old root and its COW ledger with `StagedExec.old_state` through
scheduler root publication, finalization, and old-state drop.  Fork keeps the
child suspended until the parent downgrade acknowledgement completes.  These
rules prevent a slot/MM-generation transition from becoming ownership
transfer.

## 6. Required proof and acceptance evidence

Before enabling fork COW, add a model and source tests covering at least:

- fork success and each rollback point;
- two simultaneous write faults to one inherited leaf;
- write fault racing `munmap`, `mprotect`, exec, and exit;
- child activation only after parent writable-TLB acknowledgement;
- exact frame reuse only after the last mapping's acknowledgement; and
- denial for every excluded mapping class.

The model must distinguish parent and child roots, PTE identity, shared-frame
reference count, leaf-ledger membership, and TLB generation.  A count-only
model is insufficient: it cannot detect a duplicate owner or a stale leaf
reusing a physical frame.

## 7. References

Linux also treats COW mapping eligibility separately from sharedness and
writeability, and implements fork COW by write-protecting mappings so a later
write fault installs a copied page.  The architecture here follows that
semantic boundary, while retaining RustOS's fixed-resource and exact-TLB-ack
requirements.

- <https://docs.kernel.org/core-api/mm-api.html>
- <https://www.kernel.org/doc/gorman/pdf/understand.pdf>
