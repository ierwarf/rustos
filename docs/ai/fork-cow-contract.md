# Fork private-COW admission contract

> **Status: Linux private-anonymous fork COW is implemented.** File-private and
> image-section mappings use the same descriptor and write-fault substrate;
> publishing those mappings from pagerd and the Win32 memory API remains a
> separate frontend task. This contract is the admission and lifetime rule for
> every enabled private COW leaf.

## 0. Prerequisite

`address-space-lifecycle-contract.md` §1 has landed: a Linux fork inherits the
parent's complete VMA reservation before the child runs. The physical frame
catalog and its boot-sized exact-mapping pool have also landed. Windows
`CreateProcess` deliberately does not use the fork path; it constructs a fresh
image address space.

## 1. Scope and authority

Linux fork admits committed private-anonymous normal 4 KiB user leaves.
Shared mappings, device/DMA mappings, huge pages, and kernel mappings remain
non-COW and retain their existing fork rules. File-private and image-section
leaves can be admitted explicitly with a nonzero backing identity; they are
always write-copy, even with one mapping. A PTE's writable bit is not the
authority to write a private COW page. That authority is the conjunction of:

1. a live VMA that permits write,
2. a live per-root COW-leaf record for the exact virtual address and physical
   frame, and
3. an exact shared-frame ownership record.

The exact mapping record is needed even when the PTE has a software COW tag:
the tag classifies the hardware fault, but cannot recover a frame identity or
resolve a concurrent unmap/exit. Descriptor and mapping metadata are
boot-sized and pre-admitted before publication. The only fresh allocation on a
copying write fault is its already-reserved data frame; metadata does not
allocate or take a sleepable lock.

## 2. Frame and root states

Each shareable frame has one bounded physical descriptor whose role identifies
anonymous-fork COW rather than private-file/section COW:

```
Exclusive(root, va) -> Shared(readers >= 2) -> Replacing(root, va)
                         -> Exclusive(root, va) | Retired
```

`readers` counts exact live root/virtual mappings, not process handles or VMA
intent. The physical descriptor holds the first mapping inline and links any
additional `(root, virtual_address)` records from a boot-sized pool. Each root
also counts its live data leaves. Therefore an old exec address space can
coexist with its replacement, and exit/unmap cannot release a physical frame
merely because a process slot or MM generation changed.

A reference reaches zero only after its PTE is no longer reachable *and* the
exact TLB invalidation generation has been acknowledged.  The final owner
then returns the data frame through the normal physical-frame lifecycle.

## 3. Fork transaction

Fork has one all-or-nothing publication boundary: the child becomes runnable.
The process-state lock and VMA writer form a single `mmap_lock`-equivalent
transaction. Every live VMA is held at an odd publication sequence and all
fault installers are drained before leaf cloning begins. Before the runnable
boundary fork must:

1. snapshot every candidate VMA and pre-admit child PTE and mapping-record
   capacity;
2. bind an additional exact frame reference and child COW-leaf record for
   each shareable page;
3. install the child leaf read-only and COW-tagged, and atomically downgrade
   the corresponding parent leaf to the same read-only COW form;
4. invalidate and receive acknowledgement for every parent translation that
   could still be writable; and
5. publish the inherited VMA set over the already-cloned resident leaves; and
6. publish the child runnable only after every record and acknowledgement is
   complete.

Step 5 uses the fork-only inherited publication entrypoint. The ordinary mmap
entrypoint MUST continue to require an empty target page-table range, while the
inherited entrypoint MUST be callable only for a suspended child that is
destroyed on failure. The child template preserves `PROT_NONE`,
reserved/committed state, object rights, sharing, file/image backing identity,
object offset, and fault endpoint. Only process, MM, and VMA generation stamps
are cleared for `kernel-ps` to author again. Anonymous-private objects alone
receive a fresh object slot so future zero-fill faults remain child-private;
anonymous-shared inheritance fails closed until it has a proved shared-write
lifecycle.

`CLONE_CHILD_SETTID` adds one kernel-copyout exception to aliasing. Before the
clone, the parent VMA set must cover the complete word with committed write
authority and the target page(s) must already be resident. The clone eagerly
copies those exact leaves, clears the COW tag, and makes them writable before
`copy_into_user` runs. The child remains suspended throughout. Kernel copyout
MUST NOT write through a read-only COW alias or rely on a later user-mode page
fault to make pre-publication state valid.

If leaf cloning or parent downgrade itself fails, its internal rollback
restores every parent leaf it changed before releasing child records. Once a
complete child address space exists, a later pre-activation failure follows
Linux's safe lazy rollback: destroy the invisible child and its exact aliases;
the parent may remain a sole read-only anonymous COW mapping, whose next write
promotes in place after descriptor and TLB validation. Immediate writable
restoration is not required and must not be claimed. Both cases forbid a frame
leak or runnable child with an incomplete ledger. `clone_user_space` cannot
remain an immutable `&self` byte-copy helper for this transaction.

