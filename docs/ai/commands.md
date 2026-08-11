# AI Commands

Run from repo root. Commands are expected to be quiet on success; treat
failure output as the primary debugging context.

## Build, stage, check

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask dev-plan` | classify all tracked and untracked changes into fast `now` checks and one-time `stable-batch` gates | none | non-UTF-8 path or unavailable Git worktree |
| `cargo xtask check` | validate layering/manifests/workspace | `target/` | dependency layer violation, bad manifest, missing target |
| `cargo xtask check --timings` | run the same check and print deterministic phase timings | `target/` | same as `check`; the slow phase identifies the next optimization target |
| `cargo xtask build` | full OS build + stage | `target/`, `build/` | compile error, missing firmware/artifact, manifest staging error |
| `cargo xtask build --timings` | run the same build and print phase timings | `target/`, `build/` | same as `build`; the slow phase identifies the next optimization target |
| `cargo xtask build-user` | userspace packages only | `target/`, `build/artifacts` | service/app compile error |
| `cargo xtask stage` | restage built artifacts | `build/image` | missing required artifact, bad install path |
| `cargo xtask clean` | remove generated host/build/runtime outputs | removes `target/`, `build/`, `logs/` | stale generated artifact cleanup |

## Run and debug

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask build-dvm` | build the pinned Linux DVM, cryptographically verify every installed module against its generated X.509 certificate, and emit a self-contained schema-9 bundle | `driver-domains/linux/out/` | missing Buildroot prerequisite, unsigned/foreign module, or source/artifact mismatch |
| `make -C driver-domains/linux build-plan` | read-only classification of the next DVM build as cold/full or explicit incremental lanes | temporary config probe only | stale Buildroot/toolchain identity, unsafe config transition, or the listed kernel/package/rootfs lanes |
| `make -C driver-domains/linux selftest-config-cache` | prove the config admission policy and that a kernel-fragment mutation changes the kernel lane without changing the host-toolchain lane | temporary files only | cache boundary regression |
| `make -C driver-domains/linux ccache-stats` | report Buildroot's persistent compiler-cache hits and misses | none after the cached ccache tool exists | missing/incomplete Buildroot host tools |
| `make -C driver-domains/linux profile-build` | generate Buildroot's package/step duration graphs after a completed build | `out/buildroot-output/graphs/` | incomplete timing data or missing matplotlib/numpy |
| `cargo xtask verify-dvm` | verify every co-located DVM artifact, kernel signature-enforcement configuration, certificate, source lock, and control contract | none | altered/missing DVM artifact, signing policy, source input, or contract |
| `make -C driver-domains/linux verify` | recheck Buildroot/kernel configuration and every installed module's detached PKCS#7 signature without rebuilding the DVM | temporary files under `/tmp` only | unsigned, malformed, or foreign-signed module; stale build tree |
| `make -C driver-domains/linux stage-release DEST=/trusted/new/path` | verify, copy, reverify, and atomically publish the eight-file DVM bundle to a fresh owner-controlled path | the new destination only | existing destination, symlink/mutable ancestor, or artifact mutation |
| `make -C driver-domains/linux rebuild-agent` | rebuild only the DVM control/input agent while preserving the Buildroot host toolchain | DVM package/artifacts only | agent compile or artifact refresh failure |
| `make -C driver-domains/linux rebuild-display` | rebuild only the DVM display relay while preserving the Buildroot host toolchain | DVM package/artifacts only | display relay compile or artifact refresh failure |
| `make -C driver-domains/linux rebuild-net` | rebuild only the DVM network relay while preserving the Buildroot host toolchain | DVM package/artifacts only | network relay compile or artifact refresh failure |
| `make -C driver-domains/linux dev-agent` | compile only the cached DVM control/input package; no rootfs or artifact is created | `out/buildroot-output/target/` only | cold/stale configuration; run `build` first |
| `make -C driver-domains/linux dev-display` | compile only the cached DVM display package; no rootfs or artifact is created | `out/buildroot-output/target/` only | cold/stale configuration; run `build` first |
| `make -C driver-domains/linux dev-net` | compile only the cached DVM network package; no rootfs or artifact is created | `out/buildroot-output/target/` only | cold/stale configuration; run `build` first |
| `cargo xtask kvm-smoke` | concurrently boot Linux DVM and RustOS with QEMU/KVM | `build/kvm/` | unavailable `/dev/kvm`, guest exit, missing readiness marker |
| `cargo xtask kvm-smoke --timeout 30 --gui-dvm-surfaces --dvm-network-shmem --dvm-block-shmem --recovery-probe all` | after positive readiness, abruptly terminate and relaunch the Linux DVM and then reboot RustOS in a fresh QEMU process; require fresh authenticated control, display, storage, and service epochs rather than old log markers | `build/kvm/` and private DVM apertures | stale evidence, peer-ID drift, missing revoke/rebind, failed authenticated relay, missing fresh boot/service markers, guest exit, or deadline |
| `cargo xtask kvm-smoke --timeout 30 --storage-dvm-only` | independently prove the virtual storage-DVM topology: authenticated peer readiness, exact signed geometry, first completion, and a generation-bound read-only media barrier without accepting unrelated UI/GPU markers; this is a transport-liveness check, not backing-image write-durability evidence | `build/kvm/` and private DVM block disk/aperture | missing block peer, malformed geometry/signature, absent completion/media barrier, guest exit, or deadline |
| `RUSTOS_FAULTS='block.flush=fail' cargo xtask kvm-smoke --timeout 30 --storage-dvm-only --storage-dvm-expect-flush-fault` | prove the same storage-DVM topology independently observes a live generation-bound completion and then reports media-barrier `DeviceFault` without emitting the success marker | `build/kvm/` and private DVM block disk/aperture | absent/competing fault rule, missing peer/geometry/completion/fault marker, impossible media-barrier success marker, guest exit, or deadline |
| `cargo xtask kvm-smoke --timeout 30 --gui-dvm-surfaces --physical-gpu <BDF> --gpu-firmware <TABLE>` | explicitly non-commercial physical-GPU lab run through the sealed device-profile registry; the current registered profile is AMD `1002:1900` with a relocated VFCT. QEMU 11.0 or newer uses IOMMUFD and VFIO PCI-BAR DMA-BUF mapping, executes the real `uiserver` GPU scene, and scans it out on the physical connector. Because this lane disables reset, an atomic boot-ID claim permits exactly one launch attempt per host boot. The runner never binds, unbinds, or resets the device and attaches no network device. `--physical-amdgpu`/`--amd-vfct` remain compatibility aliases | `build/kvm/` plus the physical display | repeated launch in one boot, unknown/ambiguous profile, unsafe VFIO/IOMMUFD/profile firmware state, inaccessible per-device cdev, unavailable VFIO BAR DMA-BUF support, inherited memlock below 4 GiB, reset-dirty driver probe, missing end-to-end GPU completion, or guest exit; never counts as supervised reset/revoke evidence |
| `tools/prepare-physical-amdgpu-vfio-lab.sh [--check] [AMD_VFCT]` | prepare only GA403UM AMD `1002:1900` for the non-commercial physical-QEMU lab lane: require a pre-unbound or already-correct function, singleton IOMMU group, disabled reset and idle-D3, cleared bus mastering, limited cdev ACLs, inherited memlock, IOMMUFD probe, and physical dry-run; never unbinds, resets, starts QEMU, or admits another VFIO function | AMD `0000:65:00.0` VFIO binding and transient sysfs/ACL/rlimit state; `build/kvm/` dry-run inputs | wrong hardware, active host driver, another VFIO function, unsafe reset/DMA state, missing access, invalid VFCT, or failed dry-run |
| `tools/configure-amdgpu-vfio-early-bind.sh [--apply]` | plan by default; with `--apply`, install the exact GA403UM `1002:1900` vfio-pci ID/idle-D3 policy, amdgpu blacklist, and initramfs module entry, then update initramfs without touching the live driver | `/etc/modprobe.d/rustos-amd-vfio.conf`, `/etc/initramfs-tools/modules`, initramfs | wrong/multiple AMD displays, conflicting policy, modified owned file, duplicate module entry, or initramfs failure |
| `tools/remove-amdgpu-vfio-early-bind.sh [--apply]` | plan by default; with `--apply`, remove only the exact RustOS policy and exact initramfs module entry, update initramfs, and leave the live device untouched so amdgpu may bind on the next cold boot | same persistent files and initramfs | modified/foreign policy, duplicate entry, separate GRUB override, or initramfs failure |
| `cargo xtask kvm-run` | start the interactive Linux-DVM display session from the existing signed RustOS image; require a kernel-timestamped WayClick first frame before the operator closes QEMU, then record real pointer ingress and healthy idle UI ticks | `build/kvm/` including a bounded `failure-summary.json` on startup/stall failure | stale RustOS image, unavailable GUI backend, `/dev/kvm`, missing acceptance evidence when the window closes, or a guest exit |
| `cargo xtask kvm-run --build --rustos-vcpus 8` | build/sign the RustOS image and then enter the exact same verified cached-DVM interactive path at the supported maximum eight-vCPU SMP topology; this is the sole VS Code F5 command and uses the same source-bound SMP and outer-session readiness oracles; a multicore launch whose `smp-iteration` profile is unsealed for the current tree runs `bash formal/verify-smp-iteration.sh` first instead of failing, then revalidates | signed RustOS image plus `build/kvm/` and `build/formal/verification-run/smp-iteration.json` | build/sign failure, a failed or still-unsealed automatic verification, invalid cached DVM, a real guest/stall failure, or missing acceptance evidence when the window closes |
| `cargo xtask kvm-run --build --rustos-vcpus 8 --no-auto-verify` | the same interactive launch, but refuse an unsealed `smp-iteration` profile instead of sealing it | signed RustOS image plus `build/kvm/` | stale/missing SMP evidence, plus every failure of the row above |
| `cargo run -p rustos-hostd -- discover` | read host IOMMU groups | none | IOMMU unavailable or unreadable sysfs |
| `cargo run -p rustos-hostd -- preflight --plan <file>` | require complete, non-protected IOMMU-group ownership and reject live `boot_vga`/connected DRM displays | none | incomplete group, declared host-critical BDF, or active L0 display |
| `cargo run -p rustos-hostd -- preflight-physical --plan <file> --dvm-artifact-manifest <file> --device-policy <file> --qemu <file>` | before any VFIO bind, validate topology, live display, lease-contained reset scope, DMA-safe VFIO bind configuration, at least 4 GiB soft memlock, exact policy/QEMU/bundle, exact checksummed AMD VFCT/ATOM VBIOS, and an empty IOMMUFD IOAS allocate/destroy probe | none | unsafe/mismatched runtime input, reset scope escaping the lease, insufficient pinning budget, idle-D3 DMA window, missing/mismatched VBIOS, or unusable IOMMUFD ABI |
| `cargo run -p rustos-hostd -- extract-amd-vbios --vfct <VFCT> --bdf <BDF> --output <ROM>` | extract one exact AMD APU VBIOS from a read-only VFCT snapshot into a new owner-private file for focused diagnosis; the subsystem pair must be exact or both VFCT fields must be zero | new 0600 ROM snapshot | bad ACPI checksum/bounds, wrong identity, partial/mismatched subsystem, duplicate image, invalid 0x55aa/ATOM header, symlink/mutable source, or existing output |
| `cargo run -p rustos-hostd -- prepare-amd-vfct --vfct <VFCT> --bdf <HOST-BDF> --output <TABLE>` | validate the host identity, relocate only its VFCT image BDF to fixed guest slot `0000:00:08.0`, recompute the ACPI checksum, and preserve the VBIOS bytes | new 0600 relocated VFCT table | any source validation failure, changed payload, invalid relocated checksum/identity, unsafe path, or existing output |
| `sudo target/debug/rustos-hostd probe-iommufd` | exercise one empty IOMMUFD IOAS allocate/destroy round trip without binding or opening a VFIO device | none | missing administrator access or incompatible IOMMUFD userspace ABI |
| `cargo run -p rustos-hostd -- supervise ...` | launch one signed display-only physical-device DVM with IOMMUFD, an exact private relocated AMD VFCT table supplied through ACPI at fixed guest BDF, authenticated readiness, private QMP/ACPI shutdown with actual-exit proof, bounded forced fallback, reset, and restore | private runtime record/VFCT/QMP endpoint and supervised QEMU | stale authorization, artifact/policy/QEMU/VFCT mismatch, absent IOMMUFD/reset, failed authentication, rejected/timed-out ACPI shutdown, signaled/nonzero QEMU exit, or quarantine |
| `cargo run -p rustos-hostd -- verify-artifacts --dvm-artifact-manifest <release/rustos-linux-dvm-x86_64.manifest>` | independently admit one staged self-contained schema-9 DVM bundle | none | mutable path, missing/extra metadata, or companion-file hash mismatch |
| `cargo run -p rustos-hostd -- recover --plan <file>` | recover an active lease by canonical runtime record plus exact post-open PID/start-time identity, signal only through pidfd, then reset and restore the whole group | removes runtime/lease state only after success | unsafe/stale runtime identity, unavailable pidfd, or reset/restore failure |
| `cargo run -p rustos-hostd -- relay-input ...` | relay validated DVM Linux input into RustOS's fixed input ring | launch-owned ivshmem backing and doorbell | policy mismatch, malformed DVM event, or peer lifecycle failure |

