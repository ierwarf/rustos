# Evolution rules

Rules for adding to or changing RustOS interfaces without breaking the
architecture. They record decisions already made in this repository; where a
rule has a mechanical check it names it. A change that cannot follow a rule
states the exception in its commit and in the owning contract.

## 1. Kernel scope

- Ring0 holds mechanism only: address spaces, threads, IPC transport,
  capabilities, interrupts, accounting. Policy lives in Ring3 named services
  (`rootd`, `syscalld`, `vfsd`, `loaderd`, `netd`, `inputd`, `pagerd`, ...).
- Minimality test: if a competing Ring3 implementation of the operation is
  possible (a different pager, a different VFS policy, a different scheduler
  class selector), it does not belong in the kernel. Move it out and leave the
  kernel an authenticated mechanism the service calls.
- No kernel operation is unbounded. Work proportional to caller-controlled
  input (path walks, directory scans, image parsing, copy loops, retries) has a
  fixed ceiling or runs in a Ring3 server. Waits are finite and name their
  deadline owner (see the IPC deadline rules in `docs/ai/abi/core.md`).

## 2. Every kernel allocation is charged to a Job

- A kernel resource a process can cause to exist is charged to its Job through
  the `JOB_KMEM_CLASS_*` classes in `libs/rustos-user-abi/src/syscall.rs`
  (handle table, IPC endpoint, page table, thread, VMA, IPC message, device
  buffer). `SYS_RUSTOS_JOB_KMEM` inspects and limits them; the model is in
  `docs/architecture/job-model.md`.
- A new resource kind adds a class, a limit path, a refusal that reaches the
  caller as an error, and release on teardown. The `DeviceBuffer` charge is
  being added in a separate change; until it lands, treat that class as
  declared but not yet enforced.
- An allocation with no owner Job (boot-time, per-CPU) is bounded by
  construction and says so where it is allocated.

## 3. One set of identifiers per family

- Each family (native syscalls, handle kinds, driver-domain operations, service
  ids, IPC op codes, milestone names) has one source: Rust constants in
  `libs/rustos-user-abi` or the owning protocol crate. Docs, other languages and
  the KVM harness repeat a value only as a checked copy.
- Hand-assigned numbers are written once as a literal at the declaration.
  Aliases reference the constant (`apps/wayclick` imports the syscall numbers
  from `rustos-user-abi`; `kernel_handle_kind_code` returns
  `handle_kind::HANDLE_KIND_*`).
- Enforced by `cargo xtask check` (`tools/xtask/src/abi_drift.rs`):
  syscall numbers unique, in `0x5255_xxxx` and never retired; ABI docs quote
  only registered numbers and `SYS_RUSTOS_X (0x...)` carries X's current
  number; every `*_MARKER` the KVM harness waits for exists as a literal in
  `kernel/ services/ libs/ apps/ driver-domains/`.
  Also: `rustos_syscall_numbers_are_unique` and the `handle_kind` tests
  (`cargo test -p rustos-user-abi`), the DVM operation/version tests
  (`cargo test -p driver-domain-protocol`), and
  `formal/check-native-syscall-numbers.py`.
- `docs/ai/session-handoff.md` is dated history and is not checked.

## 4. ABI changes are additive

- Default to additive: a new syscall number, a new op, a new trailing field
  behind an explicit size/version, a new flag bit that old callers leave zero.
- Bump a version only for a semantic break. The version constant
  (`DVM_*_VERSION`, `*_ABI_VERSION`) changes with the contract text and every
  producer and consumer in the same change.
- Never mutate a published number or wire layout in place. To swap, retire the
  old identifier and add a new one. Retired numbers stay reserved forever and
  keep returning `ENOSYS`: `RETIRED_NATIVE_SYSCALLS` and
  `RETIRED_HANDLE_KIND_CODES` in `libs/rustos-user-abi`, and the retired
  service ids listed in `docs/ai/abi/core.md`.
- Deprecate before removing: mark the old interface deprecated with a note
  naming its replacement, keep it working for at least one release of the
  contract, then retire it.
- Old-version paths get the same review as new ones. A compatibility decoder
  validates lengths, bounds and authority exactly as the current one does; a
  legacy path is not an unchecked back door.

## 5. Abstractions need a second user

