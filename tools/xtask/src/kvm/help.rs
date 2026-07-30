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
  --gui-dvm-surfaces   enable the V3 GUI-DVM control/pixel backing and private
                       three-slot GPU atlas transport; no standalone legacy
                       surface renderer or native-GPU path is accepted
  --dvm-network-shmem  attach the bounded RustOS↔DVM Ethernet ring; RustOS keeps
                       no native virtio-net device in this topology
  --dvm-block-shmem    attach a private virtual NVMe namespace only to Linux
                       DVM and the fixed RustOS↔DVM block ring; RustOS receives
                       no native storage controller
  --storage-dvm-only   run the independent storage-DVM acceptance gate: enable
                       --dvm-block-shmem and require boot, both block peers,
                       first completion, and E2E flush without depending on UI,
                       input, network, or GPU readiness markers
  --storage-dvm-expect-flush-fault
                       with --storage-dvm-only and exactly block.flush=fail,
                       require a pre-publication DeviceFault and reject any E2E
                       flush-success marker
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