## VS Code F5 contract

The single F5 configuration, `RustOS: verified KVM desktop`, executes only
`cargo xtask kvm-run --build --rustos-vcpus 8`. The fixed eight-vCPU option
keeps the interactive developer path on the maximum supported SMP topology and
therefore requires the same fresh source-bound SMP evidence as any other
multi-vCPU launch. The option builds and signs RustOS in-process,
then the runner verifies the existing signed Linux DVM bundle before QEMU
starts. The runner holds one nonblocking launch lock across build, shared-file
preparation, both QEMU children, and final evidence, and assigns a
process-scoped non-reserved DVM CID to each invocation. A second F5/smoke run
therefore fails before it can truncate logs or alias the prior vsock identity,
while an immediate sequential rerun does not reuse a retiring vhost-vsock CID.
There is no separately mutable pre-launch task. F5 must never run
`build-dvm`; DVM source changes use the explicit build-plan and stable-batch
lanes above. `kvm-run` keeps the interactive session operator-owned after
startup. During SMP qualification the runner records the guest timestamp but
does not terminate on an independent boot-to-UI deadline; `kvm-smoke` uses its
bounded outer process timeout for readiness and proof windows. The runtime
trace it records follows the same rule: a step that lands after its absolute
deadline is printed with the observed and budgeted milliseconds and does not
end the session, because a developer paused at a breakpoint or a cold host
cache is not a product regression. `kvm-smoke` still enforces every one of
those deadlines, so the budgets remain gates wherever the result is evidence.
The ten-second
product target remains a measured release requirement that must be restored as
an enforcing gate after the SMP failure path is stable. The repository Cargo
config does not make optional `sccache`
availability a prerequisite for compiling xtask; developers may opt in with
`RUSTC_WRAPPER=sccache` when their environment supports it. The base
`tools/check-dev-environment.sh` gate checks the one-command launch contract,
rejects a reintroduced split task or `build-dvm`, and rejects any mandatory
repository `rustc-wrapper`.

