# RustOS formal models

This directory contains small, executable TLA+ models for RustOS contracts
whose correctness depends on interleavings. They are design and regression
checks; they do not generate kernel code or replace Rust tests, fuzzing, ABI
checks, or KVM smoke tests.

The modeled Rust contracts and their remaining abstraction limits are recorded
in [CONFORMANCE.md](CONFORMANCE.md). Update that audit whenever a mapped
source transition or cleanup owner changes.

[REVIEW.md](REVIEW.md) records the model-quality review: it distinguishes an
exhaustive TLC result from the separate checks for clock horizon, freshness,
terminal cleanup, and named liveness assumptions.

## Registered gates and evidence

`models.tsv` is the sole executable-model registry. `selftest.sh` rejects an
unregistered primary TLA source or TLA/CFG pair, orphaned config, missing invariant/property,
unexplained terminal deadlock policy, missing conformance mapping, or a pilot
flag without its corresponding executable artifact. A `temporal` row must use
`SPECIFICATION Spec`; configuring `INIT`/`NEXT` directly would silently bypass
the fairness assumptions written into `Spec`. Do not add a model only to
`run-all-tlc.sh`; that list is generated from the registry.
`system-flows.tsv` is the cross-model lifecycle registry. Its checker binds
stable requirement/hazard IDs to owner transitions, finite waits, one formal
model, one source anchor, and one exact source witness before TLC runs.
`product-scenarios.tsv` composes those local transitions into exact KVM
topologies with explicit prerequisites and absolute deadlines. Sequence is a
stable topological identifier, not a claim that independent branches finish in
that wall-clock order. The `product-boot/ProductBoot` model runs display and
storage admission in parallel after input policy, joins both before the first
presented frame, and requires an exact executable image to be sealed and
activated. The storage-only branch terminates only after its generation-bound
read-only media-barrier request traverses
`vfs-policy->block-broker->shared-ring->linux-dvm->media-barrier`. That terminal
is liveness evidence for the admitted data plane, not backing-image write
durability.
`fault-scenarios.tsv` is the closed fault-point/evidence registry; its checker
rejects phantom and duplicate rules and requires every critical/high point to
bind a live production `source` independently to one executable
`witness_source`; test-module extraction cannot silently replace the production
boundary being proved. The durability fault also requires the bounded
storage-DVM negative KVM gate. `recovery-scenarios.tsv` uses the same explicit
production/witness split and hashes both inputs into its evidence. The
recovery runner compiles each unique Cargo package/feature/target test binary
once using Cargo's JSON artifact paths; the registry's `max_ms` applies to
executing the witness binaries, **not** waiting on Cargo compilation or another
parallel formal lane's build lock. Missing test artifacts, empty test filters,
execution timeouts, and nonzero test exits remain terminal failures. The
artifact-path binding follows Cargo's [JSON compiler-artifact format](https://doc.rust-lang.org/cargo/reference/external-tools.html#json-messages),
which reports each built test executable; it is not a guess at `target/deps`.
`abi-divergences.tsv`, `recovery-scenarios.tsv`,
`spec-mutations.toml`, `implementation-mutations.tsv`,
`sanitizer-targets.tsv`, and `concurrency-triangle.toml` close six
independent source-evidence gaps: native dual-ABI drift, bounded
restart/crash-consistency outcomes, test sensitivity to real implementation
regressions, TLA+ property/transition mutation survivors, and instrumented
host-testable critical/high boundaries. Their runners reject stale exceptions,
missing transition classes, zero-test filters, parser-only or compile-only
mutant failures, and unbounded execution. See
SPEC_MUTATION_CONTRACT.md for the one-mutant, named-invariant,
counterexample-trace protocol.
`CONCURRENCY_TRIANGLE_CONTRACT.md` defines the complementary bounded Loom,
Shuttle PCT, and x86_64 herd7 lanes. It requires a source/model/flow binding
and rejects both TLA+ and litmus survivors, so a green checker cannot be
credited for an empty property or a non-sensitive order assertion.
The scheduler local-frame entry runs Kani on the production custody predicate
in `rustos-scheduler-frame-gate`, and source-anchored Loom/Shuttle tests on a
single reader racing a generation change and owner claim. The separately
registered `scheduler-local-authority` TLA+ model checks two-candidate fair
selection, fail-closed partial observations, excluded policy arms, and one
owner-generation republish before the exact claim. These do not prove the full
local picker, multiple writers,
or catalog equivalence.
`proof-index.toml` is a closed proof-retrieval graph for the selected Kani and
Verus kernels. It binds an executable source symbol to a registered TLA+ model,
the exact harness or lemma, its companion test, and any dependency edge. The
index checker rejects a Kani proof without its own `kani::cover!`, unregistered
Verus proof files, stale source/model links, dependency cycles, and Verus
`admit`/`assume`/axiom/external shortcuts. `run-proof-index.sh` records hashes
of the index and all indexed inputs, which the PR seal consumes. It is not an
LLM, proof generator, or claim that the source has been fully verified; see
`PROOF_INDEX_CONTRACT.md` for the precise boundaries.
The PR TLC profile is deliberately a 120-second, risk-weighted pre-QEMU set:
it retains the exact finite configurations of the critical ownership, CPU,
TLB, wake, ABI, and product-boot models selected by `run-all-tlc.sh` from
`contracts.toml`, rather than quietly reducing their state depth. The complete
registered inventory is the nightly qualification lane. `dev-plan` still selects
every impacted model before either profile: it reuses only a recent exact-input
individual proof or runs that model to completion. A PR pass is not a claim that
unrelated nightly models were explored.
The `tlc_max_wall_seconds` profile budget fails closed rather than accepting a
partially explored model.
An exact PR or SMP-iteration pass may be reused for at most 24 hours. These
two profiles reciprocally share a completed TLC result only for a model selected
by both, because their exhaustive worker/fingerprint/seed policy is identical;
the non-TLC PR lanes and release seal remain distinct. `tlc_cache.py` rejects it
unless the TLA module, CFG, pinned TLC version and digest, deadlock policy,
worker policy, fingerprint, seed, positive exploration metrics, and model name
still match. A source or policy change therefore reruns only the affected
model. The SMP iteration profile applies the same rule to its smaller model
set; nightly qualification never reuses TLC evidence. For explicitly selected
`dev-plan` models outside the curated PR set, `tlc_cache.py --individual` may
reuse the same PR execution policy, but cannot widen or satisfy a complete
profile gate. This requires a new before/after input fingerprint from the actual
TLC runner, including its validator, pinned tool lock, TLA and CFG. Only closed
standard-module imports qualify; local shadows, unbound imports, mutation and
execution overrides force a fresh run. Legacy summaries without this binding
cannot satisfy individual reuse. The runner refuses a passing result if the
checked TLA/CFG or a bound closed-proof input changes during checking. This is proof-evidence
reuse, not TLC state recovery, and no artifact is touched to fabricate
freshness. TLC's `-depth` option controls random simulation rather than the
depth of exhaustive model checking, so the PR lane does not use it to truncate
the state graph; see the official [TLC tool options](https://github.com/tlaplus/tlaplus/blob/master/general/docs/current-tools.md).
`verify-all.sh` emits a profile verification-run seal only after every selected
gate succeeds. That seal hashes the complete source tree and every normalized
gate/TLC artifact; commercial evidence rejects a stale, partial, or mixed-source
run. KVM product traces separately bind the exact source tree, RustOS boot
image, and verified DVM manifest used for the observed run. An optional stale
KVM trace is classified as stale and excluded from the formal seal; a required
KVM trace still fails until a fresh run replaces it.
`proof-assumptions.tsv` explicitly lists the assembly, hardware, boot, DMA,
toolchain, external-kernel, observability, hypervisor, physical-hardware, and
side-channel assumptions below those proofs. `verified-configurations.tsv`
limits each evidence claim to one exact platform/topology. The proof-boundary
checker rejects missing assumption classes, unknown references, unsealed
profiles, and any attempt to inherit QEMU evidence into physical hardware.
`check-performance-contracts.sh` is the source-drift gate for the shared boot,
frame, and typed IPC limits. During SMP qualification it also rejects restoring
an independent guest boot deadline before the runtime failure path is stable.
It rejects unclassified compat service calls,
service-registration retry amplification, a stable endpoint lookup that takes
the global writer lock, synchronous policy IPC in frame/present code, unbounded
foreground VFS recovery, and unbounded outer KVM execution.
`check-rust-source-contracts.py` binds every critical/high Rust source in
`contracts.toml` to a leading owner/boundary/lifecycle/concurrency/failure
contract. It prevents undocumented unsafe/ordering debt, warns (never fails)
about files over 2000 lines, and rejects unresolved
production markers or unexplained `dead_code` allowances.
`check-smp-source-assumptions.py` separately rejects mutable kernel statics,
raw APIC-ID indexing, CPU-zero identity fallback, undocumented unsafe
`Send`/`Sync`, stale BSP-only runtime contracts, and scalar regressions of the
registry-declared per-CPU authority set. Its normalized result is required by
the PR, nightly, and bounded SMP-iteration evidence profiles.

### Revoke and qualification evidence

`dvm-block-transport-revoked` is a Required milestone whose `arg0` is one of
the twelve production revoke reasons and whose `arg1` is the installed
generation. The terminal guard captures flags plus all four ring cursors once,
before clearing pending slots or readiness; a valid FLUSH completion must still
admit the first 64 KiB READ. Only the exact
`smp-qualification-{ready,start,finish,complete}` names are
QualificationCritical. Their rendered frames carry qualification-local loss
counters, a critical drop increments both global and local counters, scheduler
measurements make one attempt, and xtask rejects nonzero qualification loss.

## Run the PR suite

Java 11 or later plus curl and sha256sum are required. The runner fetches the
TLC jar named in [tla2tools.lock](tla2tools.lock), verifies its SHA-256, and
stores it outside the worktree. TLC state files also stay in a temporary
directory.

    bash formal/run-all-tlc.sh --profile pr

The full formal gate also runs the Rust implementation proofs:

    bash formal/setup-kani.sh   # once per pinned Kani version
    bash formal/setup-verus.sh  # once per pinned Verus release
    bash formal/setup-herdtools.sh # once, with documented OCaml prerequisites
    bash formal/verify-all.sh --profile pr

The isolated Loom workspace optimizes its dev/test profile at level 2 while
explicitly retaining debug assertions and integer overflow checks. This follows
[Loom's optimized-test guidance](https://docs.rs/loom/0.7.2/loom/), without the
unchecked defaults of `--release`; [Cargo test inherits the dev profile](https://doc.rust-lang.org/cargo/reference/profiles.html#test).
Exploration bounds, seeds, workers, test selection and evidence/cache policy are
unchanged. Warm matched four-CPU runs of the activation-claim witness (three
samples each) improved median wall time from 36.52s to 11.12s. First compilation
and complete-gate timings have separate cache/lock costs, not this speed ratio.

The Kani lane retains successful package logs in
`build/formal/kani-cache-v1/`. A cache key covers the pinned Kani version,
proof policy/scripts, workspace lock/configuration, and every non-ignored file
in the selected package's complete local Cargo dependency closure. A hit is
still rescanned for failed or uncovered properties and a new normalized
summary is emitted for the current verification run. Consequently an
unrelated model or documentation edit does not repeat four unchanged proof
builds, while a source, dependency, harness, tool, or proof-policy change
cannot reuse the old result.

For an iterative multi-vCPU debugging boot, use the bounded exact-tree SMP
profile instead:

    bash formal/verify-smp-iteration.sh
    cargo xtask kvm-smoke --timeout 30 --rustos-vcpus 2 --smp-iteration

This profile is not release evidence. It covers source conformance and the
registry-declared high-risk SMP model subset, caps each model at 30 seconds,
and is mechanically rejected by FPS, recovery, and physical-GPU gates.

The scheduled/manual nightly tier changes TLC fingerprint and seed, retains
single-worker reproducibility, adds fixed-seed long-trace simulation only for
registry-selected models, and runs Miri, Apalache, TLAPS, and bounded
Rust/C libFuzzer campaigns:

    bash formal/verify-all.sh --profile nightly

Set up its pinned optional tools with `setup-miri.sh`, `setup-fuzz.sh`,
`setup-apalache.sh`, and `setup-tlaps.sh`. Tool archives are hash checked and
installed below user caches; no setup script changes host packages.

`PROOF-INFRA.md` records the evidence boundary and the rule for accepting a
counterexample as an implementation bug. Do not treat a solver limitation or
an unmapped model trace as a source defect.

Run an individual model with:

    bash formal/run-tlc.sh endpoint-registry/EndpointRegistry

The runner uses TLC's automatic local worker count and fixed PR fingerprint and
seed. Worker scheduling can change exploration order; use `TLC_WORKERS=1` for
a serial reproduction. Deadlock handling is per-model in `models.tsv`:
`check` requires a deadlock-free state graph, while `intentional-terminal`
must name why a finite protocol is allowed to stop and passes TLC's `-deadlock`
flag (which disables deadlock reporting). TLC expression coverage is retained;
in TLC 1.7.4 a `0:N` action was evaluated but produced no new state, so only an
evaluation count of zero is a coverage failure. Logs, normalized summaries,
and counterexamples are retained under `build/formal/`.

## Models and required properties

The per-model catalog with required invariants and properties lives in
[models.md](models.md).

## Shared-RAM cache and AP PAT bindings

`dvm-block-startup/DvmBlockStartup` binds the block aperture to an exact
prefetchable non-I/O shared-RAM BAR and WB mappings on RustOS and Linux; WC is
not a valid atomic-ring alias. `dvm-input-ring/DvmInputRing` binds its same
BAR class to the host's ordinary `MAP_SHARED` producer and RustOS WB consumer.
`dvm-network-ring/DvmNetworkRing` separately permits RustOS installation before
Linux starts, while requiring a prefetchable RustOS-WB atomic contract at
install and a Linux-WB atomic contract before any relay data-plane activity.
The global MMIO registry rejects mixed-cache physical overlap before either a
direct-map or high-window mapping can publish, including an interval straddling
the direct-map limit; it reserves registry and direct-map metadata before the
first mapping/PTE mutation. `cpu-online-lifecycle/CpuOnlineLifecycle` requires
the BSP to seal an exact MTRR/PAT/cache baseline before SIPI. Every reset AP
clears NW while retaining CD, WBINVDs before mailbox/paging, restores the
sealed baseline in no-fill mode, flushes TLB/cache state, and only enables
caching after exact MTRR/PAT readback before `OnlineParked` or private-ready.
Display pixel memory remains a separate write-mostly WC contract and is not
admitted as atomic shared control memory.

Permanent early-boot APIC MMIO is a separate direct-map-leaf UC retype before
APIC/PIC use; generic high-window maps reject below-limit physical addresses.
Raw direct-map cache inspection/update is whitelisted to kernel-mm, its paging
facade, and io-manager's ownership transaction. Display pixels remain on the
io-manager WC mapper and are not admitted as atomic shared RAM.

| lifecycle-execution-claim/LifecycleExecutionClaim | `kernel-ps` identity writer, claim/resource join and execution banks | Desired lifecycle intent linearizes with exact Running CAS; captured UID survives stop and hidden metadata through Prepared/Current/Outgoing. Twelve lifecycle and three admitted-ACK native witnesses; excludes weak memory, full hardware/funding/affinity and directory deletion |

| cold-exec-intent/ColdExecIntent | `kernel-ps` cold exec intent, process receipts and owned image coordinator | Exact nonce/UID custody through Sealed/Cancelling/Image/Published; only typed cancellation/finalization opens, terminal and poison stay closed. Self remains schedulable until bounded image scope. No hardware/full cold-directory equivalence claim |

| affinity-claim-admission/AffinityClaimAdmission | exact affinity UID/mask replacement versus Local/Direct Running CAS | Affinity54 leaf and empty reset; admitted prior holders survive desired mask changes. No group-wide/hardware/guard-deletion claim |
