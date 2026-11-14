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
                       (1..={MAX_SMOKE_TIMEOUT}, default {DEFAULT_SMOKE_TIMEOUT})
  --expect <marker>    require an additional RustOS debugcon marker (repeatable)
  --expect-dvm <marker>
                       require an additional Linux-DVM serial marker (repeatable)
  --exercise-input     run the DVM's bounded evdev loopback self-test and require
                       RustOS inputd keyboard and pointer ingress markers
  --exercise-network   run netprobe through netd and the DVM Ethernet ring;
                       requires --gui-dvm-surfaces and --dvm-network-shmem
  --exercise-google-ping
                       opt in to external DNS resolution and a verified Google
                       ICMP Echo reply; requires --exercise-network (not offline)
  --ipcbench-probe <name>
                       restrict the `ipcbench` session-startup harness to one
                       named probe, so the in-kernel `ipc-call-phase-*` and
                       `usermem-phase-*` counters it charges belong to that
                       probe alone for the whole boot
  --oom-hog            arm the `oomhog` session program, which touches memory until
                       the allocator refuses it, and require the root supervisor
                       to terminate it (`rootd: oom killed`) while the desktop,
                       driver-domain relay, and every service keep running
  --oom-hog-job-limit  require four framed hog page faults at the 128 MiB job
                       quota, physical headroom, and a zero-charge retired job;
                       build first with RUSTOS_LOADERD_LIMIT_PROBE=1 and
                       RUSTOS_ROOTD_APP_LIMIT_PROBE=1 (both off by default)
  --job-policy-probe  require hierarchical JobPolicy ABI and framed Current-cache
                       deny/restore proof; build with RUSTOS_ROOTD_POLICY_PROBE=1
                       (off by default), separate from memory/process stress
  --proc-stress        arm the `procstress` session program (fork, copy-on-write,
                       mmap/mprotect/munmap churn from many threads) and wait for
                       it to finish; used to reproduce scheduler and TLB panics
  --local-fd-probe     arm the `localfdprobe` session program: kernel-local pipe/pipe2
                       and eventfd/eventfd2 semantics (EOF, EPIPE/SIGPIPE, atomic and
                       partial writes, blocking wakeups, dup/fork/exec, poll/epoll);
                       fails fast on `localfd: FAIL`, requires `localfd: PASS`
                       (implies --gui-dvm-surfaces)
  --library-map-probe  arm the `libmapprobe` session program: two processes map a
                       pagerd-warmed library, share its physical frames, isolate a
                       private write, and release on exit; needs the storage-backed
                       graphical session (implies --gui-dvm-surfaces)
  --library-reclaim-probe
                       with plain --oom-hog: the probe holds a live alias mapping
                       while pagerd reclaims library objects under pressure
                       (`pagerd: library cache reclaimed`); the mapping must keep
                       its bytes and frames (`library-map-probe: reclaim-survived`)
  --mmap-cost-probe    measure (not change) the current cost of mapping a large
                       non-warmed file MAP_PRIVATE: writes deterministic files
                       (default 4,16,64,128 MiB, `--mmap-cost-sizes-mib A,B,..`)
                       onto the per-run boot volume and arms `libmapprobe`
                       `mode=mmapcost`; prints a table and `mmap-cost-probe:`
                       lines (mmap/sparse/full touch, fork, free pages; implies
                       --gui-dvm-surfaces)
  --app-dmabuf-probe   boot with the app DMA-BUF aperture and arm `apps/wayclick` to
                       allocate an app DMA-BUF through devmgrd and the DVM exporter,
                       present it through zwp_linux_dmabuf_v1, prove a forged fd is
                       refused, and require export/import/release/revoke/scrub
                       markers in order (implies --gui-dvm-surfaces)
  --release-stage-probe
                       put a signed release bundle on the volume and require
                       initd -> loaderd -> rootd to verify and stage it; build
                       first with RUSTOS_INITD_RELEASE_PROBE=1 and
                       RUSTOS_LOADERD_TEST_RELEASE_KEY set to the
                       public test key printed by `cargo xtask kvm-smoke
                       --print-release-probe-key` (never ship such an image)
  --oom-hog-watchdog   like --oom-hog, but require the ring0 watchdog to end the
                       hog while rootd is blinded; run as
                       RUSTOS_FAULTS='rootd.memory.census=fail'
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
  --host-cpu-pressure <percent>
                       timing profile for readiness claims: keep this percent
                       (1..=100, rounded up to whole cores) of the host cores
                       the run may use 100% busy, one pinned worker per core,
                       for the whole run including every --repeat boot. The
                       default is the idle-host profile. Not combinable with
                       --smp-iteration, whose sealed evidence does not record
                       the profile; ignored by --dry-run
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
  --physical-usb <BDF> non-commercial lab mode: attach one already-bound xHCI
                       controller (sole IOMMU-group member, bound to vfio-pci)
                       to the DVM through IOMMUFD and omit the virtual DVM
                       keyboard and pointer, so the DVM agent relays a physical
                       USB keyboard and pointer. It has no sealed profile and
                       never binds, unbinds, or resets the controller
  --gpu-firmware <path>
                       profile-specific owner-private firmware table. The
                       currently certified AMD profile requires a relocated
                       VFCT produced by rustos-hostd prepare-amd-vfct
  --repeat <count>     boot this exact topology 1..=64 times and report the
                       pass rate, naming each failed run's panic line and
                       archived debugcon log. A defect that appears in one boot
                       of six is a rate, and a single run cannot measure it
  --no-build           boot the image already on disk instead of refreshing it.
  --no-auto-verify     skip the unsealed-formal-profile check of a multi-vCPU
                       boot (interactive runs only warn; --smp-iteration and
                       --smp-ring3-qualification seal or refuse otherwise).
                       This lane rebuilds by default: a stale image silently
                       answers questions about code that is no longer in the tree
  --dry-run            validate inputs and prepare KVM log paths without launching QEMU
  -h, --help           show this help

Option values cannot start with --; prefix such paths with ./.

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