If a guest exits before debugcon can publish a panic, one diagnostic rerun may
set `RUSTOS_KVM_QEMU_INT_TRACE=1`. The runner then records QEMU interrupt/reset
and KVM system-exit events in `build/kvm/rustos-qemu-int.log`; this opt-in trace
is never enabled in normal F5 or acceptance runs and is not success evidence.

## Tests and inventory

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `tools/check-dev-environment.sh [--ai] [--docs] [--formal] [--physical-gpu] [--release]` | read-only diagnosis of the pinned toolchain and only the optional host capabilities needed by the selected task | none | missing or wrong-version prerequisite; never installs tools or changes host state |
| `cargo xtask selftest` | host selftests for fault parsing, executable-image admission, ABI/layout, and runtime contracts | `target/` | contract/layout regression |
| `cargo xtask fuzz-host --target all` | deterministic host fuzz smoke for fault rules, executable-image admission, project config, package/DVM manifests, and hostd launch-plan/device-policy/control-contract parsing | `logs/` on crash | parser panic or invariant bug |
| `cargo xtask fuzz-host --target image-admission --iterations 1000` | exercise overflow, bounds, overlap, W^X, and entry-point admission without booting a guest | `logs/` on crash | shared ELF/PE admission panic or invariant bug |
| `bash formal/run-tlc.sh <model/name>` | exhaustively run one changed finite TLA+ model with automatic local CPU parallelism | temporary files only | invariant violation, malformed model, or unavailable pinned TLC input |
| `bash formal/run-spec-mutations.sh --id <mutation-id>` | rerun only a previously failed TLA+ mutation and its unchanged baseline during repair; it never overwrites full-corpus evidence | `build/formal/spec-mutations/<id>/` | a surviving mutant, timeout, unrelated invariant failure, or malformed counterexample |
| `bash formal/run-spec-mutations.sh` | kill the whole registered TLA+ mutation corpus; mutants run concurrently over private module/config/artifact copies while each stays a single-worker TLC run, so the invariant that rejects a mutant is unchanged, and any mutant that exhausts its pinned wall budget under load is re-adjudicated alone before it can fail the lane. `RUSTOS_SPEC_MUTATION_JOBS` overrides the default half-CPU share | `build/formal/spec-mutations/` | a surviving mutant, a mutant still timing out with the host to itself, an unrelated invariant failure, or a malformed counterexample |
| `bash formal/run-all-tlc.sh` | admit the 120-second, fail-closed PR set of critical TLA+ models at unchanged finite configurations, reusing only exact-input recent passes that retain the declared five-minute seal reserve and running every miss; `--profile nightly` runs the full registry without reuse | temporary files only | model failure, an invalid/expired/near-expiry cache followed by a failed rerun, a global wall-budget breach, or an unbounded/partial result |
| `bash formal/verify-all.sh --profile pr` | run the registered merge gate: selftest and exhaustive TLC first, then independent mutation, Loom/Shuttle/herd7, Kani/Verus, trace, dual-ABI, and recovery lanes concurrently with every child status collected before sealing; each lane and the sealed gate report `elapsed_seconds`, so the pole lane is readable from the output | `build/formal/`, tool caches | invariant/proof/trace/reference/recovery/mutation failure, missing pinned tool, or any failed parallel lane |
| `bash formal/verify-all.sh --profile nightly` | add alternate TLC seed/simulation, Miri, Apalache, TLAPS, bounded Rust/C coverage-guided fuzzing, and instrumented address/thread profiles | `build/formal/`, tool/corpus caches | nightly bug-finding or proof lane failed; simulation/fuzz success is not proof |
| `bash formal/run-runtime-traces.sh` | generate concrete `runtime-control` source outcomes and replay them against the registered TLA action matrix | `build/formal/runtime-traces/` | source/spec classification drift or malformed trace |
| `bash formal/run-source-conformance.sh` | run the registry-scripted unique exact source decision witnesses for high-risk lifecycle, RPC, and IPC models; rejects duplicate and zero-test entries. The registry is read first and executed second, one `cargo test -- --exact` per package/feature selection rather than per row, so a witness shared by several models is built and run once; each registered name must still print its own passing libtest line and the executed count must equal the exact requested set | `build/formal/source-conformance/summary.json` | source/spec decision drift, duplicate/missing test, a witness that did not execute and pass, or an executed count that does not match the requested witnesses |
| `bash formal/check-system-flows.sh` | validate the machine-readable end-to-end requirement/hazard graph and every model/source/witness link | none | duplicate IDs, missing terminal path, unbounded timeout, absent model/source/test, or direct `.ko` lifecycle route |
| `formal/check-rust-source-contracts.py` | enforce critical/high Rust module contracts, unsafe/ordering documentation debt, dead-code rationale, and the 1300-line split registry | none | undocumented boundary, new source debt, stale ledger, or unregistered oversized file |
| `bash formal/run-apalache.sh` / `bash formal/run-tlaps.sh` | run the typed symbolic-refinement pilots and the unbounded theorem pilot | `build/formal/{apalache,tlaps}/` | type/SMT/proof failure or missing hash-pinned tool |
| `bash formal/run-concurrency-triangle.sh` | run the bounded pre-QEMU Loom proof kernels, Shuttle PCT schedules, and mutation-sensitive pinned herd7 x86_64 litmuses; setup-herdtools.sh must first build the pinned local tool | `build/formal/{loom,shuttle,herd,concurrency-triangle}/` | source/model/flow drift, a concurrency assertion, a schedule/litmus timeout, missing pinned herd7, or a surviving order mutant |
| `bash formal/run-proof-index.sh` | validate and hash the closed Kani/Verus proof-retrieval graph before either proof lane | `build/formal/proof-index/summary.json` | unindexed/stale source or Verus file, missing Kani cover, unknown formal-model anchor, cyclic dependency, or forbidden trusted Verus shortcut |
| `bash formal/run-fuzz-smoke.sh` | run bounded libFuzzer campaigns over shared Rust admission and the exact Linux-DVM C GPU parser with ASan/UBSan | `build/formal/fuzz/`, ignored fuzz target | crash, sanitizer finding, compile failure, or wall-clock bound exceeded |
| `bash formal/run-sanitizers.sh --profile=all` | rebuild and execute registered critical/high host-testable Rust boundaries with the pinned address/thread instrumentation profiles | `build/formal/sanitizers/` | instrumented test failure, unsupported target, or per-target deadline |
| `bash formal/run-abi-differential.sh` | compare compiled RustOS Linux/Windows ABI constants and layouts with native Linux and MinGW/Wine reference probes; permit only exact expiring divergences | `build/formal/abi-differential/` | missing reference tool, ABI drift, stale divergence, or probe failure |
| `bash formal/run-recovery-scenarios.sh` | execute the bounded checkpoint, service-restart, and storage recovery matrix with exact source witnesses | `build/formal/recovery-scenarios/` | missing transition class, zero-test filter, failed terminal state, or deadline |
| `bash formal/run-implementation-mutations.sh --check` | validate every implementation-mutation row without invoking Cargo: a unique anchor uses `N`, an intentionally repeated anchor uses exact `N/M`, and duplicate mutation semantics are rejected | none | stale/ambiguous source anchor, wrong source/package path, duplicate ID, or duplicate mutation semantics |
| `bash formal/run-implementation-mutations.sh [--only <id> ...]` | seal each resolved source offset/context/hash in an isolated live-tree copy, prove each shard's pristine-tree preconditions once per Cargo selection (listing) and once per registered witness (baseline) before any mutant runs, then inject each registered critical/high regression and require that exact witness to kill it | `build/formal/implementation-mutations/` | survived mutant, compile-only or foreign-target rejection, source-seal drift, ambiguous/missing witness, or an exact witness that did not execute |
| `cargo test -p contract-tests` | active DVM transport, user ABI, keyboard, boot-random, and fault-rule layout tests | `target/` | active contract/layout regression |
| `git diff --check` | whitespace sanity | none | trailing whitespace/conflict marker |

