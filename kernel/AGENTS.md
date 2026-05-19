# Kernel Subtree Notes

Inherits the repo root `AGENTS.md`. Add-only overrides below.

## Truth Sources

- Linux user-ABI surface: `kernel/ps/src/user/linux.rs` is the single truth.
  The old `kernel/compat/src/user/linux.rs` was consumed into service-oriented
  syscall routing and removed. Do not re-create it; route new policy through
  `syscalld` instead.
- Kernel API boundaries: `kernel/*/src/api.rs` files are the only public
  surface other crates should call. Prefer extending these over reaching into
  internals.

## Microkernel Offload Pattern

Offload syscalls follow: ring0 entry → narrow privileged broker → user
service (`syscalld`, `vfsd`, `loaderd`, `netd`, `devmgrd`, `driverd`,
`storaged`, `inputd`). When extending:

- Keep ring0 to **primitive** capability gates. No policy.
- New brokers must be explicit (named `SYS_RUSTOS_*_BROKER`) and bounded.
- PTE mutation and backing lifetime go through `SYS_RUSTOS_MM_BROKER`. Do
  not duplicate the MM policy in ring0.

## Fast-Path Rule

When the broker is a **no-op forwarder** (just copies a request into a
shared region and signals the service), skip the broker IPC and cache the
shared-region kernel pointer at the call site. The IPC round-trip is the
single biggest source of stall in the syscall hot path; only keep it where
the broker actually arbitrates.

## Validation During Refactor

Per root `AGENTS.md`, compile/QEMU validation may be intentionally deferred
when the task is structural removal. Do not paper over a half-removed
module with a fake success path — leave the build broken and flag it.