- Do not add a generic framework for one caller. Keep the specific mechanism,
  write down what is shared, and generalize when a second real user exists.
  The custody/revoke subsystems are the worked example: they stay separate
  because their wait conditions differ; see `docs/ai/lease-patterns.md`.

## 6. Every new interface ships with proof

- A formal contract row (`formal/contracts.toml`, checked by
  `cargo xtask formal-contracts check`) naming the invariant and its witness.
- A KVM probe: a milestone or probe line that the harness in
  `tools/xtask/src/kvm/` waits for, so the interface is exercised on a booted
  system and a rename of its marker fails `cargo xtask check` rather than
  timing out.
- An owner contract update in Markdown when behavior, ownership or an
  invariant changes (`docs/ai/abi/*.md`).

## 7. Native ABI revision

A native binary is any packaged executable whose crate depends directly on
`rustos-user-abi` or `rustos-svc-runtime`, i.e. one that can use the native
`0x5255_xxxx` syscall/IPC surface: the static-PIE boot services (`rootd`,
`syscalld`, `vfsd`, `loaderd`, `procd`, `pagerd`) and the dynamic Rust-std
services and apps (`devmgrd`, `initd`, `inputd`, `netd`, `runtimed`,
`storaged`, `uiserver`, `ipcbench`, `wayclick`). It carries the revision of the
native ABI it was built against as an ELF note, stamped at build time by one
`rustos_user_abi::native_abi_note!();` expansion in the executable's root
(`rustos_svc_runtime::entry!` expands it for static-PIE services). The format
and constants are in `docs/ai/abi/core.md`; the single source is
`libs/rustos-user-abi/src/abi_revision.rs`. C demos, `apps/shell`, `sessiond`
and prebuilt apps do not link the ABI crates and are ordinary Linux ELFs,
correctly unstamped. A new binary that depends on an ABI crate is covered
automatically; the only exception path is the reasoned `NOT_STAMPED` list in
`tools/xtask/src/native_abi.rs`.

- One monotonic integer, `NATIVE_ABI_REVISION_CURRENT`. Fuchsia RFC-0002 uses a
  two-axis, partly random level scheme to allow independent teams to land
  levels without coordination; RustOS has one tree, one build and one
  maintainer set, so a plain counter is enough and has no ordering ambiguity.
- Compatible changes do not bump it: a new syscall number, op, trailing field
  behind a size, or flag bit (section 4). Bump it only for a semantic break
  that an old binary would misinterpret: a changed meaning of an existing
  syscall, handle kind, wire layout or startup contract (auxv, bootstrap
  block) that cannot be made additive.
- Lifecycle: introduce the new revision and bump `CURRENT` together with the
  contract text; keep the old revision in `NATIVE_ABI_SUPPORTED_REVISIONS` only
  while a legacy shim exists (in `loaderd`/`rootd` policy, with the same
  validation as the current path, section 4); deprecate it in the owning
  contract; then remove the shim and the list entry. A binary of a removed
  revision is refused by `loaderd` with `ENOEXEC`.
- The stamp is a self-declared compatibility hint, not an authority claim. It
  does not select a personality, is not read by `personality_gate` or `rootd`,
  and grants nothing. An absent stamp means "ordinary ELF", unchanged.
- Enforcement: `loaderd` refuses an unsupported or malformed stamp on normal
  spawns. The kernel does not parse notes, so boot services spawned by the
  kernel bootstrap allowlist are verified by `cargo xtask build`/`stage`
  (every staged native ELF must carry the current stamp) and the source rule in
  `cargo xtask check` (`tools/xtask/src/native_abi.rs`).
- The kernel has no per-process revision field. When the first kernel behavior
  depends on the revision, record it additively (a new field behind an
  explicit size, set from loader-supplied launch metadata, not from the note
  inside the process), and update this section.

## Checklist for a new syscall or handle kind

1. Pick the next free number in the Rust table; never reuse a retired one.
2. Add the constant, the args struct with an explicit version and size, and
   a Job charge if it allocates kernel memory.
3. Add the dispatch arm in a Ring0 handler that only does mechanism.
4. Update the owning `docs/ai/abi/*.md` section, quoting the number once.
5. Add the formal row and the KVM probe marker.
6. Run `cargo xtask check` and `cargo test -p rustos-user-abi`.
