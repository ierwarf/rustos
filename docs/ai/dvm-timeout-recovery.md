# DVM timeout recovery

## Block

The Linux block relay forks an independent supervisor before accepting I/O. Its
worker sends begin/end records around each synchronous request and completion
publication. The worker submits completion data over a process-shared channel;
only the independent supervisor publishes completion cursors. Timeout and
publication are serialized by the supervisor, so a stopped worker cannot hold a
publication lock or publish after quarantine. After 10 seconds without an end,
or if the worker/control channel is lost, the supervisor marks the shared worker gate quarantined, withdraws
DVM_READY, rings the host, signals the worker, and retains the relay lock and
mappings. It emits `rustos-dvm-block: quarantine` with generation, request,
operation, cause, unknown outcome, and unproven quiescence. Completion rings
are notified per request. A signal cannot cancel a Linux task in D state;
neither the signal nor the watchdog record proves the device has stopped.

A signed successor is authenticated by the kernel; that signature alone does
not establish quiescence. The issuer must not sign a physical successor until
independent quiescence has been established. `storaged` also quarantines its
own admissions after a completion timeout or uncertain submitted write, clears
its read cache, and never replays an unknown write. Its local state does not
survive restart and does not drain previously admitted operations.

Hostd's physical-device authority reserves the IOMMU group and stable PCI BDFs
in `/var/lib/rustos-hostd` before a handoff. A root operator must run
`rustos-hostd provision-authority` once on a trusted, canonical ext-family,
XFS, or Btrfs filesystem. Routine commands never provision an absent authority;
missing, corrupt, unsafe, tmpfs, ramfs, overlay, and unrecognized filesystems
fail closed. Explicit provisioning is a trusted operator action: total loss of
all durable state cannot be distinguished from first provisioning without an
external or TPM anchor. Do not reprovision a lost authority while any previous
physical ownership may remain unresolved.

The durable reservation precedes a prepared lease and a durable mutation intent;
the lease precedes signing and aperture initialization. A durable prelaunch
intent is written before the lease becomes active. The QEMU wrapper cannot
execute the guest until its PID/start identity is durable and its gate receives
a release byte; EOF fails closed. A failed or interrupted prelaunch is recovered
with the same exclusive group claim and reset, not by assuming a missing runtime
is safe. A reservation whose
mutation intent was never published may be discarded only with matching plan,
group and BDFs; an existing prepared lease additionally requires its exact
original drivers and overrides. Mutating prepared leases require the same reset
proof as active leases. Raw `release --activate` is forbidden; `recover` is the
only restoration path for a failed physical lease.

For an active lease, recovery requires the matching durable runtime identity.
On the same boot it verifies the recorded process identity and uses pidfd to
establish exit; after reboot the old process is absent, but boot identity by
itself is never quiescence proof. Every recovery attempt, including replay of a
recorded reset phase, validates the whole-group BDFs and reset scope, obtains a
fresh exclusive VFIO group claim, disables bus mastering and resets every
function. A recorded reset is progress, not a durable ownership claim. With the
claim still held, hostd installs and verifies the PCI `none` driver override on
every function, withdraws and verifies every VFIO binding, and only then closes
the claim and restores original drivers. An existing PCI driver named `none`
is refused. This transition assumes trusted privileged host administration does
not concurrently alter PCI bindings or overrides.

The Linux [VFIO documentation](https://docs.kernel.org/driver-api/vfio.html)
describes the group as the ownership unit; Linux v6.12
[`container.c`](https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/drivers/vfio/container.c?h=v6.12)
claims IOMMU DMA ownership on legacy container attachment, and
[`group.c`](https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/drivers/vfio/group.c?h=v6.12)
detaches the container when the last VFIO device is removed. Linux's
[PCI sysfs ABI](https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/Documentation/ABI/testing/sysfs-bus-pci?h=v6.12)
documents `none` as an override that opts out of driver binding. Device reset
and actual withdrawal behavior still require validation on the deployed kernel
and hardware; the native mocks do not perform a physical reset.

A recovery-complete tombstone precedes runtime and lease deletion and is removed
last. If a crash or error interrupts VFIO withdrawal or host driver restoration,
the group may become partially or fully non-VFIO-bound; hostd cannot reacquire
the same proof and retains quarantine for independent operator/provider
recovery. A changed group, missing/corrupt runtime identity, busy or unsupported
group claim, unavailable reset, or unverifiable aperture also fails closed.
Activation-record failure leaves the device VFIO-bound and quarantined rather
than restoring drivers outside this recovery protocol.

## GPU

The cold/steady submission budget starts before source acquisition and covers
upload and render together; present has a separate 100 ms phase cap, bounded by
the original operation deadline. Prime uses one deadline across its existing
pipeline phases. Performance measurements remain distinct from the hard
watchdog. A separate normal-priority watchdog stops the entire display process
after withdrawing peer readiness when a deadline expires even if Mesa/DRM is
blocked. A frame failure with potentially outstanding work is terminal and
parks the relay without destroying or reusing its GPU/KMS resources. Neither
parking nor SIGSTOP proves DMA quiescence. Hostd must not SIGCONT, restart, or
reassign the GPU until independent quiescence is established. Supervisor loss
must also be treated as terminal.

Recovery markers use `rustos-dvm-display: recovery v=1 generation=<GUI pool
invitation> attempt=<n> state=<starting|recovering|ready|failed> cause=<stage>
errno=<n> elapsed_ms=<n> frames=<n> sequence=<n>`. `ready` follows an actual
RustOS frame, the existing active marker and frame evidence. Before a nonzero
invitation, `bootstrap` replaces `recovery` and generation is zero; bootstrap
can never establish readiness. Retries before terminal GPU quarantine are
bounded by a cumulative 30-second recovery window. A new authoritative
invitation starts a new bound attempt series.

Deadline checks cannot cancel synchronous driver calls, and bounded watchdog
reaction depends on Linux scheduling and the offline notification path. No
physical hardware, DVM, KVM or formal validation is claimed.
