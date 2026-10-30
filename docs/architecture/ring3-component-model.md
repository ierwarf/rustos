# Ring3 component model integration boundary

**Status: policy-engine foundation, not an 8-stage implementation.**

The `rustos-component-model` library is a new, userspace-only, allocation-
bounded state machine with host-testable invariants for:

- generational component names and exact incarnation identity;
- hierarchical qualitative policy ceilings (never positive kernel authority);
- fail-closed component manifest preparation;
- explicit, scoped capability routes and restart revocation;
- checked hierarchical resource budgets and single-use charge tickets;
- restart windows, exponential backoff and terminal failure decisions;
- bounded in-memory health and crash events;
- strict length/version-checked IDL envelope staging.

It deliberately performs no syscall, touches no kernel/compat code and does
not change rootd's existing bootstrap/service graph. The current kernel Job
tree remains the authoritative resource ledger, and only kernel-validated
Job descriptors can create actual process/resource authority.

## Integration work required

1. Bind manifests to **signed** rootd image/registry state; reject untrusted
   manifest-supplied Job IDs. The policy graph is not signature verification.
2. Replace the historical fixed-ID discovery routing only after dual-running
   source/ABI witnesses and proving no unauthorized endpoint transfer.
3. Translate component lifecycle decisions to existing exact-generation
   rootd service leases, handle revocation and loader transactions.
4. Connect session charge tickets to real kernel-backed Job accounting,
   including kernel-byte budget and memory high/min behavior.
5. Generate the transport bindings from IDL; the current `wire` module is
   envelope validation only and must not replace production ABI structs yet.
6. Add persistent crash storage/health escalation and tested update rollback.
7. Preserve ring0 hot paths: futex wait/wake, scheduler and thread substrate,
   clock_gettime/sleep, getpid/gettid, usercopy, fault and page-table commit.
   Never add a policy IPC round trip to these paths as a migration shortcut.

**Unimplemented stages are not marked complete.** See
[Ring3 roadmap](ring3-process-model.md) and
[Job model](job-model.md) for the existing work and remaining stages.
Neither TLB shootdown nor ring0 compatibility code is modified here.

## Verification

`cargo test -p rustos-component-model --locked` exercises the model
separately from live rootd. This does not prove reboot, SMP or ABI correctness.
The existing `cargo xtask check`, formal requirements and KVM smoke remain
mandatory once production wiring changes.