Do not rerun `cargo xtask build-dvm` for RustOS-only, documentation, formal,
manifest-consumer, or unrelated service changes. Reuse the verified artifact;
for a local DVM relay source change, use the matching `rebuild-*` target above
and then `cargo xtask verify-dvm`.

## DVM build-speed contract

Run `make -C driver-domains/linux build-plan` before any DVM integration build.
Its output is routing, not artifact evidence. `mode=full-output` is reserved for
a changed Buildroot/toolchain identity or a configuration transition whose
complete `BR2_*` diff cannot be admitted safely. Kernel source/config changes
select `linux+signed-kernel-modules+rootfs`; local relay changes select only
their package plus rootfs; overlay, post-build, and AMD firmware-policy changes
select rootfs only. The wrapper removes stale installed modules before the
kernel lane completes and writes cache stamps only after all release checks
succeed. Configuration-identity stamps are the exception: they are recorded
after successful Kconfig reconciliation so an interrupted build can resume;
they are routing state, not release evidence.

For a source-only edit under one local DVM relay package, `cargo xtask dev-plan`
puts `make -C driver-domains/linux dev-*` in `now` and the matching
`rebuild-*` in `stable-batch`. `dev-*` requires an unchanged, warm Buildroot
configuration and source tree; it refuses to fetch, reconfigure, clean, or
rebuild the host toolchain. It refreshes and compiles exactly one local package
against the cached sysroot, but intentionally does not create a rootfs,
manifest, or signed/release artifact.