## 4. Present-write fault

The COW path is a distinct present-write classifier. It accepts only an exact
present, read-only, COW-tagged leaf backed by the live VMA and exact descriptor
mapping. An exception-time TLB mutation uses one nonblocking try; contention
returns `Retry`, restarts the instruction with interrupts restored, and lets
the in-flight shootdown finish.

The winner claims `Replacing(root, va)`, preserves the original user/NX/cache
rights, copies to a newly admitted frame, replaces the PTE, and acknowledges
the exact invalidation before retiring its old shared reference.  If it sees
that the frame has one remaining mapping, it may promote that exact COW leaf
to writable only after the same VMA/ledger revalidation and invalidation
protocol.  Concurrent writers, unmap, protect, exec, and exit either retry
against the new leaf identity or fail closed; none may consume the same
reference twice.

## 5. Protection and teardown

### Kernel copyout is a logical write

Every current or retained user-copy write admission (including prevalidation,
batch writes, and combined copyin/copyout) must resolve resident COW before
constructing the physical write proof. `kernel/ps/src/multitask/user_copy.rs`
binds the exact process-state lock to committed, private, writable VMA intent;
the PTE COW bit is never permission to override `PROT_NONE`, read-only intent,
or `MEM_RESERVE`. `kernel-mm` reuses the same descriptor/split/shootdown
transaction as exception writes, but targets the retained root explicitly,
never the executor's current CR3. Normal-context descriptor retries release
the TLB guard between attempts and have a one-second lost-owner bound.

After resolution, admission must retranslate and recheck the actual leaf.
Resolver success alone cannot authorize a copy or carry the old physical
address into a `ValidatedUserWrite`. The process-state lock remains held
through validation and copying, serializing fork, protection, unmap, and MM
generation replacement. Ordinary writable pages retain the single-walk fast
path; only readonly COW pages consult the VMA authority.

The initial direct-store KVM witness missed this class: a child store split
the page before any kernel write. The strengthened `fork_cow_private_write`
probe uses an additional untouched resident page as a `pread` destination,
checks the child's received bytes, and checks that the parent's bytes remain
unchanged. Before the repair it failed on iteration zero with child status
29184 (exit 114 = 100 + EFAULT). Host tests must reject reserved, readonly,
shared and non-COW object authority for both anonymous and image/file-private
backing; the runtime witness must not pre-write its copyout destination.

This follows Zircon's distinction between a user-copy failure and a true
permission failure: [VmObjectPaged::ReadUser at f4f1338](https://fuchsia.googlesource.com/fuchsia/+/f4f1338616d65c83f609467fda3d38d7ea0c3929/zircon/kernel/vm/vm_object_paged.cc)
resolves a captured write fault through the address space and retries the
copy. RustOS performs explicit retained-root admission because its copy uses
the direct map, which cannot itself trigger the user leaf's write fault.

### Exact teardown ordering

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

The registered `CowFrameLifecycle` model and source/mutation tests cover:

- fork success and each rollback point;
- serialized competing write-fault claims to one inherited leaf;
- write replacement, `munmap`, protection, rollback, and retirement ownership;
- child activation only after parent writable-TLB acknowledgement;
- exact frame reuse only after the last mapping's acknowledgement; and
- denial for every excluded mapping class.

The model distinguishes parent and child roots, PTE identity, shared-frame
reference count, leaf-ledger membership, and TLB generation.  A count-only
model is insufficient: it cannot detect a duplicate owner or a stale leaf
reusing a physical frame.

Runtime acceptance is `ipcbench: result name=fork_cow_private_write`: the
parent and child both write distinct values to one resident inherited page,
and the child reports its before/after values through a separate memfd. A skip,
missing result, nonzero child status, or mismatched value fails the capability.

On 2026-09-05 the isolated one-vCPU acceptance produced 64 non-skipped
iterations at `tsc_khz=3991237`, with min/p50/p99 20.01/42.86/65.09 ms and the
independent `vmexit_cpuid` hardware anchor. The same debugging run established
the regression rule: a publication failure must retain its exact internal
class and address. The packed `Malformed` report identified an initial/fork
`PROT_NONE` validator mismatch that a generic `ENOMEM` could not.

## 7. References

Linux also treats COW mapping eligibility separately from sharedness and
writeability, and implements fork COW by write-protecting mappings so a later
write fault installs a copied page.  The architecture here follows that
semantic boundary, while retaining RustOS's fixed-resource and exact-TLB-ack
requirements.

- <https://docs.kernel.org/core-api/mm-api.html>
- <https://www.kernel.org/doc/gorman/pdf/understand.pdf>
- <https://fuchsia.googlesource.com/fuchsia/+/3bed753925d767cf624088dee2cb421080f439be/zircon/kernel/vm/vm_object_paged.cc>
- <https://learn.microsoft.com/en-us/windows/win32/memory/memory-protection>
