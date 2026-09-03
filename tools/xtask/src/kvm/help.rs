// SPDX-License-Identifier: MIT

pub(crate) fn print_kvm_smoke_help() {
    println!(
        "\
usage: cargo xtask kvm-smoke [options]

Boots the Linux DVM and RustOS concurrently with QEMU/KVM. This verifies the
host-authenticated Linux-DVM input relay endpoint and RustOS's dedicated
framed virtual input transport. It does not synthesize QMP input.

options:
  --timeout <seconds>  bound boot, readiness, and active proof collection
                       (1..={MAX_SMOKE_TIMEOUT}, default {MAX_SMOKE_TIMEOUT})
  --expect <marker>    require an additional RustOS debugcon marker (repeatable)
  --expect-dvm <marker>
                       require an additional Linux-DVM serial marker (repeatable)
  --exercise-input     run the DVM's bounded evdev loopback self-test and require
                       RustOS inputd keyboard and pointer ingress markers
  --exercise-network   run netprobe through netd and the DVM Ethernet ring;
                       requires --gui-dvm-surfaces and --dvm-network-shmem
  --ipcbench-probe <name>
                       restrict the `ipcbench` session-startup harness to one
                       named probe, so the in-kernel `ipc-call-phase-*` and
                       `usermem-phase-*` counters it charges belong to that
                       probe alone for the whole boot
  --min-ui-fps <fps>   enable the private KVM-only UI profiler and bounded DVM
                       uinput/evdev load, then require three high-volume input
                       windows, WayClick commit/frame-callback windows, and three
                       accepted DVM atomic-page-flip relay samples at or above
                       this integer FPS; virtual display proof uses QEMU GTK,
                       while --physical-gpu observes the physical KMS path
  --ui-proof-windows <count>
                       require 3..={MAX_UI_FPS_ACTIVE_WINDOWS} consecutive one-second UI/input
                       and DVM relay samples (default {DEFAULT_UI_FPS_ACTIVE_WINDOWS}); requires
                       --min-ui-fps and supports bounded active soak proofs
  --recovery-probe <dvm-restart|rustos-reboot|all>
                       after initial readiness, force an abrupt Linux-DVM exit
                       and/or restart RustOS in a fresh QEMU process;
                       require a new authenticated epoch and full readiness
  --smp-iteration      require framed, checksummed per-CPU scheduler lifecycle
                       evidence; restricted to a bounded 30-second KVM smoke
  --smp-ring3-qualification
                       with --smp-iteration, inject a private KVM-only Ring3
                       workload contract and require one pinned ready/start/
                       finish proof from every requested CPU (1, 2, 4, or 8);
                       requires --smp-evidence-cohort
  --smp-evidence-cohort <32-lowercase-hex>
                       bind one 1/2/4/8 Ring3 qualification matrix to an
                       explicit path-safe cohort; the runner archives unique
                       JSON, checksum, debugcon, DVM serial, and contract
                       artifacts without replacing an earlier run
  --gui-dvm-surfaces   enable the V3 GUI-DVM control/pixel backing and private
                       three-slot GPU atlas transport, plus the production DVM
                       block provider needed by the visible desktop; no
                       standalone legacy surface or native-GPU path is accepted
  --dvm-network-shmem  attach the bounded RustOS↔DVM Ethernet ring; RustOS keeps
                       no native virtio-net device in this topology
  --dvm-block-shmem    attach a private read-only ATAPI namespace to Linux DVM
                       through its existing q35 AHCI controller and the fixed
                       RustOS↔DVM block ring; RustOS receives no native storage
                       controller
  --storage-dvm-only   run the independent storage-DVM acceptance gate: enable
                       --dvm-block-shmem and require boot, both block peers,
                       first completion, and a read-only media barrier without
                       depending on UI, input, network, or GPU readiness markers
  --storage-dvm-expect-flush-fault
                       with --storage-dvm-only and exactly block.flush=fail,
                       require a pre-publication DeviceFault and reject any
                       media-barrier success marker
  --physical-gpu <BDF> non-commercial lab mode: attach one already-bound GPU
                       from the sealed physical-GPU profile registry through
                       IOMMUFD instead of virtio-GPU; never binds, unbinds, or
                       resets the device
  --gpu-firmware <path>
                       profile-specific owner-private firmware table. The
                       currently certified AMD profile requires a relocated
                       VFCT produced by rustos-hostd prepare-amd-vfct
  --physical-amdgpu <BDF>, --amd-vfct <path>
                       compatibility aliases for the current AMD profile
  --repeat <count>     boot this exact topology 1..=64 times and report the
                       pass rate, naming each failed run's panic line and
                       archived debugcon log. A defect that appears in one boot
                       of six is a rate, and a single run cannot measure it
  --no-build           boot the image already on disk instead of refreshing it.
  --no-auto-verify     refuse a multi-vCPU boot whose formal profile is unsealed
                       instead of sealing it first.
                       This lane rebuilds by default: a stale image silently
                       answers questions about code that is no longer in the tree
  --dry-run            validate inputs and prepare KVM log paths without launching QEMU
  -h, --help           show this help

The default proof requires RustOS to reach init handoff and the L0-style host
broker to complete an authenticated Linux-DVM health/inventory/input-stream
handshake. A real key requires a physical input source assigned to the DVM;
the default smoke command does not fabricate one. `--exercise-input` is an
explicit KVM-only self-test: its Linux agent writes a bounded uinput device,
then consumes it through its ordinary evdev relay. It never opens QMP or a
host-to-DVM input injection endpoint.
"
    );
}