After `dev-*`, `make verify`, `cargo xtask verify-dvm`, and every KVM command
fail closed until the matching `rebuild-*` succeeds. This prevents a fast
package result from being mistaken for a release image. Keep `rebuild-*` for
one stable change set, where it regenerates the rootfs and runs the full module
signature and artifact verification. This follows Buildroot's distinction
between package-only rebuilds and integration builds: <https://buildroot.org/downloads/manual/manual.html>.

The integration rebuild still has to regenerate the immutable initramfs. The
repository wrapper publishes schema-9 `.cpio.zst` with deterministic
`zstd -3 -T1`, removes any stale schema-8 XZ image, and hashes the result into
the release manifest. Do not call Buildroot directly: doing so bypasses the
packaging, stale-artifact rejection, and reproducibility contract. The single
compression worker deliberately makes the Zstandard frame independent of host
worker scheduling while retaining fast guest decompression.

An additive defconfig change preserves the cached Buildroot host toolchain only
when every changed symbol is a disabled-to-`y` transition named in
`scripts/additive-package-cache-v1.txt`. That file contains only audited
target-only leaf packages that cannot alter feature detection or linkage of an
already-built package. The wrapper renders a separate desired configuration
and compares the complete `BR2_*` maps before building the new package and
rootfs. Value changes, missing keys, an unlisted package, architecture/toolchain
changes, package removal, or conservative driver source
identity changes force Buildroot's clean-output rebuild. Linux Kconfig/source
and host kernel-build header changes are isolated to the kernel and signed
module lane; AMD firmware lock and post-build changes are rootfs-only. This
matches the
[Buildroot incremental-build warning](https://buildroot.org/downloads/manual/manual.html):
package additions may be incremental only when existing optional consumers do
not need rebuilding; removals require a clean rebuild.

`BR2_CCACHE=y` and `BR2_CCACHE_USE_BASEDIR=y` keep object cache entries in
`driver-domains/linux/out/ccache`, outside the disposable
`buildroot-output` tree but inside the writable managed checkout. The wrapper
exports that directory explicitly so sandboxed builds cannot inherit an
unwritable home cache and fail halfway through LLVM. An operator may override
it only with `RUSTOS_DVM_CCACHE_DIR`. Use `ccache-stats` to measure whether a
repeated cold build is receiving real hits. After a completed build,
Buildroot's official `graph-build` facility may be run through the wrapper
during performance work to attribute duration by package; do not infer build
cost from artifact size.

The profile intentionally leaves `BR2_PER_PACKAGE_DIRECTORIES` disabled.
Buildroot's `.NOTPARALLEL` guard therefore serializes the package graph while
`BR2_JLEVEL=0` retains automatic parallel compilation inside each package.
Do not enable the documented experimental top-level parallel mode as a speed
shortcut; its per-package host/target directory semantics are not admitted by
the current overlay, signing, or verification wrapper.

The stronger cold-build optimization is a separately produced, checksummed
Buildroot SDK consumed as a custom external toolchain. Buildroot explicitly
recommends that backend when internal-toolchain rebuild time is excessive.
RustOS does not switch to it opportunistically: the SDK needs a pinned source
identity, relocation test, ABI/config equivalence check, and release provenance
before it can replace the current internal toolchain.

### Cold DVM integration runbook

Use this only when `build-plan` requires a cold/full integration build or when
producing the final appliance. Do not insert `clean` between these steps.

1. Confirm no DVM build process is live, then run `selftest-config-cache` and
   `build-plan`. Record the complete plan output.
2. Capture cumulative `ccache-stats` if the cached host tool exists. Do not
   reset a shared cache merely to obtain cleaner numbers.
3. Run exactly one `cargo xtask build-dvm`. If it is interrupted or a
   compile error is corrected, rerun that same command against the partial
   output; do not restart from an empty tree.
4. Only after success, run `ccache-stats`, `profile-build`, and
   `cargo xtask verify-dvm`. The timing graphs explain build cost; they do not
   prove runtime correctness.
5. Keep KVM and physical-device tests as separate gates. A successful image
   build never authorizes or proves VFIO binding, DMA-BUF scanout, FPS, reset,
   revoke, or recovery.

The signed DVM is verification-reproducible, not necessarily byte-identical
between independent builds: Linux may generate a new per-build module-signing
key. The release contract is the exact certificate-bound module verification,
locked inputs, normalized packaging, and artifact manifest. Never strip or
otherwise mutate a `.ko` after its signature is attached.

Schema 9 is an exact locked-input and artifact manifest, not a CycloneDX SBOM
or Buildroot `legal-info` bundle. Those standardized release-provenance outputs
require a separate schema/admission change and are not fabricated by this
build. Their absence does not invalidate the functional appliance build, but
the schema-9 manifest must not be described as standardized SBOM evidence.

Settle a physical-DVM kernel envelope before its first integration build. The
AMD display envelope explicitly pins ZONE_DEVICE page ownership, DMA-BUF/sync,
AMD DC/KMS, and the absence of nested VFIO/IOMMUFD, generic DMA heaps, userptr,
and diagnostic DRM providers. The post-build step seals AMDGPU firmware to
`board/amdgpu-firmware-1002-1900.txt`; changes to that rootfs-only profile
first reinstall the cached `linux-firmware` package into the mutable target
tree, then regenerate the image while preserving the host toolchain, kernel,
Mesa, and LLVM. This restoration is required because the previous post-build
pass deliberately pruned every firmware file outside the sealed profile.

`cargo xtask dev-plan` never executes the printed commands. `now` is the
edit-loop set. `stable-batch` is ordered and should run once after the related
source/config set settles. Override TLC parallelism only for diagnosis with
`TLC_WORKERS=<positive integer>`; `TLC_WORKERS=1` is the serial reproducibility
fallback.

## KVM smoke arguments

- `kvm-smoke` requires read/write `/dev/kvm` and `/dev/vhost-vsock` access plus
  `qemu-system-x86_64`; it does not alter host hypervisor configuration.
- Headless virgl smoke discovers exactly one direct, read/write AMDGPU render
  node by sysfs vendor and bound-driver identity, then pins that exact path in
  QEMU's EGL backend. Render-node numbering is not an ABI; zero or multiple
  AMDGPU candidates fail closed. QEMU GTK has no `rendernode` option, so
  `kvm-run` resolves the already-validated node back to its canonical sysfs PCI
  identity and pins Mesa OpenGL selection with exact-address `DRI_PRIME`.
- The Linux DVM disables q35's implicit i8042 controller and exposes one
  explicit virtio keyboard plus one absolute virtio tablet. Virtual-display
  launches register `dvm-virtio-gpu` first and bind both input devices to its
  head 0; this prevents PS/2/virtio source ambiguity and cross-console input.
- Interactive input keeps the fixed L0 ivshmem producer peer alive for the
  entire QEMU session. A bounded DVM-vsock setup timeout retries only the
  authenticated stream. The broker retains each fixed peer's eventfd lease
  across a QEMU process replacement, but never reassigns that logical ID or
  accepts a third peer; shared transport generations, not socket reconnect,
  decide whether old work is admissible.
- `--timeout <seconds>` is bounded to `1..=120` and applies only while waiting
  for expected RustOS debugcon and Linux DVM serial markers.
- `--storage-dvm-only` enables the private block aperture and removes only the
  unrelated GPU-scene/compositor acceptance requirements. It still requires
  RustOS boot/provenance, the authenticated DVM control handshake, both block
  readiness markers, the first completion, exact live geometry, and storaged's
  generation-bound read-only media barrier. This is transport-liveness
  evidence, not backing-image write-durability evidence. It cannot be combined
  with UI, input, network, FPS, or physical-GPU proof options.
- `--storage-dvm-expect-flush-fault` is valid only with
  `--storage-dvm-only` and exactly one active `block.flush=fail` rule. It
  replaces the positive E2E marker with the exact live-generation injection
  marker, retains peer/geometry/first-completion admission, and fails
  immediately if a media-barrier success marker appears.
- The default marker is `rootd: core services ready, spawning initd via loaderd`;
  its kernel-stamped `product-root-core-ready` record is equivalent. Because
  the bounded observability channel may drop one contended record, the strictly
  later kernel-stamped `product-init-identity-ready` milestone also proves this
  gate: rootd cannot spawn initd before the core-ready transition. Repeat
  `--expect <marker>` for each additional RustOS milestone.
- `--dry-run` verifies DVM artifacts and prepares `build/kvm/` without
  launching QEMU. It creates missing log files but never truncates evidence
  from the preceding real run; only a new real launch rotates the active log
  set.
- Physical-GPU smoke readiness treats any compositor `offline` record as a
  terminal failure and requires four consecutive zero-copy frames with GPU and
  present fences. Four frames traverse the three-slot pool and prove one slot
  reuse; this is a lifecycle smoke gate, not sustained-FPS evidence.
- The DVM's `agent-v1-control` contract makes a host-authenticated KVM-vsock
  health, PCI-inventory, driver-inventory, and `input-stream` handshake. L0 validates keyboard
  and relative-pointer evdev records before forwarding sequenced, checksummed
  RDI3 frames into an L0-owned 128 KiB fixed ring, then signals RustOS's one
  MSI-X eventfd; no QMP socket is launched and the DVM never maps the ring.
  L0 releases tracked keys/buttons when the DVM stream ends. The smoke
  establishes the relay but does not synthesize input, so a live event still
  needs a real input source assigned to the DVM. It is not storage or
  PCI-passthrough validation.
- `--gui-dvm-surfaces` adds the private launch-owned production
  `ivshmem-doorbell` topology to both KVM guests. Both its writable control BAR
  and separate pixel backing live in the same owner-only tmpfs directory so a
  physical VFIO device can pin every guest RAM section through IOMMUFD. Its broker accepts exactly
  two same-UID QEMU peers and two fixed reverse-vector meanings, then passes
  only the host-created control records and eventfds. A separate 32 MiB
  cacheable pixel pool is writable in RustOS QEMU and read-only/ROM in the DVM.
  RustOS copies a complete immutable 1600×900 BGRA snapshot, fences the slot,
  then rings the DVM; the Linux relay reconstructs a
  pre-load invitation through its validating UIO module, returns a readiness
  acknowledgement bound to that exact generation, and permits one validated
  RELEASE until the host ACK. The second reverse vector revokes availability;
  a replacement relay must revoke any inherited confirmation, RustOS increments
  the context epoch, and the newest complete READY slot is re-invited even when
  the desktop is idle. The module exports each page-aligned slot as a DMA-BUF whose device
  mapping is read-only. The module retains a DMA-BUF exporter, but the current
  GPU-command relay does not import those GUI surface slots: the former branch
  was unreachable under the mandatory nonzero V3 atlas header and has been
  retired. QEMU validates staged atlas upload plus fixed GLES composition and
  must report `source-path=staged-copy zero-copy=0`. Physical AMD read-only
  atlas import, GPU composition, and atomic scanout now have an explicit
  authenticated physical-AMD relay mode, but remain failed hardware gates
  until captured on the assigned GPU; this KVM command must not bind a physical
  GPU or claim their evidence. V2, polling, synchronous `DirtyFB`, a CPU-frame
  renderer, and a native-GPU fallback are rejected. The NVIDIA
  package admits only the exact 580.173.02 open-module/GSP pair and excludes
  UVM/CUDA; redistribution authorization remains a separate release gate.
  This KVM command does not imply physical GPU passthrough; the separate signed
  `rustos-hostd supervise` lifecycle owns that evidence.
  This KVM validation profile explicitly disables guest x2APIC because the
  current RustOS MSI-X receiver requires an xAPIC destination until a complete
  interrupt-remapping substrate exists; the kernel fails closed on x2APIC.
- `--exercise-input` is the explicit exception for bounded integration tests.
  It adds a DVM kernel command-line flag; the DVM agent then creates a local
  `uinput` device and consumes it through its ordinary evdev relay. RustOS
  must log both ring-3 `inputd` keyboard and pointer ingress markers. It emits
  no printable key or click: one F12 proof is followed by pointer-only motion,
  tracing a 192-pixel square, so the test cannot type into a focused shell or
  masquerade as a trembling cursor. It neither enables QMP nor a host-to-DVM
  input endpoint, and normal DVM boots do not run this self-test.
- `--dvm-network-shmem` adds a private 512 KiB fixed-ring `ivshmem-plain`
  aperture to both guests. RustOS owns only bounded Ethernet-frame ring access;
  Linux owns the virtio-net NIC and raw socket relay; `netd` retains socket/TCP
  policy. RustOS has no native virtio-net device in this topology.
- `--exercise-network` requires both `--gui-dvm-surfaces` and
  `--dvm-network-shmem`; the GUI provider is required because runtimed admits
  the app catalog only after UI readiness. The option changes only the private
  KVM disk copy so the existing `netprobe` reaches the QEMU gateway.
  Passing requires the normal app result plus nonzero producer and consumer
  counters in both bounded rings. It is an Ethernet transport proof, not a
  physical NIC assignment or an L0 network control plane.
- `--min-ui-fps <fps>` enables both `RUSTOS_UI_PROFILE` and
  `RUSTOS_WAYCLICK_PROFILE` only in the private KVM disk copy by replacing the
  equal-length disabled values. It also attaches the DVM block aperture because
  the profiled apps are admitted from the mutable DVM-backed volume; an
  embedded-volume or retry-only success is forbidden. It never alters the
  release boot image. The
  proof requires the requested number of consecutive one-second windows for
  uiserver render/input health, balanced WayClick commit/frame-callback/
  buffer-release progress whose exact aggregate reaches the requested rate,
  whose individual windows remain at or above 80% of that rate, and whose
  callback gap is at most 50 ms. When enabled, it also requires DVM runtime plus
  atomic-page-flip relay throughput. One subsystem passing
  cannot mask another subsystem's failure. Timeout diagnostics include the
  observed WayClick rate range, callback gap, and redraw maximum before the
  focused log paths. The range includes non-one-second startup windows; compare
  their elapsed time with later one-second windows before attributing a stall.
- `--rustos-vcpus <1..=8>` selects the RustOS guest topology for SMP
  qualification. The launcher rejects counts above one unless every compiled
  scheduler, syscall, CPU-online, reschedule-IPI, TLB, robust-futex, and
  per-CPU-clockevent prerequisite is admitted. This option selects a test
  topology; it is not release evidence without the matching bounded run.
- `cargo xtask kvm-run` seals its own profile. A multicore interactive launch
  whose `smp-iteration` evidence is missing, expired, or bound to an older tree
  runs `bash formal/verify-smp-iteration.sh` and revalidates before claiming a
  layout, doorbell, or relay. Admission still comes only from that second
  validation, so a failed verification refuses the launch; `--no-auto-verify`
  restores the plain refusal. `kvm-smoke` is unchanged and still requires the
  seal up front.
- Iterative SMP debugging uses `bash formal/verify-smp-iteration.sh` followed
  by `cargo xtask kvm-smoke --timeout 30 --rustos-vcpus <1|2|4|8>
  --smp-iteration --smp-ring3-qualification --smp-evidence-cohort <32hex>`.
  The exact-tree seal covers source conformance and the
  bounded high-risk SMP model set. `--smp-iteration` rejects runs longer than
  30 seconds and cannot be combined with FPS, recovery, or physical-GPU
  acceptance. Its TLC sub-lane reuses only exact-input recent passes and runs
  every changed or invalid model. Remove it and use the full PR seal for any
  release claim.
- A full-minute active proof uses
  `cargo xtask kvm-smoke --timeout 90 --gui-dvm-surfaces --min-ui-fps 55
  --ui-proof-windows 60`. The 90-second host deadline includes boot and
  readiness headroom; acceptance still requires 60 consecutive one-second
  uiserver, WayClick, input, and DVM-relay samples. This longer host bound does
  not widen any guest service deadline.

## L0 VFIO lifecycle

- `rustos-hostd acquire --plan <file>` is dry-run by default. Production
  activation requires the detached release signature, pinned keyring, exact
  artifact manifest, schema-3 AMD physical-display device policy, and fleet policy. Unsigned device
  binding is unavailable. Before writing a prepared lease or changing any
  driver binding, activation also runs the same physical runtime preflight
  against `--qemu`; an absent `/dev/iommu` therefore cannot detach the GPU.
- `supervise` accepts only an already-active, signed display-only lease and a
  policy whose QEMU digest matches the root-owned executable. It uses one
  non-identity IOMMUFD VFIO address space, a durable pre-exec runtime identity,
  authenticated readiness, five fresh consecutive physical page-flip evidence
  samples meeting signed throughput/latency bounds, bounded process teardown,
  and group reset before launch and before restore.
- Never assign the host boot disk, active host display, or a mixed/protected
  IOMMU group. `recover` is the crash path; `release --activate` is only for a
  prepared lease or a known non-running active lease. Both retain the durable
  lease on any reset/restore failure.

## Do not run

- destructive git commands unless explicitly requested.
- formatters that rewrite files unless the task is implementation, not
  planning/review.

## Docs verification

- `.codex/hooks/selftest.sh` for the versioned agent/hook policy bundle.
- `mdbook build` if `mdbook` exists.
- Inspect markdown links with pattern `\[[^]]+\]\(([^)#]+\.md)`.
- Top-level human docs should include `[English](#english) | [한국어](#korean)`.

## Fast context commands

- Prefer symbol-aware search (Serena MCP) for symbols and scoped text search
  (ripgrep MCP or `rg`) for raw `symbol_or_path` matches under `kernel`,
  `services`, `tools`, `libs`, `drivers`, and `apps`.
- `find kernel -maxdepth 4 -name api.rs | sort`
- `find . -name RUSTOS.package.toml | sort`
- Search for `enum XtaskCommand|struct Config|enum PackageKind` under
  `tools/xtask/src`.
- Read `START..END` only after search finds the relevant line range.
- Prefer scoped file-listing search (`rg --files`) over recursive `ls` or
  broad `find`.

## GRUB Secure Boot debug environment

- `cargo xtask build` creates a local dev GRUB signing key under
  `build/dev-grub-gpg` when `RUSTOS_GRUB_*` is unset.
- `grub-file --is-x86-multiboot2 build/image/nucleus.elf`
- `gpg --homedir build/dev-grub-gpg --verify build/image/nucleus.elf.sig build/image/nucleus.elf`

## KVM display boot loop

1. `cargo xtask build`
2. `cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'`
3. Search the relevant log for
   `error: no suitable video mode|boot framebuffer|virtio-gpu|virtio register|DisplayUnavailable|uiserver|panic|scheduler invalid`.

## Generated path exceptions

See `token-policy.md` §10 for the canonical list. Summary: `logs/` only for
run/debug failures, `build/image/system/registry/` only for stage/registry
verification, `vendor/` only for firmware/module packaging.
